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

from datetime import timedelta

import pytest

import janitor.publish as publish
from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.publish import create_app
from janitor.runner import store_change_set, store_run


async def create_client(aiohttp_client, db, config_text=""):
    config = read_config_string(config_text)
    app = await create_app(vcs_managers={}, db=db, redis=None, config=config)
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


async def test_publish_request_without_a_candidate_skips_every_role(
    aiohttp_client, db, monkeypatch
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

    published = []

    async def _noop():
        return None

    def fake_publish_and_store(**kwargs):
        published.append(kwargs["role"])
        return _noop()

    monkeypatch.setattr(publish, "publish_and_store", fake_publish_and_store)

    client = await create_client(aiohttp_client, db, CAMPAIGN_CONFIG)
    resp = await client.post("/lintian-fixes/mypkg/publish")
    assert resp.status == 202
    body = await resp.json()
    assert body["run_id"] == "run-1"
    assert list(body["publish_ids"]) == ["main"]
    assert published == []


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
    aiohttp_client, db, monkeypatch
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

    captured = {}

    async def _noop():
        return None

    def fake_publish_and_store(**kwargs):
        captured.update(kwargs)
        return _noop()

    monkeypatch.setattr(publish, "publish_and_store", fake_publish_and_store)

    client = await create_client(aiohttp_client, db, CAMPAIGN_CONFIG)
    resp = await client.post("/lintian-fixes/mypkg/publish", data={"mode": "propose"})
    assert resp.status == 202
    assert captured["rate_limit_bucket"] == "bucket1"
