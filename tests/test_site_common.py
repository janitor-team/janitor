#!/usr/bin/python
# Copyright (C) 2026 Jelmer Vernooij <jelmer@jelmer.uk>
#
# This program is free software; you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation; either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.

from datetime import timedelta

import aiozipkin
from aiohttp import web

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.site.common import generate_codebase_context, get_unchanged_run

from .conftest import _insert_codebase, _insert_run


async def test_get_unchanged_run(con):
    await _insert_codebase(con, "foo")
    now = utcnow()
    await _insert_run(
        con,
        run_id="control-run",
        codebase="foo",
        campaign="control",
        start_time=now - timedelta(minutes=30),
        finish_time=now,
    )

    unchanged_run = await get_unchanged_run(con, "foo", b"revid")
    expected = {
        "id": "control-run",
        "codebase": "foo",
        "suite": "control",
        "result_code": "success",
        "revision": "revid",
    }
    row = dict(unchanged_run)
    assert {key: row.get(key) for key in expected} == expected


async def test_generate_codebase_context(aiohttp_client, db, con):
    await _insert_codebase(con, "foo")
    await _insert_codebase(con, "bar")
    now = utcnow()
    for i in range(2):
        await _insert_run(
            con,
            run_id=f"run-{i}",
            codebase="foo",
            start_time=now - timedelta(hours=2 - i),
            finish_time=now - timedelta(hours=1 - i),
        )
    await con.execute(
        "INSERT INTO candidate (codebase, suite, command, context, value, "
        "success_chance) VALUES ('foo', 'mycampaign', 'true', 'ctx', 10, 0.5)"
    )
    await con.execute(
        "INSERT INTO review (run_id, comment, reviewer, verdict) "
        "VALUES ('run-1', 'fine', 'alice', 'approved')"
    )
    config = read_config_string('campaign { name: "mycampaign" }')
    endpoint = aiozipkin.create_endpoint("janitor.site", ipv4="127.0.0.1", port=80)
    tracer = await aiozipkin.create_custom(endpoint)

    seen_queue_queries = []
    seen_runs_queries = []

    async def handle_queue_position(request):
        seen_queue_queries.append(dict(request.query))
        return web.json_response(
            {"position": 2, "wait_time": 300.0, "cumulative_wait_time": 600.0}
        )

    async def handle_run(request):
        run_id = request.match_info["run_id"]
        if run_id != "run-1":
            return web.json_response({"reason": "no such run"}, status=404)
        return web.json_response(
            {
                "id": "run-1",
                "codebase": "foo",
                "campaign": "mycampaign",
                "publish_status": "unknown",
                "result_code": "success",
                "main_branch_revision": None,
                "revision": None,
                "vcs_type": "git",
                "branch_url": "https://example.com/foo.git",
                "target_branch_url": None,
                "start_time": (now - timedelta(hours=1)).isoformat(),
                "finish_time": now.isoformat(),
                "command": "",
                "description": None,
                "context": None,
                "result": None,
                "instigated_context": None,
                "logfilenames": [],
                "worker_name": None,
                "worker_link": None,
                "change_set": "run-1",
                "failure_stage": None,
                "failure_transient": None,
                "resume_from": None,
                "result_tags": None,
                "result_branches": [],
                "debian_build": None,
            }
        )

    async def handle_runs(request):
        seen_runs_queries.append(dict(request.query))
        # Hand back the two seeded runs in finish_time-desc order, as
        # the real runner would.
        return web.json_response(
            {
                "runs": [
                    {
                        "id": "run-1",
                        "codebase": "foo",
                        "campaign": "mycampaign",
                        "result_code": "success",
                        "start_time": (now - timedelta(hours=1)).isoformat(),
                        "finish_time": now.isoformat(),
                        "duration_seconds": 3600,
                        "description": None,
                        "worker_name": None,
                        "worker_link": None,
                        "failure_transient": None,
                        "revision": None,
                        "main_branch_revision": None,
                    },
                    {
                        "id": "run-0",
                        "codebase": "foo",
                        "campaign": "mycampaign",
                        "result_code": "success",
                        "start_time": (now - timedelta(hours=2)).isoformat(),
                        "finish_time": (now - timedelta(hours=1)).isoformat(),
                        "duration_seconds": 3600,
                        "description": None,
                        "worker_name": None,
                        "worker_link": None,
                        "failure_transient": None,
                        "revision": None,
                        "main_branch_revision": None,
                    },
                ]
            }
        )

    runner_app = web.Application()
    runner_app.router.add_get("/queue/position", handle_queue_position)
    runner_app.router.add_get("/runs/{run_id}", handle_run)
    runner_app.router.add_get("/runs", handle_runs)
    runner_client = await aiohttp_client(runner_app)

    context = await generate_codebase_context(
        db,
        config,
        "mycampaign",
        runner_client.session,
        None,
        {},
        "foo",
        tracer.new_trace(),
        "run-1",
        runner_url=str(runner_client.make_url("/")),
    )

    assert context["candidate_context"] == "ctx"
    assert context["candidate_value"] == 10
    assert context["candidate_success_chance"] == 0.5
    assert [run["id"] for run in context["previous_runs"]] == ["run-1", "run-0"]
    assert context["queue_position"] == 2
    assert context["queue_wait_time"] == timedelta(minutes=5)
    assert [review["comment"] for review in context["reviews"]] == ["fine"]
    assert seen_queue_queries == [{"codebase": "foo", "campaign": "mycampaign"}]
    assert seen_runs_queries == [{"codebase": "foo", "campaign": "mycampaign"}]
