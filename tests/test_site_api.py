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

import logging
from datetime import timedelta

import aiozipkin
import pytest
from aiohttp import web
from aiohttp.test_utils import unused_port

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.site.api import create_app
from janitor.vcs import RemoteGitVcsManager

from .conftest import _insert_codebase, _insert_run


def _user_middleware(user):
    @web.middleware
    async def middleware(request, handler):
        request["user"] = user
        return await handler(request)

    return middleware


async def create_client(
    aiohttp_client,
    db,
    *,
    runner_url=None,
    publisher_url=None,
    differ_url=None,
    user=None,
    config="",
    vcs_managers=None,
):
    config = read_config_string(config)
    app = create_app(
        publisher_url=publisher_url,
        runner_url=runner_url,
        vcs_managers=vcs_managers or {},
        differ_url=differ_url,
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
    async def handle_codebase_names(request):
        return web.json_response(["asciijump", "pv", "sl"])

    runner_app = web.Application()
    runner_app.router.add_get("/codebases/names", handle_codebase_names)
    runner_client = await aiohttp_client(runner_app)
    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    resp = await client.get("/codebase-names")
    assert (resp.status, resp.headers["Cache-Control"], await resp.json()) == (
        200,
        "max-age=600",
        ["asciijump", "pv", "sl"],
    )


async def test_codebase_names_runner_error(aiohttp_client, db):
    async def handle_codebase_names(request):
        return web.Response(status=500, text="Database error")

    runner_app = web.Application()
    runner_app.router.add_get("/codebases/names", handle_codebase_names)
    runner_client = await aiohttp_client(runner_app)
    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    resp = await client.get("/codebase-names")
    assert (resp.status, await resp.text()) == (
        502,
        "Unable to retrieve codebase names",
    )


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


async def _diff_store(aiohttp_server, status, text):
    store_app = web.Application()

    async def _handle_diff(request):
        return web.Response(status=status, text=text)

    store_app.router.add_get("/{codebase}/diff", _handle_diff)
    return await aiohttp_server(store_app)


async def _diff_runner(aiohttp_client):
    runner_app = web.Application()

    async def _handle_run(request):
        branch = {
            "role": "main",
            "base_revision": "git-v1:" + "1" * 40,
            "revision": "git-v1:" + "2" * 40,
        }
        return web.json_response(
            {"codebase": "foo", "vcs_type": "git", "result_branches": [branch]}
        )

    runner_app.router.add_get("/runs/r1", _handle_run)
    runner_client = await aiohttp_client(runner_app)
    return str(runner_client.make_url("/"))


@pytest.mark.parametrize(
    "store_status,store_text,status,text,logged",
    [
        (200, "diff --git a/x b/x\n", 200, "diff --git a/x b/x\n", []),
        (
            500,
            "git diff failed: fatal: bad object 1111",
            502,
            "Unable to retrieve diff",
            ["Unable to retrieve diff"],
        ),
    ],
    ids=["ok", "error-status"],
)
async def test_run_diff(
    aiohttp_client,
    aiohttp_server,
    db,
    caplog,
    store_status,
    store_text,
    status,
    text,
    logged,
):
    store = await _diff_store(aiohttp_server, store_status, store_text)
    client = await create_client(
        aiohttp_client,
        db,
        runner_url=await _diff_runner(aiohttp_client),
        vcs_managers={"git": RemoteGitVcsManager(str(store.make_url("/")))},
    )

    with caplog.at_level(logging.WARNING):
        resp = await client.get("/run/r1/diff", headers={"Accept": "text/plain"})
        assert (resp.status, await resp.text()) == (status, text)
    assert [m.partition(": ")[0] for m in caplog.messages] == logged


async def test_run_diff_store_unreachable(aiohttp_client, db):
    store_url = f"http://127.0.0.1:{unused_port()}/"
    client = await create_client(
        aiohttp_client,
        db,
        runner_url=await _diff_runner(aiohttp_client),
        vcs_managers={"git": RemoteGitVcsManager(store_url)},
    )

    resp = await client.get("/run/r1/diff", headers={"Accept": "text/plain"})
    assert (resp.status, await resp.text()) == (502, "Unable to retrieve diff")


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


async def test_run_logs(aiohttp_client, db):
    async def handle_log_index(request):
        if request.match_info["run_id"] != "r1":
            raise web.HTTPNotFound()
        return web.json_response(["build.log", "worker.log"])

    async def handle_log(request):
        key = (request.match_info["run_id"], request.match_info["filename"])
        responses = {
            ("r1", "worker.log"): web.Response(text="Opening branch\n"),
            ("r1", "build.log"): web.Response(status=500, text="storage unavailable"),
        }
        return responses.get(key, web.Response(status=404))

    runner_app = web.Application()
    runner_app.router.add_get("/log/{run_id}", handle_log_index)
    runner_app.router.add_get("/log/{run_id}/{filename}", handle_log)
    runner_client = await aiohttp_client(runner_app)
    client = await create_client(
        aiohttp_client, db, runner_url=str(runner_client.make_url("/"))
    )

    got = []
    for path in [
        "/run/r1/logs",
        "/run/r1/logs/worker.log",
        "/run/r1/logs/build.log",
        "/run/r1/logs/x.log",
        "/run/r2/logs",
        "/run/r2/logs/worker.log",
    ]:
        resp = await client.get(path)
        got.append((path, resp.status, resp.content_type, await resp.text()))
    assert got == [
        ("/run/r1/logs", 200, "application/json", '["build.log", "worker.log"]'),
        ("/run/r1/logs/worker.log", 200, "text/plain", "Opening branch\n"),
        (
            "/run/r1/logs/build.log",
            502,
            "text/plain",
            "Unable to read log file build.log for run r1",
        ),
        ("/run/r1/logs/x.log", 404, "text/plain", "No log file x.log for run r1"),
        ("/run/r2/logs", 404, "text/plain", "No such run: r2"),
        (
            "/run/r2/logs/worker.log",
            404,
            "text/plain",
            "No log file worker.log for run r2",
        ),
    ]


async def test_archive_diff(aiohttp_client, db):
    seen = []

    async def handle_debdiff(request):
        run_id = request.match_info["run_id"]
        seen.append((run_id, dict(request.query)))
        if run_id == "r1":
            return web.Response(text="debdiff output", content_type="text/plain")
        if run_id == "r2":
            return web.json_response(
                {
                    "reason": "unchanged-run-not-found",
                    "message": "no unchanged run for run r2",
                },
                status=404,
            )
        if run_id == "r3":
            return web.json_response(
                {
                    "reason": "run-not-successful",
                    "message": "run r3 was not successful",
                    "unavailable_run_id": "r3",
                },
                status=422,
                headers={"unavailable_run_id": "r3"},
            )
        return web.json_response(
            {"reason": "database-error", "message": "database error"}, status=500
        )

    differ_app = web.Application()
    differ_app.router.add_get("/debdiff/{run_id}", handle_debdiff)
    differ_client = await aiohttp_client(differ_app)
    client = await create_client(
        aiohttp_client, db, differ_url=str(differ_client.make_url("/"))
    )

    resp = await client.get("/run/r1/debdiff?filter_boring=1")
    assert (resp.status, resp.headers["Vary"], await resp.text()) == (
        200,
        "Accept",
        "debdiff output",
    )
    resp = await client.get("/run/r2/debdiff")
    assert (resp.status, await resp.json()) == (
        404,
        {
            "reason": "unchanged-run-not-found",
            "message": "no unchanged run for run r2",
            "unavailable_run_id": None,
        },
    )
    resp = await client.get("/run/r3/debdiff")
    assert (resp.status, await resp.text()) == (404, "Build r3 has no artifacts")
    resp = await client.get("/run/r4/debdiff")
    assert resp.status == 503
    assert seen == [
        ("r1", {"jquery_url": "/_static/jquery.js", "filter_boring": "yes"}),
        ("r2", {"jquery_url": "/_static/jquery.js"}),
        ("r3", {"jquery_url": "/_static/jquery.js"}),
        ("r4", {"jquery_url": "/_static/jquery.js"}),
    ]
