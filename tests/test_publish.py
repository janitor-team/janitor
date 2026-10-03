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

import logging
from datetime import timedelta, timezone

import aiozipkin
import pytest

import janitor.publish as publish
from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.publish import create_app
from janitor.runner import store_change_set, store_run


async def create_client(aiohttp_client, db):
    config = read_config_string("")
    app = await create_app(vcs_managers={}, db=db, redis=None, config=config)
    endpoint = aiozipkin.create_endpoint("janitor.publish", ipv4="127.0.0.1", port=80)
    tracer = await aiozipkin.create_custom(endpoint)
    aiozipkin.setup(app, tracer)
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


async def _insert_publish_ready_run(conn, *, finish_time):
    # Store a real run, since the timestamp column hands finish_time back naive.
    await conn.execute(
        "INSERT INTO codebase (name, branch_url, url) VALUES ($1, $2, $2)",
        "mypkg",
        "https://example.com/mypkg.git",
    )
    await conn.execute(
        "INSERT INTO named_publish_policy (name, rate_limit_bucket) "
        "VALUES ('mypolicy', 'default')"
    )
    await conn.execute(
        "INSERT INTO candidate (codebase, suite, command, publish_policy) "
        "VALUES ('mypkg', 'lintian-fixes', 'true', 'mypolicy')"
    )
    await store_change_set(conn, "run-1", campaign="lintian-fixes")
    await store_run(
        conn,
        run_id="run-1",
        codebase="mypkg",
        campaign="lintian-fixes",
        vcs_type="git",
        subpath="",
        start_time=finish_time - timedelta(minutes=5),
        finish_time=finish_time,
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
        change_set="run-1",
        branch_url="https://example.com/mypkg.git",
    )


async def _insert_failed_publish(conn):
    await conn.execute(
        "INSERT INTO publish (id, change_set, target_branch_url, revision, "
        "mode, result_code) VALUES ('p1', 'run-1', "
        "'https://example.com/mypkg.git', 'revid', 'propose', 'publish-failed')"
    )


def _an_hour_ago():
    return utcnow() - timedelta(hours=1)


async def test_blockers_backoff_no_attempts(aiohttp_client, db):
    finish_time = _an_hour_ago()
    async with db.acquire() as conn:
        await _insert_publish_ready_run(conn, finish_time=finish_time)

    client = await create_client(aiohttp_client, db)
    resp = await client.get("/blockers/run-1")
    assert resp.status == 200
    assert (await resp.json())["backoff"] == {
        "result": True,
        "details": {
            "attempt_count": 0,
            "next_try_time": finish_time.replace(tzinfo=timezone.utc).isoformat(),
        },
    }


async def test_blockers_backoff_inside_window(aiohttp_client, db):
    finish_time = _an_hour_ago()
    async with db.acquire() as conn:
        await _insert_publish_ready_run(conn, finish_time=finish_time)
        await _insert_failed_publish(conn)

    client = await create_client(aiohttp_client, db)
    resp = await client.get("/blockers/run-1")
    assert resp.status == 200
    next_try_time = finish_time.replace(tzinfo=timezone.utc) + timedelta(hours=2)
    assert (await resp.json())["backoff"] == {
        "result": False,
        "details": {"attempt_count": 1, "next_try_time": next_try_time.isoformat()},
    }


async def test_consider_publish_run_inside_backoff_window(db, caplog):
    caplog.set_level(logging.INFO, logger="janitor.publish")
    config = read_config_string('campaign {\n  name: "lintian-fixes"\n}\n')
    async with db.acquire() as conn:
        await _insert_publish_ready_run(conn, finish_time=_an_hour_ago())
        await _insert_failed_publish(conn)
        run = await publish.get_last_effective_run(conn, "mypkg", "lintian-fixes")
        assert run is not None

        result = await publish.consider_publish_run(
            conn,
            None,
            config=config,
            publish_worker=publish.PublishWorker(),
            vcs_managers={},
            bucket_rate_limiter=publish.NonRateLimiter(),
            run=run,
            rate_limit_bucket="default",
            unpublished_branches=[],
            command="true",
        )
    assert result == {}
    assert "due to exponential backoff" in caplog.text
