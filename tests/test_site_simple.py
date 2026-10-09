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

import tempfile
from pathlib import Path

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


LIMIT_ROUTES = [
    "/cupboard/review",
    "/cupboard/history",
    "/cupboard/queue",
    "/cupboard/publish/",
    "/cupboard/api/needs-review",
]

NON_INTEGER = ["abc", ""]

# A value the database layer rejects is pinned to the status only, since the body
# is that layer's own wording and so depends on its version and locale.
OUT_OF_RANGE = ["-1", "99999999999999999999999"]


async def _client(aiohttp_client, database_location):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    return await aiohttp_client(app)


async def _answers(client, paths, param, values):
    # Every path and value is asked, and the asserts compare the whole mapping, so
    # one bad route cannot hide another behind it.
    answers = {}
    for path in paths:
        for value in values:
            resp = await client.get(f"{path}?{param}={value}")
            answers[path, value] = (resp.status, await resp.text())
    return answers


async def test_limit_routes_accept_a_usable_limit(aiohttp_client, database_location):
    client = await _client(aiohttp_client, database_location)
    answers = await _answers(client, LIMIT_ROUTES, "limit", ["5"])
    bad = {k: status for k, (status, _body) in answers.items() if status != 200}
    assert not bad, bad


async def test_limit_routes_reject_a_non_integer_limit(
    aiohttp_client, database_location
):
    client = await _client(aiohttp_client, database_location)
    answers = await _answers(client, LIMIT_ROUTES, "limit", NON_INTEGER)
    wanted = (400, "limit must be an integer")
    bad = {
        k: (status, body[:80])
        for k, (status, body) in answers.items()
        if (status, body) != wanted
    }
    assert not bad, bad


async def test_limit_routes_reject_a_limit_the_database_will_not_take(
    aiohttp_client, database_location
):
    client = await _client(aiohttp_client, database_location)
    answers = await _answers(client, LIMIT_ROUTES, "limit", OUT_OF_RANGE)
    bad = {k: status for k, (status, _body) in answers.items() if status != 400}
    assert not bad, bad


async def test_history_accepts_a_usable_offset(aiohttp_client, database_location):
    client = await _client(aiohttp_client, database_location)
    resp = await client.get("/cupboard/history?offset=0")
    assert resp.status == 200


async def test_history_rejects_a_bad_offset(aiohttp_client, database_location):
    client = await _client(aiohttp_client, database_location)
    answers = await _answers(client, ["/cupboard/history"], "offset", NON_INTEGER)
    wanted = (400, "offset must be an integer")
    bad = {
        k: (status, body[:80])
        for k, (status, body) in answers.items()
        if (status, body) != wanted
    }
    assert not bad, bad

    answers = await _answers(client, ["/cupboard/history"], "offset", OUT_OF_RANGE)
    bad = {k: status for k, (status, _body) in answers.items() if status != 400}
    assert not bad, bad
