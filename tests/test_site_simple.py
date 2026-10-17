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
import tempfile
from pathlib import Path
from xml.etree import ElementTree

import aiohttp
import asyncpg
import pytest_asyncio
import testing.postgresql
from fakeredis.aioredis import FakeRedis

from janitor.config import read_string as read_config_string
from janitor.site.simple import create_app

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


async def test_logo_is_served(aiohttp_client, database_location):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get("/_static/janitor.svg")
    assert resp.status == 200
    assert resp.content_type == "image/svg+xml"
    root = ElementTree.fromstring(await resp.read())
    svg = "{http://www.w3.org/2000/svg}"
    assert root.tag == svg + "svg"
    assert root.get("viewBox") == "0 0 64 64"
    assert root.findtext(svg + "title") == "Janitor"
    tags = {el.tag for el in root.iter()}
    assert all(tag.startswith(svg) for tag in tags)
    assert not tags & {svg + "script", svg + "image", svg + "use", svg + "style"}
    assert not any("href" in name for el in root.iter() for name in el.attrib)


async def test_logo_in_pages(aiohttp_client, database_location):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    icon = '<link rel="icon" type="image/svg+xml" href="/_static/janitor.svg" />'
    img = '<img src="/_static/janitor.svg" alt="" width="32" height="32" />'
    for path in ["/lintian-fixes/", "/cupboard/"]:
        resp = await client.get(path)
        assert resp.status == 200
        text = await resp.text()
        assert icon in text[: text.index("</head>")]
        assert text.count(img) == 1


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
