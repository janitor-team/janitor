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

import tempfile
from pathlib import Path

import asyncpg
import pytest
import pytest_asyncio
import testing.postgresql
from aiohttp import web
from fakeredis.aioredis import FakeRedis
from yarl import URL

from janitor.config import read_string as read_config_string
from janitor.site import openid
from janitor.site.simple import create_app

_SCHEMA_DIR = Path(__file__).resolve().parent.parent / "schema"


def test_sanitize_redirect_rejects_protocol_relative_url():
    # "//evil.com" has an empty path once the host is removed.
    assert openid._sanitize_redirect("//evil.com") is None


def test_sanitize_redirect_rejects_backslash_host_payload():
    assert openid._sanitize_redirect("/\\evil.com") is None


def test_sanitize_redirect_rejects_double_slash_after_host_strip():
    # Without its host this is "//x", which a browser reads as a host again.
    assert openid._sanitize_redirect("http://evil.com//x") is None


def test_sanitize_redirect_rejects_unparseable_url():
    assert openid._sanitize_redirect("not a url at all") is None


def test_sanitize_redirect_keeps_a_relative_path():
    # The callback reads back the path that /login stored in the cookie.
    assert openid._sanitize_redirect("/cupboard/queue") == "/cupboard/queue"
    assert openid._sanitize_redirect("/c/foo?x=1") == "/c/foo?x=1"
    assert openid._sanitize_redirect("/") == "/"


def test_sanitize_redirect_strips_scheme_and_host_from_absolute_url():
    # A full URL is cut down to its path.
    assert openid._sanitize_redirect("https://example.com/somewhere") == "/somewhere"


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


async def _start_provider(
    aiohttp_server, *, userinfo=None, discovery_status=200, discovery=None
):
    # A local server standing in for the OpenID provider.
    async def handle_discovery(request):
        if discovery is not None:
            return web.json_response(discovery, status=discovery_status)
        base = request.url.origin()
        return web.json_response(
            {
                "authorization_endpoint": str(base / "authorize"),
                "token_endpoint": str(base / "token"),
                "userinfo_endpoint": str(base / "userinfo"),
            },
            status=discovery_status,
        )

    async def handle_token(request):
        # 54668dac moved the token exchange to a form-encoded body
        form = await request.post()
        assert form["grant_type"] == "authorization_code"
        assert form["code"] == "authcode"
        assert form["client_id"] == "test-client"
        assert form["client_secret"] == "test-secret"
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


async def _create_site(aiohttp_client, database_location, provider=None, groups=""):
    base_url = f'base_url: "{provider.make_url("/")}"' if provider else ""
    config = read_config_string(f"""
campaign {{
  name: "lintian-fixes"
}}
artifact_location: "{tempfile.mkdtemp()}"
database_location: "{database_location}"
oauth2_provider {{
  client_id: "test-client"
  client_secret: "test-secret"
  {base_url}
  {groups}
}}
""")
    _private_app, app = await create_app(
        config=config, external_url="https://example.com/", redis=FakeRedis()
    )
    return await aiohttp_client(app)


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


async def _callback(client, **cookies):
    return await client.get(
        "/oauth/callback",
        params={"code": "authcode", "state": "mystate"},
        cookies={"state": "mystate", **cookies},
        allow_redirects=False,
    )


async def test_login_redirects_to_authorization_endpoint(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server)
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await client.get("/login", allow_redirects=False)
    assert resp.status == 302
    location = URL(resp.headers["Location"])
    assert location.with_query(None) == provider.make_url("/authorize")
    assert location.query["client_id"] == "test-client"
    assert location.query["redirect_uri"] == "https://example.com/oauth/callback"


async def test_login_disabled_returns_404(aiohttp_client, database_location):
    client = await _create_site(aiohttp_client, database_location)

    resp = await client.get("/login", allow_redirects=False)
    assert resp.status == 404


async def test_login_rejects_open_redirect_url(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server)
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await client.get(
        "/login", params={"url": "//evil.com"}, allow_redirects=False
    )
    assert resp.status == 400


async def test_login_rejects_double_slash_after_host(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server)
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await client.get(
        "/login", params={"url": "http://evil.com//x"}, allow_redirects=False
    )
    assert resp.status == 400


async def test_login_accepts_relative_back_url(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server)
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await client.get(
        "/login", params={"url": "/cupboard/queue"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.cookies["back_url"].value == "/cupboard/queue"


async def test_login_accepts_same_origin_back_url(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server)
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await client.get(
        "/login",
        params={"url": "https://example.com/somewhere"},
        allow_redirects=False,
    )
    assert resp.status == 302
    back_url_cookie = next(
        h for h in resp.headers.getall("Set-Cookie") if h.startswith("back_url=")
    )
    assert 'back_url="/somewhere"' in back_url_cookie


async def test_logout_deletes_session_and_redirects_home(
    aiohttp_client, database_location
):
    client = await _create_site(aiohttp_client, database_location)
    await _add_session(
        client, "mysession", {"email": "alice@example.com", "groups": []}
    )

    resp = await client.get(
        "/logout", cookies={"session_id": "mysession"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/"
    assert await _get_session(client, "mysession") is None
    session_deletion = next(
        h for h in resp.headers.getall("Set-Cookie") if h.startswith("session_id=")
    )
    assert "Max-Age=0" in session_deletion


async def test_logout_without_session_cookie_still_redirects(
    aiohttp_client, database_location
):
    client = await _create_site(aiohttp_client, database_location)

    resp = await client.get("/logout", allow_redirects=False)
    assert resp.status == 302
    assert resp.headers["Location"] == "/"


async def test_logout_ignores_unsafe_redirect_target(aiohttp_client, database_location):
    client = await _create_site(aiohttp_client, database_location)

    resp = await client.get(
        "/logout", params={"url": "//evil.com"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/"


async def test_logout_honours_safe_redirect_target(aiohttp_client, database_location):
    client = await _create_site(aiohttp_client, database_location)

    resp = await client.get(
        "/logout",
        params={"url": "https://example.com/after-logout"},
        allow_redirects=False,
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/after-logout"


async def test_oauth_callback_defaults_missing_groups_claim(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(
        aiohttp_server, userinfo={"name": "Alice", "email": "alice@example.com"}
    )
    client = await _create_site(
        aiohttp_client, database_location, provider, groups='admin_group: "admins"'
    )

    resp = await _callback(client)
    assert resp.status == 302
    session_id = resp.cookies["session_id"].value
    # Without the claim the admin check failed on every later page.
    resp = await client.get(
        "/lintian-fixes/candidates", cookies={"session_id": session_id}
    )
    assert resp.status == 200
    assert (await _get_session(client, session_id))["groups"] == []


async def test_oauth_callback_preserves_existing_groups_claim(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(
        aiohttp_server,
        userinfo={"email": "alice@example.com", "groups": ["admins"]},
    )
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await _callback(client)
    assert resp.status == 302
    userinfo = await _get_session(client, resp.cookies["session_id"].value)
    assert userinfo["groups"] == ["admins"]


async def test_oauth_callback_clears_state_cookie_with_matching_path(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(
        aiohttp_server, userinfo={"email": "alice@example.com", "groups": []}
    )
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await _callback(client)
    assert resp.status == 302
    state_deletion = next(
        h for h in resp.headers.getall("Set-Cookie") if h.startswith("state=")
    )
    assert "Path=/oauth/callback" in state_deletion


async def test_oauth_callback_returns_to_back_url_cookie(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(
        aiohttp_server, userinfo={"email": "alice@example.com", "groups": []}
    )
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await _callback(client, back_url="/cupboard/queue")
    assert resp.status == 302
    assert resp.headers["Location"] == "/cupboard/queue"


@pytest.mark.parametrize(
    "back_url",
    [
        "//evil.example",
        "http://evil.example//x",
        "https://evil.example/",
        "javascript:alert(1)",
        "not a url at all",
    ],
)
async def test_oauth_callback_keeps_unsafe_back_url_cookie_on_site(
    aiohttp_client, aiohttp_server, database_location, back_url
):
    # back_url is a plain cookie, so a request can set it without /login.
    provider = await _start_provider(
        aiohttp_server, userinfo={"email": "alice@example.com", "groups": []}
    )
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await _callback(client, back_url=back_url)
    assert resp.status == 302
    location = resp.headers["Location"]
    assert location.startswith("/") and not location.startswith("//"), location


async def test_oauth_callback_state_mismatch_returns_400(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server)
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await client.get(
        "/oauth/callback",
        params={"code": "authcode", "state": "mystate"},
        cookies={"state": "othervalue"},
        allow_redirects=False,
    )
    assert resp.status == 400


async def test_discover_openid_config_stores_provider_response(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server)
    client = await _create_site(aiohttp_client, database_location, provider)

    assert client.app["openid_config"] == {
        "authorization_endpoint": str(provider.make_url("/authorize")),
        "token_endpoint": str(provider.make_url("/token")),
        "userinfo_endpoint": str(provider.make_url("/userinfo")),
    }


async def test_discover_openid_config_failure_leaves_config_unset(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server, discovery_status=500)
    client = await _create_site(aiohttp_client, database_location, provider)

    assert client.app["openid_config"] is None
    resp = await client.get("/login", allow_redirects=False)
    assert resp.status == 404


async def test_login_link_hidden_without_provider(aiohttp_client, database_location):
    # app["openid_config"] is always set, to None when login is disabled.
    client = await _create_site(aiohttp_client, database_location)

    resp = await client.get("/lintian-fixes/candidates")
    assert resp.status == 200
    assert 'href="/login?' not in await resp.text()


async def test_login_link_hidden_when_discovery_returns_nothing(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server, discovery={})
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await client.get("/lintian-fixes/candidates")
    assert resp.status == 200
    assert 'href="/login?' not in await resp.text()


async def test_login_link_shown_with_provider(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server)
    client = await _create_site(aiohttp_client, database_location, provider)

    resp = await client.get("/lintian-fixes/candidates")
    assert resp.status == 200
    assert 'href="/login?' in await resp.text()


async def test_generic_sidebar_shows_role_and_logout_link(
    aiohttp_client, database_location
):
    client = await _create_site(
        aiohttp_client, database_location, groups='admin_group: "admins"'
    )
    await _add_session(
        client,
        "mysession",
        {"name": "Alice", "email": "alice@example.com", "groups": ["admins"]},
    )

    resp = await client.get(
        "/lintian-fixes/candidates", cookies={"session_id": "mysession"}
    )
    assert resp.status == 200
    text = await resp.text()
    assert "Logged in as Alice" in text
    assert "(admin)" in text
    assert '<a href="/logout">Log out</a>' in text
    assert "/m/alice@example.com" not in text


async def test_cupboard_sidebar_shows_role_and_logout_link(
    aiohttp_client, database_location
):
    client = await _create_site(
        aiohttp_client,
        database_location,
        groups='admin_group: "admins"\n  qa_reviewer_group: "reviewers"',
    )
    await _add_session(
        client,
        "mysession",
        {"name": "Alice", "email": "alice@example.com", "groups": ["reviewers"]},
    )

    resp = await client.get("/cupboard/history", cookies={"session_id": "mysession"})
    assert resp.status == 200
    text = await resp.text()
    assert "Logged in as Alice" in text
    assert "(reviewer)" in text
    assert '<a href="/logout">Log out</a>' in text
    assert "/cupboard/maintainer/" not in text
