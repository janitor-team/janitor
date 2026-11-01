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

import json
from datetime import timedelta

import aiozipkin
import pytest
from aiohttp import web
from aiohttp.test_utils import make_mocked_request

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.site.api import create_app, forward_request

from .conftest import _insert_codebase, _insert_run


def _user_middleware(user):
    @web.middleware
    async def middleware(request, handler):
        request["user"] = user
        return await handler(request)

    return middleware


async def create_client(
    aiohttp_client, db, *, runner_url=None, publisher_url=None, user=None, config=""
):
    config = read_config_string(config)
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


async def test_active_runs_and_runner_status(aiohttp_client, db):
    def entry(run_id, link):
        return {
            "id": run_id,
            "worker": run_id,
            "worker_link": link,
            "backchannel": {"my_url": link},
        }

    no_backchannel: dict = {
        "id": "none",
        "worker": "none",
        "worker_link": None,
        "backchannel": {},
    }
    runner_app = web.Application()

    async def _handle_status(request):
        return web.json_response(
            {
                "processing": [
                    entry("public", "https://worker.example.com/"),
                    entry("private", "http://192.168.1.5:8080/"),
                    no_backchannel,
                ],
                "avoid_hosts": ["example.org"],
                "rate_limit_hosts": {},
            }
        )

    runner_app.router.add_get("/status", _handle_status)
    runner_client = await aiohttp_client(runner_app)

    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    processing = [
        entry("public", "https://worker.example.com/"),
        entry("private", None),
        no_backchannel,
    ]

    resp = await client.get("/runner/status")
    assert resp.status == 200
    assert await resp.json() == {
        "processing": processing,
        "avoid_hosts": ["example.org"],
        "rate_limit_hosts": {},
    }

    resp = await client.get("/active-runs")
    assert resp.status == 200
    assert await resp.json() == processing

    resp = await client.get("/active-runs/private")
    assert resp.status == 200
    assert await resp.json() == processing[1]

    resp = await client.get("/active-runs/unknown")
    assert resp.status == 404
    assert await resp.json() == {}


async def test_codebase_names(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)

    resp = await client.get("/codebase-names")
    assert resp.status == 200
    assert await resp.json() == []

    async with db.acquire() as conn:
        for name in ["sl", "pv", "asciijump"]:
            await _insert_codebase(conn, name)
        await conn.execute(
            "INSERT INTO codebase (name, branch_url, url, inactive) "
            "VALUES ('hello', 'https://example.com/hello.git', "
            "'https://example.com/hello.git', true)"
        )
        await conn.execute(
            "INSERT INTO codebase (branch_url, url) VALUES "
            "('https://example.com/unnamed.git', 'https://example.com/unnamed.git')"
        )

    resp = await client.get("/codebase-names")
    assert resp.status == 200
    assert resp.headers["Cache-Control"] == "max-age=600"
    assert await resp.json() == ["asciijump", "pv", "sl"]


async def test_publish_mode_needs_admin(aiohttp_client, db):
    publisher_calls: list[str] = []
    publisher_app = web.Application()

    async def _handle_publish(request):
        publisher_calls.append(request.path)
        return web.json_response({}, status=202)

    publisher_app.router.add_post("/{campaign}/{codebase}/publish", _handle_publish)
    publisher_client = await aiohttp_client(publisher_app)

    client = await create_client(
        aiohttp_client,
        db,
        publisher_url=str(publisher_client.make_url("/")),
        user={"email": "alice@example.com", "groups": ["users"]},
        config='oauth2_provider { client_id: "site" admin_group: "admins" }',
    )

    resp = await client.post("/lintian-fixes/c/foo/publish", data={"mode": "push"})
    assert resp.status == 401
    assert publisher_calls == []


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


@pytest.mark.parametrize(
    "status,body", [(200, {"id": "1"}), (404, {"reason": "No such publish"})]
)
async def test_forward_request_returns_backend_response(aiohttp_client, status, body):
    backend_app = web.Application()

    async def _handle(request):
        return web.json_response(body, status=status)

    backend_app.router.add_get("/publish/1", _handle)
    backend_client = await aiohttp_client(backend_app)

    request = make_mocked_request(
        "GET", "/", app={"http_client_session": backend_client.session}
    )
    resp = await forward_request(
        request, "GET", backend_client.make_url("/publish/1"), "publisher"
    )
    assert resp.status == status
    assert resp.content_type == "application/json"
    assert json.loads(resp.body) == body


ADMIN_CONFIG = 'oauth2_provider { client_id: "site" admin_group: "admins" }'
ADMIN_USER = {"email": "admin@example.com", "groups": ["admins"]}


async def test_merge_proposal_change_forwards_publisher_error(aiohttp_client, db):
    seen = []
    publisher_app = web.Application()

    async def _handle_change(request):
        seen.append(dict(await request.post()))
        return web.json_response({"reason": "No such merge proposal"}, status=404)

    publisher_app.router.add_post("/merge-proposal", _handle_change)
    publisher_client = await aiohttp_client(publisher_app)

    client = await create_client(
        aiohttp_client,
        db,
        publisher_url=str(publisher_client.make_url("/")),
        user=ADMIN_USER,
        config=ADMIN_CONFIG,
    )

    resp = await client.post(
        "/merge-proposal",
        data={"url": "https://example.com/merge/1", "status": "closed"},
    )
    assert resp.status == 404
    assert await resp.json() == {"reason": "No such merge proposal"}
    assert seen == [{"url": "https://example.com/merge/1", "status": "closed"}]


async def test_runner_kill_forwards_runner_error(aiohttp_client, db):
    runner_app = web.Application()

    async def _handle_kill(request):
        return web.json_response({"reason": "No such current run: run-1"}, status=404)

    runner_app.router.add_post("/kill/{run_id}", _handle_kill)
    runner_client = await aiohttp_client(runner_app)

    client = await create_client(
        aiohttp_client,
        db,
        runner_url=str(runner_client.make_url("/")),
        user=ADMIN_USER,
        config=ADMIN_CONFIG,
    )

    resp = await client.post("/active-runs/run-1/kill")
    assert resp.status == 404
    assert await resp.json() == {"reason": "No such current run: run-1"}


async def test_admin_action_without_oauth2_provider_is_unauthorized(aiohttp_client, db):
    client = await create_client(
        aiohttp_client, db, user={"email": "alice@example.com", "groups": []}
    )

    resp = await client.post(
        "/merge-proposal",
        data={"url": "https://example.com/merge/1", "status": "closed"},
    )
    assert resp.status == 401


async def test_review_without_oauth2_provider_is_not_qa_review(aiohttp_client, db):
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        now = utcnow()
        await _insert_run(
            conn,
            run_id="r1",
            codebase="foo",
            start_time=now - timedelta(hours=1),
            finish_time=now,
        )
    # runner_url is None: a QA review would fail to tell the runner.
    client = await create_client(
        aiohttp_client, db, user={"email": "alice@example.com", "groups": []}
    )

    resp = await client.post("/run/r1", data={"verdict": "approved"})
    assert resp.status == 200
    assert await resp.json() == {"verdict": "approved", "review-comment": None}
    async with db.acquire() as conn:
        rows = await conn.fetch("SELECT run_id, reviewer, verdict FROM review")
    assert [tuple(row) for row in rows] == [("r1", "alice@example.com", "approved")]
