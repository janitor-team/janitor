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

"""OpenID support."""

import logging
import os
import uuid
from typing import Optional

from aiohttp import web
from yarl import URL


@web.middleware
async def openid_middleware(request, handler):
    session_id = request.cookies.get("session_id")
    if session_id is not None:
        async with request.app.database.acquire() as conn:
            userinfo = await conn.fetchval(
                "SELECT userinfo FROM site_session WHERE id = $1", session_id
            )
    else:
        userinfo = None
    request["user"] = userinfo
    resp = await handler(request)
    return resp


def _sanitize_redirect(raw: str) -> Optional[str]:
    r"""Reduce a user-supplied redirect target to a same-origin path.

    URL(...).relative() strips scheme and host, but a result like
    '//evil.com' or '/\\evil.com' is still browser-navigable to a
    different origin - browsers resolve repeated leading slashes and
    backslashes as authority separators - so those are rejected rather
    than passed through. Returns None if raw isn't a usable absolute URL.
    """
    try:
        rel = str(URL(raw).relative())
    except ValueError:
        return None
    if not rel.startswith("/") or rel.startswith("//") or "\\" in rel:
        return None
    return rel


async def handle_oauth_callback(request):
    code = request.query.get("code")
    state_code = request.query.get("state")
    if request.cookies.get("state") != state_code:
        return web.Response(status=400, text="state variable mismatch")
    if not request.app["openid_config"]:
        raise web.HTTPNotFound(text="login disabled")
    token_url = URL(request.app["openid_config"]["token_endpoint"])
    redirect_uri = (request.app["external_url"] or request.url).join(
        request.app.router["oauth2-callback"].url_for()
    )
    params = {
        "code": code,
        "client_id": request.app["config"].oauth2_provider.client_id
        or os.environ["OAUTH2_CLIENT_ID"],
        "client_secret": request.app["config"].oauth2_provider.client_secret
        or os.environ["OAUTH2_CLIENT_SECRET"],
        "grant_type": "authorization_code",
        "redirect_uri": str(redirect_uri),
    }
    async with request.app["http_client_session"].post(token_url, data=params) as resp:
        if resp.status != 200:
            return web.json_response(
                status=resp.status,
                data={
                    "error": "token-error",
                    "message": f"received response {resp.status}",
                },
            )
        resp = await resp.json()
        if resp["token_type"] != "Bearer":
            return web.Response(
                status=500,
                text="Expected bearer token, got {}".format(resp["token_type"]),
            )
        refresh_token = resp["refresh_token"]  # noqa: F841
        access_token = resp["access_token"]

    back_url = "/"
    raw_back_url = request.cookies.get("back_url")
    if raw_back_url is not None:
        target = _sanitize_redirect(raw_back_url)
        if target is not None:
            back_url = target

    async with request.app["http_client_session"].get(
        request.app["openid_config"]["userinfo_endpoint"],
        headers={"Authorization": f"Bearer {access_token}"},
        raise_for_status=True,
    ) as resp:
        userinfo = await resp.json()
    # Not every OAuth2 provider's userinfo response includes a "groups"
    # claim - GitHub's doesn't, since org/team membership isn't part of a
    # generic OIDC userinfo response there. Without a default,
    # is_admin()/is_qa_reviewer() would KeyError on every page for such a
    # user once admin_group or qa_reviewer_group is configured. Defaulting
    # to [] means those configs just never grant anyone admin/reviewer on
    # such a provider, instead of crashing.
    userinfo.setdefault("groups", [])
    session_id = str(uuid.uuid4())
    async with request.app.database.acquire() as conn:
        await conn.execute(
            """
INSERT INTO site_session (id, userinfo) VALUES ($1, $2)
""",
            session_id,
            userinfo,
        )

    # TODO(jelmer): Store access token / refresh token?

    callback_path = request.app.router["oauth2-callback"].url_for()
    resp = web.HTTPFound(back_url)

    resp.del_cookie("state", path=callback_path)
    resp.del_cookie("back_url")
    resp.set_cookie("session_id", session_id, secure=True, httponly=True)
    return resp


async def discover_openid_config(app, oauth2_provider_base_url):
    url = URL(oauth2_provider_base_url).join(URL("/.well-known/openid-configuration"))
    async with app["http_client_session"].get(url) as resp:
        if resp.status != 200:
            # TODO(jelmer): Fail? Set flag?
            logging.warning(
                "Unable to find openid configuration (%s): %s",
                resp.status,
                await resp.read(),
            )
            return
        app["openid_config"] = await resp.json()


async def handle_logout(request):
    session_id = request.cookies.get("session_id")
    back_url = "/"
    if "url" in request.query:
        target = _sanitize_redirect(request.query["url"])
        if target is not None:
            back_url = target
    if session_id is not None:
        async with request.app.database.acquire() as conn:
            await conn.execute("DELETE FROM site_session WHERE id = $1", session_id)
    resp = web.HTTPFound(back_url)
    resp.del_cookie("session_id")
    return resp


async def handle_login(request):
    state = str(uuid.uuid4())
    callback_path = request.app.router["oauth2-callback"].url_for()
    if not request.app["openid_config"]:
        raise web.HTTPNotFound(text="login is disabled on this instance")
    location = URL(request.app["openid_config"]["authorization_endpoint"]).with_query(
        {
            "client_id": request.app["config"].oauth2_provider.client_id
            or os.environ["OAUTH2_CLIENT_ID"],
            "redirect_uri": str(request.app["external_url"].join(callback_path)),
            "response_type": "code",
            "scope": "openid email profile",
            "state": state,
        }
    )
    response = web.HTTPFound(location)
    response.set_cookie(
        "state", state, max_age=60, path=callback_path, httponly=True, secure=True
    )
    if "url" in request.query:
        target = _sanitize_redirect(request.query["url"])
        if target is None:
            raise web.HTTPBadRequest(text="invalid url")
        response.set_cookie("back_url", target)
    return response


def setup_openid(app, oauth2_provider_base_url: Optional[str]):
    app.middlewares.insert(0, openid_middleware)
    app.router.add_get("/login", handle_login, name="login")
    app.router.add_get("/logout", handle_logout, name="logout")
    app.router.add_get("/oauth/callback", handle_oauth_callback, name="oauth2-callback")
    app["openid_config"] = None
    if oauth2_provider_base_url:
        app.on_startup.append(
            lambda app: discover_openid_config(app, oauth2_provider_base_url)
        )
