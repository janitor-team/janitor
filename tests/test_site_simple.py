#!/usr/bin/python
# Copyright (C) 2022 Jelmer Vernooij <jelmer@jelmer.uk>
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
import json
import re
import tempfile
from datetime import timedelta
from pathlib import Path

import aiohttp
import asyncpg
import pytest
import pytest_asyncio
import testing.postgresql
from fakeredis.aioredis import FakeRedis

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.review import store_review
from janitor.site.simple import create_app

from .test_cupboard import _insert_codebase, _insert_run

_SCHEMA_DIR = Path(__file__).resolve().parent.parent / "schema"


@pytest_asyncio.fixture()
async def database_location():
    with testing.postgresql.Postgresql() as postgresql:
        conn = await asyncpg.connect(postgresql.url())
        try:
            await conn.execute((_SCHEMA_DIR / "state.sql").read_text())
            await conn.execute((_SCHEMA_DIR / "debian" / "debian.sql").read_text())
        finally:
            await conn.close()

        yield postgresql.url()


def create_config(database_location=None):
    return read_config_string(f"""
campaign {{
  name: "lintian-fixes"
}}
artifact_location: "{tempfile.mkdtemp()}"
{f'database_location: "{database_location}"' if database_location else ""}
""")


async def test_create_app():
    await create_app(config=create_config(), redis=FakeRedis())


async def test_codebase_query_redirect(aiohttp_client, database_location):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get(
        "/lintian-fixes/c", params={"codebase": "foo"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/lintian-fixes/c/foo/"


async def test_ws_notifications_forwards_redis_channels(
    aiohttp_client, database_location
):
    redis = FakeRedis()
    _private_app, app = await create_app(
        config=create_config(database_location), redis=redis
    )
    client = await aiohttp_client(app)
    async with client.ws_connect(
        "/ws/notifications", timeout=aiohttp.ClientWSTimeout(ws_close=0.1)
    ) as ws:
        for _ in range(100):
            if app["topic_notifications"].subscriptions:
                break
            await asyncio.sleep(0.01)
        for channel in ["publish", "merge-proposal", "queue", "result"]:
            payload = json.dumps({"channel": channel})
            for _ in range(100):
                if await redis.publish(channel, payload):
                    break
                await asyncio.sleep(0.01)
            assert await ws.receive_json(timeout=5) == [channel, {"channel": channel}]


async def test_codebase_query_redirect_legacy_package_param(
    aiohttp_client, database_location
):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get(
        "/lintian-fixes/c", params={"package": "foo"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/lintian-fixes/c/foo/"


async def test_codebase_query_redirect_prefers_codebase_over_package(
    aiohttp_client, database_location
):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get(
        "/lintian-fixes/c",
        params={"codebase": "new", "package": "old"},
        allow_redirects=False,
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/lintian-fixes/c/new/"


async def test_codebase_query_redirect_missing_param_returns_404(
    aiohttp_client, database_location
):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get("/lintian-fixes/c", allow_redirects=False)
    assert resp.status == 404


async def test_codebase_query_redirect_empty_param_returns_404(
    aiohttp_client, database_location
):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get(
        "/lintian-fixes/c", params={"codebase": ""}, allow_redirects=False
    )
    assert resp.status == 404


async def test_webhook_github_push_reschedules_without_crashing(
    aiohttp_client, database_location
):
    # A GitHub push webhook for a known codebase reschedules it successfully.
    conn = await asyncpg.connect(database_location)
    try:
        await conn.execute(
            "INSERT INTO codebase (name, branch_url, url, vcs_type) "
            "VALUES ($1, $2, $2, $3)",
            "example",
            "https://github.com/jelmer/example",
            "git",
        )
    finally:
        await conn.close()

    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.post(
        "/webhook",
        headers={"X-GitHub-Event": "push"},
        json={
            "ref": "refs/heads/main",
            "after": "0" * 40,
            "repository": {
                "clone_url": "https://github.com/jelmer/example",
                "html_url": "https://github.com/jelmer/example",
                "git_url": "git://github.com/jelmer/example.git",
                "ssh_url": "git@github.com:jelmer/example.git",
                "default_branch": "main",
            },
        },
    )
    assert resp.status == 200, await resp.text()
    body = await resp.json()
    assert "https://github.com/jelmer/example" in body["urls"]


async def test_candidates_with_multiple_unscored_does_not_500(
    aiohttp_client, database_location
):
    # The candidates page loads fine with two or more unscored candidates.
    conn = await asyncpg.connect(database_location)
    try:
        await conn.execute(
            "INSERT INTO codebase (name, branch_url, url, vcs_type) VALUES "
            "($1, $2, $2, $3), ($4, $5, $5, $3)",
            "foo",
            "https://example.com/foo.git",
            "git",
            "bar",
            "https://example.com/bar.git",
        )
        await conn.execute(
            "INSERT INTO candidate (codebase, suite, command) VALUES "
            "($1, $2, $3), ($4, $2, $3)",
            "foo",
            "lintian-fixes",
            "true",
            "bar",
        )
    finally:
        await conn.close()

    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get("/lintian-fixes/candidates")
    assert resp.status == 200


async def test_merge_proposal_without_url_returns_400(
    aiohttp_client, database_location
):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get("/lintian-fixes/merge-proposal")
    assert resp.status == 400
    assert await resp.text() == "no url specified"


def _review_samples(text):
    return {
        m.group(1): float(m.group(2))
        for m in re.finditer(r'^review_total\{verdict="(\w+)"\} (\S+)$', text, re.M)
    }


async def _scrape(client):
    resp = await client.get("/metrics")
    assert resp.status == 200
    return await resp.text()


async def _store(conn, run_id, verdict, reviewer):
    await store_review(
        conn,
        None,
        None,
        run_id,
        verdict=verdict,
        comment=None,
        reviewer=reviewer,
        is_qa_reviewer=False,
    )


async def _metrics_client(aiohttp_client):
    private_app, _app = await create_app(config=create_config(), redis=FakeRedis())
    return await aiohttp_client(private_app)


async def test_metrics_count_reviews_by_verdict(aiohttp_client, db):
    client = await _metrics_client(aiohttp_client)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        now = utcnow()
        await _insert_run(
            conn,
            run_id="run1",
            codebase="foo",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
        )
        before = _review_samples(await _scrape(client))
        await _store(conn, "run1", "approved", "a@example.com")
        await _store(conn, "run1", "approved", "b@example.com")
        await _store(conn, "run1", "rejected", "c@example.com")

    text = await _scrape(client)
    after = _review_samples(text)
    assert after["approved"] - before.get("approved", 0) == 2
    assert after["rejected"] - before.get("rejected", 0) == 1
    assert after.get("abstained", 0) == before.get("abstained", 0)
    assert "example.com" not in text


async def test_failed_review_is_not_counted(aiohttp_client, db):
    client = await _metrics_client(aiohttp_client)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        now = utcnow()
        await _insert_run(
            conn,
            run_id="run1",
            codebase="foo",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
        )
        await _store(conn, "run1", "approved", "a@example.com")
        with pytest.raises(asyncpg.InvalidTextRepresentationError):
            await _store(conn, "nosuchrun", "bogus", "a@example.com")

    samples = _review_samples(await _scrape(client))
    assert samples["approved"] >= 1
    assert "bogus" not in samples
