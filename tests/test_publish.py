#!/usr/bin/python
# Copyright (C) 2026 Jelmer Vernooij <jelmer@jelmer.uk>
#
# This program is free software; you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation; either version 2 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program; if not, write to the Free Software
# Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301 USA

import asyncio
import inspect
from datetime import timedelta
from typing import cast

import pytest
from aiojobs.aiohttp import get_scheduler_from_app
from breezy.forge import Forge

import janitor.publish as publish
from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.publish import create_app
from janitor.runner import store_change_set, store_run
from janitor.vcs import RemoteGitVcsManager


async def create_client(
    aiohttp_client, db, config_text="", vcs_managers=None, publish_worker=None
):
    config = read_config_string(config_text)
    app = await create_app(
        vcs_managers=vcs_managers or {},
        db=db,
        redis=None,
        config=config,
        publish_worker=publish_worker,
    )
    return await aiohttp_client(app)


async def test_policy_get_not_found(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/policy/does-not-exist")
    assert resp.status == 404


async def test_policy_get(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.put(
        "/policy/lintian-fixes",
        json={
            "rate_limit_bucket": "default",
            "per_branch": {
                "main": {"mode": "propose", "max_frequency_days": 7},
            },
        },
    )
    assert resp.status == 200

    resp = await client.get("/policy/lintian-fixes")
    assert resp.status == 200
    assert await resp.json() == {
        "rate_limit_bucket": "default",
        "per_branch": {
            "main": {"mode": "propose", "max_frequency_days": 7},
        },
    }


async def test_credentials_missing_ssh_dir_returns_no_keys(aiohttp_client, monkeypatch):
    from aiohttp import web

    app = web.Application()
    app.router.add_routes(publish.routes)
    app["gpg"] = type("FakeGpg", (), {"keylist": lambda self, secret=False: []})()

    monkeypatch.setattr(publish, "forges", {})
    monkeypatch.setattr(
        publish.os.path, "expanduser", lambda p: "/nonexistent-ssh-dir-for-test"
    )

    client = await aiohttp_client(app)
    resp = await client.get("/credentials")
    assert resp.status == 200
    body = await resp.json()
    assert body["ssh_keys"] == []


class _FakeVcsManager:
    def get_branch_url(self, codebase, branch_name):
        return f"https://example.com/{codebase}/{branch_name}"


async def test_publish_one_sends_revision_id_and_invokes_compiled_binary(monkeypatch):
    captured = {}

    async def fake_run_worker_process(args, request, **kwargs):
        captured["args"] = args
        captured["request"] = request
        return 1, {"code": "some-failure", "description": "boom"}

    monkeypatch.setattr(publish, "run_worker_process", fake_run_worker_process)

    worker = publish.PublishWorker()

    with pytest.raises(publish.PublishFailure):
        await worker.publish_one(
            campaign="lintian-fixes",
            codebase="mypkg",
            command="lintian-brush",
            target_branch_url="https://example.com/mypkg",
            mode="propose",
            role="main",
            revision=b"somerevid",
            log_id="log-1",
            unchanged_id=None,
            derived_branch_name="lintian-fixes",
            rate_limit_bucket=None,
            vcs_manager=_FakeVcsManager(),
        )

    assert captured["args"] == ["janitor-publish-one"]
    assert captured["request"]["revision_id"] == "somerevid"
    assert "revision" not in captured["request"]


async def test_publish_one_passes_template_env_path_to_compiled_binary(monkeypatch):
    captured = {}

    async def fake_run_worker_process(args, request, **kwargs):
        captured["args"] = args
        return 1, {"code": "some-failure", "description": "boom"}

    monkeypatch.setattr(publish, "run_worker_process", fake_run_worker_process)

    worker = publish.PublishWorker(template_env_path="/etc/janitor/templates")

    with pytest.raises(publish.PublishFailure):
        await worker.publish_one(
            campaign="lintian-fixes",
            codebase="mypkg",
            command="lintian-brush",
            target_branch_url="https://example.com/mypkg",
            mode="propose",
            role="main",
            revision=b"somerevid",
            log_id="log-1",
            unchanged_id=None,
            derived_branch_name="lintian-fixes",
            rate_limit_bucket=None,
            vcs_manager=_FakeVcsManager(),
        )

    assert captured["args"] == [
        "janitor-publish-one",
        "--template-env-path=/etc/janitor/templates",
    ]


class _StubForge:
    """Stand-in for a forge instance.

    The scan only uses the forge as a dictionary key and in log messages.
    """

    def __repr__(self):
        return "<StubForge>"


class _StubMergeProposal:
    """A proposal that records the scan reaching it, then gives up.

    Reading a proposal goes out to the forge over the network, so this part
    has to be a stand-in. It raises ForgeLoginRequired, which the scan already
    handles by moving on, so the only thing left to differ between the two
    tests below is whether the rate limit gate let the scan get here at all.
    """

    url = "https://example.com/mypkg/merge_requests/1"

    def __init__(self):
        self.read_attempts = 0

    def get_source_revision(self):
        self.read_attempts += 1
        raise publish.ForgeLoginRequired(self.url)


def _one_proposal(monkeypatch, forge, mp):
    # Only the forge iteration is replaced, since a real one would need a
    # forge to talk to.
    monkeypatch.setattr(
        publish, "iter_all_mps", lambda statuses=None: iter([(forge, mp, "open")])
    )


async def test_check_existing_skips_a_forge_still_inside_its_backoff(
    con, monkeypatch
) -> None:
    forge = cast(Forge, _StubForge())
    mp = _StubMergeProposal()
    _one_proposal(monkeypatch, forge, mp)

    forge_rate_limiter = {forge: utcnow() + timedelta(minutes=30)}

    await publish.check_existing(
        conn=con,
        redis=None,
        config=None,
        publish_worker=None,
        bucket_rate_limiter=publish.NonRateLimiter(),
        forge_rate_limiter=forge_rate_limiter,
        vcs_managers=None,
    )

    assert mp.read_attempts == 0
    assert forge in forge_rate_limiter


async def test_check_existing_drops_a_forge_backoff_that_has_expired(
    con, monkeypatch
) -> None:
    forge = cast(Forge, _StubForge())
    mp = _StubMergeProposal()
    _one_proposal(monkeypatch, forge, mp)

    forge_rate_limiter = {forge: utcnow() - timedelta(minutes=30)}

    await publish.check_existing(
        conn=con,
        redis=None,
        config=None,
        publish_worker=None,
        bucket_rate_limiter=publish.NonRateLimiter(),
        forge_rate_limiter=forge_rate_limiter,
        vcs_managers=None,
    )

    assert mp.read_attempts == 1
    assert forge not in forge_rate_limiter


class _UnreadableMergeProposal(_StubMergeProposal):
    """A proposal whose forge answers the read with an unexpected status."""

    def get_source_revision(self):
        self.read_attempts += 1
        raise publish.UnexpectedHttpStatus(self.url, 502)


async def test_check_existing_keeps_going_after_an_unexpected_status(
    con, monkeypatch
) -> None:
    forge = cast(Forge, _StubForge())
    mp = _UnreadableMergeProposal()
    _one_proposal(monkeypatch, forge, mp)

    await publish.check_existing(
        conn=con,
        redis=None,
        config=None,
        publish_worker=None,
        bucket_rate_limiter=publish.NonRateLimiter(),
        forge_rate_limiter={},
        vcs_managers=None,
    )

    assert mp.read_attempts == 1


CAMPAIGN_CONFIG = """\
campaign {
    name: "lintian-fixes"
}
"""


async def _insert_codebase(conn, name):
    await conn.execute(
        "INSERT INTO codebase (name, branch_url, url) VALUES ($1, $2, $2)",
        name,
        f"https://example.com/{name}.git",
    )


async def _insert_bucket_only_policy(conn):
    await conn.execute(
        "INSERT INTO named_publish_policy (name, rate_limit_bucket) "
        "VALUES ('bucketonly', 'bucket1')"
    )


async def _insert_run(conn, *, run_id, codebase, campaign, result_branches):
    await store_change_set(conn, run_id, campaign=campaign)
    now = utcnow()
    await store_run(
        conn,
        run_id=run_id,
        codebase=codebase,
        campaign=campaign,
        vcs_type="git",
        subpath="",
        start_time=now - timedelta(minutes=5),
        finish_time=now,
        command="true",
        result_code="success",
        codemod_result={},
        main_branch_revision=b"base-revid",
        revision=b"revid",
        description=None,
        context=None,
        instigated_context=None,
        logfilenames=[],
        value=1,
        change_set=run_id,
        worker_name=None,
        branch_url=f"https://example.com/{codebase}.git",
        result_branches=result_branches,
    )


class _BusyPublishWorker(publish.PublishWorker):
    """Records what it is asked to publish, then reports the branch as busy."""

    def __init__(self):
        super().__init__()
        self.requests = []

    async def publish_one(self, **kwargs):
        # a call the real publish_one would not accept fails here too
        inspect.signature(publish.PublishWorker.publish_one).bind(self, **kwargs)
        self.requests.append(kwargs)
        raise publish.BranchBusy(kwargs["target_branch_url"])


async def _create_publish_client(aiohttp_client, db, worker):
    return await create_client(
        aiohttp_client,
        db,
        CAMPAIGN_CONFIG,
        vcs_managers={"git": RemoteGitVcsManager("https://example.com/git/")},
        publish_worker=worker,
    )


def _active_publish_jobs(client):
    scheduler = get_scheduler_from_app(client.app)
    assert scheduler is not None
    return scheduler.active_count


async def _wait_for_publish_jobs(client):
    for _ in range(500):
        if _active_publish_jobs(client) == 0:
            return
        await asyncio.sleep(0.01)
    raise AssertionError("publish jobs did not finish")


async def test_policy_get_with_a_null_per_branch_policy(aiohttp_client, db):
    async with db.acquire() as conn:
        await _insert_bucket_only_policy(conn)

    client = await create_client(aiohttp_client, db)

    resp = await client.get("/policy/bucketonly")
    assert resp.status == 200
    assert await resp.json() == {"rate_limit_bucket": "bucket1", "per_branch": {}}

    resp = await client.get("/policy")
    assert resp.status == 200
    assert await resp.json() == {
        "bucketonly": {"rate_limit_bucket": "bucket1", "per_branch": {}}
    }


async def test_get_publish_policy_returns_empty_policy_for_null_per_branch_policy(con):
    await _insert_codebase(con, "mycodebase")
    await con.execute(
        "INSERT INTO candidate (codebase, suite, command) VALUES ($1, $2, 'true')",
        "mycodebase",
        "lintian-fixes",
    )

    policy, command, rate_limit_bucket = await publish.get_publish_policy(
        con, "mycodebase", "lintian-fixes"
    )
    assert policy == {}
    assert command == "true"
    assert rate_limit_bucket is None


async def test_publish_request_without_a_candidate_skips_every_role(aiohttp_client, db):
    async with db.acquire() as conn:
        await _insert_codebase(conn, "mypkg")
        await _insert_run(
            conn,
            run_id="run-1",
            codebase="mypkg",
            campaign="lintian-fixes",
            result_branches=[("main", "main", b"base-revid", b"revid")],
        )

    worker = _BusyPublishWorker()
    client = await _create_publish_client(aiohttp_client, db, worker)
    resp = await client.post("/lintian-fixes/mypkg/publish")
    assert resp.status == 202
    body = await resp.json()
    assert body["run_id"] == "run-1"
    assert list(body["publish_ids"]) == ["main"]
    # a started job is either still active or has reached the worker
    assert _active_publish_jobs(client) == 0
    assert worker.requests == []


async def test_publish_request_without_result_branches_reports_nothing_to_do(
    aiohttp_client, db
):
    async with db.acquire() as conn:
        await _insert_codebase(conn, "mypkg")
        await _insert_run(
            conn,
            run_id="run-1",
            codebase="mypkg",
            campaign="lintian-fixes",
            result_branches=[],
        )

    client = await create_client(aiohttp_client, db, CAMPAIGN_CONFIG)
    resp = await client.post("/lintian-fixes/mypkg/publish")
    assert resp.status == 200
    assert await resp.json() == {
        "run_id": "run-1",
        "code": "done",
        "description": "Nothing to do",
    }


async def test_publish_request_with_mode_keeps_the_rate_limit_bucket(
    aiohttp_client, db
):
    async with db.acquire() as conn:
        await _insert_codebase(conn, "mypkg")
        await _insert_run(
            conn,
            run_id="run-1",
            codebase="mypkg",
            campaign="lintian-fixes",
            result_branches=[("main", "main", b"base-revid", b"revid")],
        )
        await _insert_bucket_only_policy(conn)
        await conn.execute(
            "INSERT INTO candidate (codebase, suite, command, publish_policy) "
            "VALUES ($1, $2, 'true', 'bucketonly')",
            "mypkg",
            "lintian-fixes",
        )

    worker = _BusyPublishWorker()
    client = await _create_publish_client(aiohttp_client, db, worker)
    resp = await client.post("/lintian-fixes/mypkg/publish", data={"mode": "propose"})
    assert resp.status == 202
    await _wait_for_publish_jobs(client)
    assert [r["rate_limit_bucket"] for r in worker.requests] == ["bucket1"]
