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

from unittest.mock import AsyncMock, MagicMock

from aiohttp import ClientSession, web
from yarl import URL

from janitor.config import read_string as read_config_string
from janitor.site.openid import (
    _sanitize_redirect,
    discover_openid_config,
    setup_openid,
)


def test_sanitize_redirect_rejects_protocol_relative_url():
    # "//evil.com" strips to an empty path once the host is removed, which
    # fails the leading-slash check.
    assert _sanitize_redirect("//evil.com") is None


def test_sanitize_redirect_rejects_backslash_host_payload():
    assert _sanitize_redirect("/\\evil.com") is None


def test_sanitize_redirect_rejects_double_slash_after_host_strip():
    # A path with a double slash right after the host survives host
    # stripping as "//x", which browsers can still resolve as an authority.
    assert _sanitize_redirect("http://evil.com//x") is None


def test_sanitize_redirect_rejects_unparseable_url():
    assert _sanitize_redirect("not a url at all") is None


def test_sanitize_redirect_strips_scheme_and_host_from_absolute_url():
    # A full URL pointing back at this site (as a browser would supply via
    # location.href) is reduced to a bare same-origin path.
    assert _sanitize_redirect("https://example.com/somewhere") == "/somewhere"


async def _build_app(db):
    app = web.Application()
    app.database = db
    config = read_config_string(
        'oauth2_provider {\n  client_id: "test-client"\n  client_secret: "test-secret"\n}\n'
    )
    app["config"] = config
    app["external_url"] = URL("https://example.com/")

    async def persistent_session(app):
        app["http_client_session"] = session = ClientSession()
        yield
        await session.close()

    app.cleanup_ctx.append(persistent_session)
    setup_openid(app, None)
    return app


async def test_login_redirects_to_authorization_endpoint(aiohttp_client, db):
    app = await _build_app(db)
    app["openid_config"] = {
        "authorization_endpoint": "https://provider.example/authorize"
    }
    client = await aiohttp_client(app)

    resp = await client.get("/login", allow_redirects=False)
    assert resp.status == 302
    location = URL(resp.headers["Location"])
    assert location.host == "provider.example"
    assert location.query["client_id"] == "test-client"
    assert location.query["redirect_uri"] == "https://example.com/oauth/callback"


async def test_login_disabled_returns_404(aiohttp_client, db):
    app = await _build_app(db)
    app["openid_config"] = None
    client = await aiohttp_client(app)

    resp = await client.get("/login", allow_redirects=False)
    assert resp.status == 404


async def test_login_rejects_open_redirect_url(aiohttp_client, db):
    app = await _build_app(db)
    app["openid_config"] = {
        "authorization_endpoint": "https://provider.example/authorize"
    }
    client = await aiohttp_client(app)

    resp = await client.get(
        "/login", params={"url": "//evil.com"}, allow_redirects=False
    )
    assert resp.status == 400


async def test_login_accepts_same_origin_back_url(aiohttp_client, db):
    app = await _build_app(db)
    app["openid_config"] = {
        "authorization_endpoint": "https://provider.example/authorize"
    }
    client = await aiohttp_client(app)

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


async def test_logout_deletes_session_and_redirects_home(aiohttp_client, db):
    app = await _build_app(db)
    app["openid_config"] = None
    client = await aiohttp_client(app)

    async with db.acquire() as conn:
        await conn.execute(
            "INSERT INTO site_session (id, userinfo) VALUES ($1, $2)",
            "mysession",
            {"email": "alice@example.com", "groups": []},
        )

    resp = await client.get(
        "/logout", cookies={"session_id": "mysession"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/"

    async with db.acquire() as conn:
        count = await conn.fetchval(
            "SELECT count(*) FROM site_session WHERE id = $1", "mysession"
        )
    assert count == 0


async def test_logout_without_session_cookie_still_redirects(aiohttp_client, db):
    app = await _build_app(db)
    app["openid_config"] = None
    client = await aiohttp_client(app)

    resp = await client.get("/logout", allow_redirects=False)
    assert resp.status == 302
    assert resp.headers["Location"] == "/"


async def test_logout_ignores_unsafe_redirect_target(aiohttp_client, db):
    app = await _build_app(db)
    app["openid_config"] = None
    client = await aiohttp_client(app)

    resp = await client.get(
        "/logout", params={"url": "//evil.com"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/"


async def test_logout_honours_safe_redirect_target(aiohttp_client, db):
    app = await _build_app(db)
    app["openid_config"] = None
    client = await aiohttp_client(app)

    resp = await client.get(
        "/logout",
        params={"url": "https://example.com/after-logout"},
        allow_redirects=False,
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/after-logout"


async def _create_oauth_provider(aiohttp_client, *, token_response, userinfo_response):
    async def handle_token(request):
        assert request.content_type == "application/x-www-form-urlencoded"
        body = await request.post()
        assert body["grant_type"] == "authorization_code"
        assert body["code"] == "authcode"
        return web.json_response(token_response)

    async def handle_userinfo(request):
        assert request.headers["Authorization"] == "Bearer the-access-token"
        return web.json_response(userinfo_response)

    provider_app = web.Application()
    provider_app.router.add_post("/token", handle_token)
    provider_app.router.add_get("/userinfo", handle_userinfo)
    return await aiohttp_client(provider_app)


async def test_oauth_callback_sends_token_request_as_form_body(aiohttp_client, db):
    provider_client = await _create_oauth_provider(
        aiohttp_client,
        token_response={
            "token_type": "Bearer",
            "access_token": "the-access-token",
            "refresh_token": "the-refresh-token",
        },
        userinfo_response={"email": "alice@example.com"},
    )
    app = await _build_app(db)
    app["openid_config"] = {
        "token_endpoint": str(provider_client.make_url("/token")),
        "userinfo_endpoint": str(provider_client.make_url("/userinfo")),
    }
    client = await aiohttp_client(app)

    resp = await client.get(
        "/oauth/callback",
        params={"code": "authcode", "state": "mystate"},
        cookies={"state": "mystate"},
        allow_redirects=False,
    )
    assert resp.status == 302


async def test_oauth_callback_defaults_missing_groups_claim(aiohttp_client, db):
    provider_client = await _create_oauth_provider(
        aiohttp_client,
        token_response={
            "token_type": "Bearer",
            "access_token": "the-access-token",
            "refresh_token": "the-refresh-token",
        },
        userinfo_response={"email": "alice@example.com"},
    )
    app = await _build_app(db)
    app["openid_config"] = {
        "token_endpoint": str(provider_client.make_url("/token")),
        "userinfo_endpoint": str(provider_client.make_url("/userinfo")),
    }
    client = await aiohttp_client(app)

    resp = await client.get(
        "/oauth/callback",
        params={"code": "authcode", "state": "mystate"},
        cookies={"state": "mystate"},
        allow_redirects=False,
    )
    assert resp.status == 302
    session_id = resp.cookies["session_id"].value

    async with db.acquire() as conn:
        userinfo = await conn.fetchval(
            "SELECT userinfo FROM site_session WHERE id = $1", session_id
        )
    assert userinfo["email"] == "alice@example.com"
    assert userinfo["groups"] == []


async def test_oauth_callback_preserves_existing_groups_claim(aiohttp_client, db):
    provider_client = await _create_oauth_provider(
        aiohttp_client,
        token_response={
            "token_type": "Bearer",
            "access_token": "the-access-token",
            "refresh_token": "the-refresh-token",
        },
        userinfo_response={"email": "alice@example.com", "groups": ["admins"]},
    )
    app = await _build_app(db)
    app["openid_config"] = {
        "token_endpoint": str(provider_client.make_url("/token")),
        "userinfo_endpoint": str(provider_client.make_url("/userinfo")),
    }
    client = await aiohttp_client(app)

    resp = await client.get(
        "/oauth/callback",
        params={"code": "authcode", "state": "mystate"},
        cookies={"state": "mystate"},
        allow_redirects=False,
    )
    session_id = resp.cookies["session_id"].value

    async with db.acquire() as conn:
        userinfo = await conn.fetchval(
            "SELECT userinfo FROM site_session WHERE id = $1", session_id
        )
    assert userinfo["groups"] == ["admins"]


async def test_oauth_callback_clears_state_cookie_with_matching_path(
    aiohttp_client, db
):
    provider_client = await _create_oauth_provider(
        aiohttp_client,
        token_response={
            "token_type": "Bearer",
            "access_token": "the-access-token",
            "refresh_token": "the-refresh-token",
        },
        userinfo_response={"email": "alice@example.com"},
    )
    app = await _build_app(db)
    app["openid_config"] = {
        "token_endpoint": str(provider_client.make_url("/token")),
        "userinfo_endpoint": str(provider_client.make_url("/userinfo")),
    }
    client = await aiohttp_client(app)

    resp = await client.get(
        "/oauth/callback",
        params={"code": "authcode", "state": "mystate"},
        cookies={"state": "mystate"},
        allow_redirects=False,
    )
    state_deletion = next(
        h for h in resp.headers.getall("Set-Cookie") if h.startswith("state=")
    )
    assert "Path=/oauth/callback" in state_deletion


async def test_oauth_callback_rejects_unsafe_back_url_cookie(aiohttp_client, db):
    # back_url is set by handle_login after sanitizing it, but it's a
    # regular cookie - nothing stops a request from arriving here with
    # that cookie set directly, bypassing handle_login's check entirely.
    provider_client = await _create_oauth_provider(
        aiohttp_client,
        token_response={
            "token_type": "Bearer",
            "access_token": "the-access-token",
            "refresh_token": "the-refresh-token",
        },
        userinfo_response={"email": "alice@example.com"},
    )
    app = await _build_app(db)
    app["openid_config"] = {
        "token_endpoint": str(provider_client.make_url("/token")),
        "userinfo_endpoint": str(provider_client.make_url("/userinfo")),
    }
    client = await aiohttp_client(app)

    resp = await client.get(
        "/oauth/callback",
        params={"code": "authcode", "state": "mystate"},
        cookies={"state": "mystate", "back_url": "//evil.example"},
        allow_redirects=False,
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/"


async def test_oauth_callback_state_mismatch_returns_400(aiohttp_client, db):
    app = await _build_app(db)
    app["openid_config"] = {
        "token_endpoint": "https://provider.example/token",
        "userinfo_endpoint": "https://provider.example/userinfo",
    }
    client = await aiohttp_client(app)

    resp = await client.get(
        "/oauth/callback",
        params={"code": "authcode", "state": "mystate"},
        cookies={"state": "othervalue"},
        allow_redirects=False,
    )
    assert resp.status == 400


def _fake_discovery_session(json_body, status=200):
    resp = AsyncMock()
    resp.status = status
    resp.json = AsyncMock(return_value=json_body)
    resp.read = AsyncMock(return_value=b"")
    cm = MagicMock()
    cm.__aenter__ = AsyncMock(return_value=resp)
    cm.__aexit__ = AsyncMock(return_value=False)
    session = MagicMock()
    session.get = MagicMock(return_value=cm)
    return session


async def test_discover_openid_config_stores_provider_response():
    app = {
        "http_client_session": _fake_discovery_session(
            {
                "token_endpoint": "https://gitlab.com/oauth/token",
                "authorization_endpoint": "https://gitlab.com/oauth/authorize",
                "userinfo_endpoint": "https://gitlab.com/oauth/userinfo",
            }
        )
    }
    await discover_openid_config(app, "https://gitlab.com")
    assert app["openid_config"]["token_endpoint"] == "https://gitlab.com/oauth/token"


async def test_discover_openid_config_failure_leaves_config_unset():
    app = {"http_client_session": _fake_discovery_session({}, status=500)}
    await discover_openid_config(app, "https://auth.example.com")
    assert "openid_config" not in app
