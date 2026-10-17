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
import pytest
from aiohttp import web

from janitor.config import read_string as read_config_string
from janitor.site.api import create_app


def _user_middleware(user):
    @web.middleware
    async def middleware(request, handler):
        request["user"] = user
        return await handler(request)

    return middleware


async def create_client(
    aiohttp_client, db, *, runner_url=None, publisher_url=None, user=None
):
    config = read_config_string("")
    app = create_app(
        publisher_url=publisher_url,
        runner_url=runner_url,
        vcs_managers={},
        differ_url=None,
        config=config,
        db=db,
    )
    app.middlewares.insert(0, _user_middleware(user))
    # In production this app is mounted as a subapp of janitor.site.simple,
    # which is what calls aiozipkin.setup() - do the same here since these
    # handlers use aiozipkin.request_span(request).
    endpoint = aiozipkin.create_endpoint("janitor.site", ipv4="127.0.0.1", port=80)
    tracer = await aiozipkin.create_custom(endpoint)
    aiozipkin.setup(app, tracer)
    return await aiohttp_client(app)


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


@pytest.mark.parametrize("limit", ["abc", "", "1.5"])
async def test_handle_queue_bad_limit(aiohttp_client, db, limit):
    runner_calls = []

    runner_app = web.Application()

    async def _handle_queue(request):
        runner_calls.append(dict(request.query))
        return web.json_response([])

    runner_app.router.add_get("/queue", _handle_queue)
    runner_client = await aiohttp_client(runner_app)

    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    resp = await client.get("/queue", params={"limit": limit})
    assert resp.status == 400
    assert await resp.text() == "limit must be an integer"
    assert runner_calls == []


async def _publisher_client(aiohttp_client, proposals, seen_paths):
    publisher_app = web.Application()

    async def _handle_list(request):
        seen_paths.append(request.path)
        return web.json_response(proposals)

    publisher_app.router.add_get("/merge-proposals", _handle_list)
    publisher_app.router.add_get("/{campaign}/merge-proposals", _handle_list)
    publisher_app.router.add_get("/c/{codebase}/merge-proposals", _handle_list)
    return await aiohttp_client(publisher_app)


async def test_merge_proposal_list_returns_publisher_body(aiohttp_client, db):
    proposals = [{"url": "https://example.com/merge/1", "status": "open"}]
    seen_paths: list[str] = []
    publisher_client = await _publisher_client(aiohttp_client, proposals, seen_paths)

    client = await create_client(
        aiohttp_client, db, publisher_url=str(publisher_client.make_url("/"))
    )

    resp = await client.get("/merge-proposals")
    assert resp.status == 200
    assert await resp.json() == proposals
    assert seen_paths == ["/merge-proposals"]


async def test_campaign_merge_proposal_list_returns_publisher_body(aiohttp_client, db):
    proposals = [{"url": "https://example.com/merge/2", "status": "merged"}]
    seen_paths: list[str] = []
    publisher_client = await _publisher_client(aiohttp_client, proposals, seen_paths)

    client = await create_client(
        aiohttp_client, db, publisher_url=str(publisher_client.make_url("/"))
    )

    resp = await client.get("/lintian-fixes/merge-proposals")
    assert resp.status == 200
    assert await resp.json() == proposals
    assert seen_paths == ["/lintian-fixes/merge-proposals"]


async def test_codebase_merge_proposal_list_returns_publisher_body(aiohttp_client, db):
    proposals = [{"url": "https://example.com/merge/3", "status": "closed"}]
    seen_paths: list[str] = []
    publisher_client = await _publisher_client(aiohttp_client, proposals, seen_paths)

    client = await create_client(
        aiohttp_client, db, publisher_url=str(publisher_client.make_url("/"))
    )

    resp = await client.get("/c/example/merge-proposals")
    assert resp.status == 200
    assert await resp.json() == proposals
    assert seen_paths == ["/c/example/merge-proposals"]


async def test_reschedule_passes_through_runner_error(aiohttp_client, db):
    runner_app = web.Application()

    async def _handle_schedule(request):
        return web.json_response({"reason": "Run not found"}, status=404)

    runner_app.router.add_post("/schedule", _handle_schedule)
    runner_client = await aiohttp_client(runner_app)

    client = await create_client(
        aiohttp_client,
        db,
        runner_url=str(runner_client.make_url("/")),
        user={"email": "alice@example.com", "groups": []},
    )

    resp = await client.post("/run/nonexistent/reschedule")
    assert resp.status == 404
    assert await resp.json() == {"reason": "Run not found"}


async def test_reschedule_returns_runner_result(aiohttp_client, db):
    runner_app = web.Application()

    async def _handle_schedule(request):
        body = await request.json()
        assert body["run_id"] == "1"
        return web.json_response(
            {
                "codebase": "foo",
                "campaign": "mycampaign",
                "offset": 0,
                "estimated_duration_seconds": 10,
                "queue_position": 1,
                "queue_wait_time": 5,
            }
        )

    runner_app.router.add_post("/schedule", _handle_schedule)
    runner_client = await aiohttp_client(runner_app)

    client = await create_client(
        aiohttp_client,
        db,
        runner_url=str(runner_client.make_url("/")),
        user={"email": "alice@example.com", "groups": []},
    )

    resp = await client.post("/run/1/reschedule")
    assert resp.status == 200
    body = await resp.json()
    assert body["codebase"] == "foo"
    assert body["campaign"] == "mycampaign"
