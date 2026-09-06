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

import aiozipkin
from aiohttp import web

from janitor.config import read_string as read_config_string
from janitor.site.api import create_app


@web.middleware
async def dummy_user_middleware(request, handler):
    request["user"] = None
    return await handler(request)


async def create_client(aiohttp_client, db, *, runner_url=None, publisher_url=None):
    config = read_config_string("")
    app = create_app(
        publisher_url=publisher_url,
        runner_url=runner_url,
        vcs_managers={},
        differ_url=None,
        config=config,
        db=db,
    )
    app.middlewares.insert(0, dummy_user_middleware)
    # In production this app is mounted as a subapp of janitor.site.simple,
    # which is what calls aiozipkin.setup() - do the same here since these
    # handlers use aiozipkin.request_span(request).
    endpoint = aiozipkin.create_endpoint("janitor.site", ipv4="127.0.0.1", port=80)
    tracer = await aiozipkin.create_custom(endpoint)
    aiozipkin.setup(app, tracer)
    return await aiohttp_client(app)


async def create_runner_client(aiohttp_client, handler):
    runner_app = web.Application()
    runner_app.router.add_post("/schedule", handler)
    return await aiohttp_client(runner_app)


async def test_handle_queue_forwards_limit(aiohttp_client, db):
    seen_query = {}

    runner_app = web.Application()

    async def _handle_queue(request):
        seen_query.update(request.query)
        return web.json_response([{"queue_id": 1, "codebase": "foo"}])

    runner_app.router.add_get("/queue", _handle_queue)
    runner_client = await aiohttp_client(runner_app)

    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    resp = await client.get("/queue?limit=5")
    assert resp.status == 200
    body = await resp.json()
    assert body == [{"queue_id": 1, "codebase": "foo"}]
    assert seen_query == {"limit": "5"}


async def test_handle_queue_without_limit(aiohttp_client, db):
    runner_app = web.Application()

    async def _handle_queue(request):
        assert dict(request.query) == {}
        return web.json_response([])

    runner_app.router.add_get("/queue", _handle_queue)
    runner_client = await aiohttp_client(runner_app)

    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    resp = await client.get("/queue")
    assert resp.status == 200
    assert await resp.json() == []


async def test_codebase_schedule_forwards_to_runner(aiohttp_client, db):
    async def handle_schedule(request):
        body = await request.json()
        assert body["campaign"] == "mycampaign"
        assert body["codebase"] == "foo"
        assert body["requester"] == "user from web UI"
        assert body["bucket"] == "manual"
        return web.json_response(
            {"campaign": "mycampaign", "codebase": "foo", "queue_position": 1}
        )

    runner_client = await create_runner_client(aiohttp_client, handle_schedule)
    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    resp = await client.post("/mycampaign/c/foo/schedule")
    assert resp.status == 200
    body = await resp.json()
    assert body == {"campaign": "mycampaign", "codebase": "foo", "queue_position": 1}


async def test_codebase_schedule_uses_authenticated_requester(aiohttp_client, db):
    async def handle_schedule(request):
        body = await request.json()
        assert body["requester"] == "alice@example.com"
        return web.json_response({"campaign": "mycampaign", "codebase": "foo"})

    runner_client = await create_runner_client(aiohttp_client, handle_schedule)
    config = read_config_string("")
    app = create_app(
        publisher_url=None,
        runner_url=str(runner_client.make_url("/")),
        vcs_managers={},
        differ_url=None,
        config=config,
        db=db,
    )

    @web.middleware
    async def user_middleware(request, handler):
        request["user"] = {"email": "alice@example.com", "groups": []}
        return await handler(request)

    app.middlewares.insert(0, user_middleware)
    client = await aiohttp_client(app)

    resp = await client.post("/mycampaign/c/foo/schedule")
    assert resp.status == 200


async def test_codebase_schedule_invalid_refresh_returns_400(aiohttp_client, db):
    async def handle_schedule(request):
        raise AssertionError("runner should not be contacted")

    runner_client = await create_runner_client(aiohttp_client, handle_schedule)
    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    resp = await client.post(
        "/mycampaign/c/foo/schedule", data={"refresh": "notabool"}
    )
    assert resp.status == 400
    assert await resp.json() == {"error": "invalid boolean for refresh"}


async def test_codebase_schedule_forwards_runner_error_status(aiohttp_client, db):
    async def handle_schedule(request):
        return web.json_response({"reason": "no such campaign"}, status=404)

    runner_client = await create_runner_client(aiohttp_client, handle_schedule)
    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    resp = await client.post("/mycampaign/c/foo/schedule")
    assert resp.status == 404
    assert await resp.json() == {"reason": "no such campaign"}


async def test_codebase_schedule_runner_returns_non_json_error(aiohttp_client, db):
    async def handle_schedule(request):
        return web.Response(status=400, text="bad request", content_type="text/plain")

    runner_client = await create_runner_client(aiohttp_client, handle_schedule)
    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    resp = await client.post("/mycampaign/c/foo/schedule")
    assert resp.status == 400
    body = await resp.json()
    assert body["error"] == "runner returned error 400"


async def test_codebase_schedule_runner_unreachable_returns_502(aiohttp_client, db):
    client = await create_client(
        aiohttp_client, db, runner_url="http://127.0.0.1:1/"
    )

    resp = await client.post("/mycampaign/c/foo/schedule")
    assert resp.status == 502
    assert await resp.json() == {"error": "unable to contact runner"}


async def test_schedule_control_forwards_runner_error_status(aiohttp_client, db):
    async def handle_schedule_control(request):
        return web.json_response({"reason": "run not found"}, status=404)

    runner_app = web.Application()
    runner_app.router.add_post("/schedule-control", handle_schedule_control)
    runner_client = await aiohttp_client(runner_app)
    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    resp = await client.post("/run/somerun/schedule-control")
    assert resp.status == 404
    assert await resp.json() == {"reason": "run not found"}
