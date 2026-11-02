"""API tokens that scripts send as Bearer tokens."""

import hashlib
import secrets

import mimeparse
from aiohttp import web

from . import check_logged_in
from .common import render_template_for_request

TOKEN_DAYS = ["7", "30", "60", "90"]


def token_hash(token: str) -> str:
    return hashlib.sha256(token.encode()).hexdigest()


@web.middleware
async def api_token_middleware(request, handler):
    """Log in with an API token; added only to the API apps."""
    authorization = request.headers.get("Authorization")
    if (
        authorization is not None
        and authorization.startswith("Bearer ")
        and request["user"] is None
    ):
        async with request.app["pool"].acquire() as conn:
            row = await conn.fetchrow(
                "SELECT userinfo, expires_at < NOW() AS expired "
                "FROM site_api_token WHERE hash = $1",
                token_hash(authorization.removeprefix("Bearer ")),
            )
        if row is None:
            raise web.HTTPUnauthorized(text="invalid token")
        if row["expired"]:
            raise web.HTTPUnauthorized(text="token expired")
        request["user"] = row["userinfo"]
    return await handler(request)


async def handle_token_create(request):
    check_logged_in(request)
    form = await request.post()
    days = form.get("days", "30")
    if days not in TOKEN_DAYS:
        raise web.HTTPBadRequest(text=f"days must be one of {', '.join(TOKEN_DAYS)}")
    token = secrets.token_urlsafe(32)
    async with request.app["pool"].acquire() as conn:
        expires_at = await conn.fetchval(
            "INSERT INTO site_api_token (hash, owner, userinfo, expires_at) "
            "VALUES ($1, $2, $3, NOW() + make_interval(days => $4)) "
            "RETURNING expires_at",
            token_hash(token),
            request["user"]["sub"],
            request["user"],
            int(days),
        )
    text = await render_template_for_request(
        "api-token.html", request, {"token": token, "expires_at": expires_at}
    )
    return web.Response(
        content_type="text/html",
        text=text,
        status=201,
        headers={"Cache-Control": "no-store"},
    )


async def handle_token_list(request):
    check_logged_in(request)
    async with request.app["pool"].acquire() as conn:
        rows = await conn.fetch(
            "SELECT hash, created, expires_at FROM site_api_token "
            "WHERE owner = $1 AND expires_at > NOW() ORDER BY created",
            request["user"]["sub"],
        )
    accept = request.headers.get("Accept", "*/*")
    if mimeparse.best_match(["text/html", "application/json"], accept) == "text/html":
        text = await render_template_for_request(
            "api-tokens.html", request, {"tokens": rows}
        )
        return web.Response(content_type="text/html", text=text)
    return web.json_response(
        [
            {
                "hash": row["hash"],
                "created": row["created"].isoformat(),
                "expires_at": row["expires_at"].isoformat(),
            }
            for row in rows
        ]
    )


async def handle_token_delete(request):
    check_logged_in(request)
    async with request.app["pool"].acquire() as conn:
        status = await conn.execute(
            "DELETE FROM site_api_token WHERE hash = $1 AND owner = $2",
            request.match_info["hash"],
            request["user"]["sub"],
        )
    if status == "DELETE 0":
        raise web.HTTPNotFound(text="no such token")
    if request.method == "POST":
        raise web.HTTPSeeOther(request.app.router["api-token-list"].url_for())
    return web.Response(status=204)


def setup_api_tokens(app):
    app.router.add_get("/api-tokens", handle_token_list, name="api-token-list")
    app.router.add_post("/api-tokens", handle_token_create, name="api-token-create")
    app.router.add_delete(
        "/api-tokens/{hash}", handle_token_delete, name="api-token-delete"
    )
    app.router.add_post("/api-tokens/{hash}/delete", handle_token_delete)
