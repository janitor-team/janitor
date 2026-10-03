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
from typing import cast

import pytest
from breezy.forge import Forge

import janitor.publish as publish
from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.publish import create_app


async def create_client(aiohttp_client, db):
    config = read_config_string("")
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
