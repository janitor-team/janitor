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

import aiohttp
import asyncpg
import pytest
import pytest_asyncio
import testing.postgresql
from aiohttp import web
from bs4 import BeautifulSoup
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


def create_config(database_location=None, extra=""):
    return read_config_string(f"""
campaign {{
  name: "lintian-fixes"
}}
artifact_location: "{tempfile.mkdtemp()}"
{f'database_location: "{database_location}"' if database_location else ""}
{extra}
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
        link = "http://192.168.1.5:8080/"
        queue = {"processing": [{"worker_link": link, "backchannel": {"my_url": link}}]}
        hidden = {
            "processing": [{"worker_link": None, "backchannel": {"my_url": None}}]
        }
        for channel in ["publish", "merge-proposal", "queue", "result"]:
            expected = hidden if channel == "queue" else queue
            payload = json.dumps(queue)
            for _ in range(100):
                if await redis.publish(channel, payload):
                    break
                await asyncio.sleep(0.01)
            assert await ws.receive_json(timeout=5) == [channel, expected]


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


async def test_webhook_gitlab_push_matches_codebase(aiohttp_client, database_location):
    conn = await asyncpg.connect(database_location)
    try:
        await conn.execute(
            "INSERT INTO codebase (name, branch_url, url, vcs_type) "
            "VALUES ($1, $2, $2, $3)",
            "example",
            "https://gitlab.com/jelmer/example.git",
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
        headers={"X-Gitlab-Event": "Push Hook"},
        json={
            "ref": "refs/heads/main",
            "after": "0" * 40,
            "project": {
                "git_http_url": "https://gitlab.com/jelmer/example.git",
                "git_ssh_url": "git@gitlab.com:jelmer/example.git",
                "default_branch": "main",
            },
        },
    )
    assert (resp.status, await resp.json()) == (
        200,
        {"rescheduled": {}, "urls": ["https://gitlab.com/jelmer/example.git"]},
    )


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


async def _start_provider(aiohttp_server, *, discovery=None, userinfo=None):
    """A local server standing in for the OpenID provider."""

    async def handle_discovery(request):
        if discovery is not None:
            return web.json_response(discovery)
        base = request.url.origin()
        return web.json_response(
            {
                "authorization_endpoint": str(base / "authorize"),
                "token_endpoint": str(base / "token"),
                "userinfo_endpoint": str(base / "userinfo"),
            }
        )

    async def handle_token(request):
        form = await request.post()
        assert (form["grant_type"], form["code"]) == ("authorization_code", "authcode")
        return web.json_response(
            {
                "token_type": "Bearer",
                "access_token": "the-access-token",
                "refresh_token": "the-refresh-token",
            }
        )

    async def handle_userinfo(request):
        assert request.headers["Authorization"] == "Bearer the-access-token"
        return web.json_response(userinfo)

    provider_app = web.Application()
    provider_app.router.add_get("/.well-known/openid-configuration", handle_discovery)
    provider_app.router.add_post("/token", handle_token)
    provider_app.router.add_get("/userinfo", handle_userinfo)
    return await aiohttp_server(provider_app)


async def _openid_site(aiohttp_client, database_location, provider=None, groups=""):
    base_url = f'base_url: "{provider.make_url("/")}"' if provider else ""
    oauth2_provider = f"""oauth2_provider {{
  client_id: "test-client"
  client_secret: "test-secret"
  {base_url}
  {groups}
}}"""
    _private_app, app = await create_app(
        config=create_config(database_location, oauth2_provider),
        external_url="https://example.com/",
        redis=FakeRedis(),
    )
    return await aiohttp_client(app)


def _login_links(html):
    soup = BeautifulSoup(html, "html.parser")
    return [a.get_text() for a in soup.select('a[href^="/login?"]')]


async def test_oauth_callback_without_state(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server)
    client = await _openid_site(aiohttp_client, database_location, provider)
    resp = await client.get("/oauth/callback", params={"code": "some-code"})
    assert (resp.status, await resp.text()) == (400, "state variable mismatch")


@pytest.mark.parametrize(
    "discovery, links",
    [(None, ["Login"]), ({}, [])],
)
async def test_login_link_with_provider(
    aiohttp_client, aiohttp_server, database_location, discovery, links
):
    provider = await _start_provider(aiohttp_server, discovery=discovery)
    client = await _openid_site(aiohttp_client, database_location, provider)
    resp = await client.get("/lintian-fixes/candidates")
    assert (resp.status, _login_links(await resp.text())) == (200, links)


async def test_login_link_hidden_without_provider(aiohttp_client, database_location):
    # app["openid_config"] is always set, to None when login is disabled.
    client = await _openid_site(aiohttp_client, database_location)
    resp = await client.get("/lintian-fixes/candidates")
    assert (resp.status, _login_links(await resp.text())) == (200, [])


async def _callback(client, **cookies):
    return await client.get(
        "/oauth/callback",
        params={"code": "authcode", "state": "mystate"},
        cookies={"state": "mystate", **cookies},
        allow_redirects=False,
    )


async def _add_session(client, session_id, userinfo):
    async with client.app.database.acquire() as conn:
        await conn.execute(
            "INSERT INTO site_session (id, userinfo) VALUES ($1, $2)",
            session_id,
            userinfo,
        )


async def _get_session(client, session_id):
    async with client.app.database.acquire() as conn:
        return await conn.fetchval(
            "SELECT userinfo FROM site_session WHERE id = $1", session_id
        )


@pytest.mark.parametrize(
    "userinfo, groups",
    [
        ({"name": "Alice", "email": "alice@example.com"}, []),
        ({"name": "Alice", "email": "alice@example.com", "groups": None}, []),
        ({"name": "Alice", "email": "alice@example.com", "groups": ["x"]}, ["x"]),
    ],
)
async def test_oauth_callback_defaults_missing_groups_claim(
    aiohttp_client, aiohttp_server, database_location, userinfo, groups
):
    provider = await _start_provider(aiohttp_server, userinfo=userinfo)
    client = await _openid_site(
        aiohttp_client, database_location, provider, groups='admin_group: "admins"'
    )
    resp = await _callback(client)
    session_id = resp.cookies["session_id"].value
    page = await client.get(
        "/lintian-fixes/candidates", cookies={"session_id": session_id}
    )
    stored = await _get_session(client, session_id)
    assert (resp.status, page.status, stored["groups"]) == (302, 200, groups)


ALICE = {"name": "Alice", "email": "alice@example.com", "groups": []}


async def test_logout_deletes_session_and_redirects_home(
    aiohttp_client, database_location
):
    client = await _openid_site(aiohttp_client, database_location)
    await _add_session(client, "mysession", ALICE)
    resp = await client.post(
        "/logout", cookies={"session_id": "mysession"}, allow_redirects=False
    )
    assert (
        resp.status,
        resp.headers["Location"],
        resp.cookies["session_id"]["max-age"],
        await _get_session(client, "mysession"),
    ) == (302, "/", "0", None)


async def test_logout_get_keeps_session(aiohttp_client, database_location):
    # A plain GET, as from a link or image on another site, must not log out.
    client = await _openid_site(aiohttp_client, database_location)
    await _add_session(client, "mysession", ALICE)
    resp = await client.get(
        "/logout", cookies={"session_id": "mysession"}, allow_redirects=False
    )
    assert (resp.status, await _get_session(client, "mysession")) == (405, ALICE)


async def test_logout_without_session_cookie_still_redirects(
    aiohttp_client, database_location
):
    client = await _openid_site(aiohttp_client, database_location)
    resp = await client.post("/logout", allow_redirects=False)
    assert (resp.status, resp.headers["Location"]) == (302, "/")


def _sidebar_user(html):
    """The POST forms in the sidebar and the text in front of the first."""
    soup = BeautifulSoup(html, "html.parser")
    forms = soup.select('div.sphinxsidebar form[method="post"]')
    actions = [(form["action"], form["method"]) for form in forms]
    return actions, " ".join(str(forms[0].previous_sibling).split())


@pytest.mark.parametrize("path", ["/lintian-fixes/candidates", "/cupboard/history"])
async def test_sidebar_shows_logout_button(aiohttp_client, database_location, path):
    client = await _openid_site(aiohttp_client, database_location)
    await _add_session(client, "mysession", ALICE)
    resp = await client.get(path, cookies={"session_id": "mysession"})
    assert resp.status == 200
    assert _sidebar_user(await resp.text()) == (
        [("/logout", "post")],
        "Logged in as Alice",
    )
