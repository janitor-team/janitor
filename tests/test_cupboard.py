#!/usr/bin/python
# Copyright (C) 2019 Jelmer Vernooij <jelmer@jelmer.uk>
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

import re
import sys
import types
from datetime import datetime, timedelta, timezone

import aiohttp_jinja2
import pytest
from aiohttp import ClientSession, web
from aiohttp.test_utils import make_mocked_request
from jinja2 import Environment
from yarl import URL

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.logs import LogRetrievalError, get_log_manager
from janitor.site import (
    TEMPLATE_ENV,
    classify_result_code,
    format_duration,
    format_timestamp,
    template_loader,
    worker_link_is_global,
)
from janitor.site.cupboard import create_app, generate_codebase, handle_result_file
from janitor.site.cupboard.api import create_app as create_api_app
from janitor.site.cupboard.api import select_runs_to_reprocess
from janitor.site.pkg import read_log

from .conftest import _insert_codebase, _insert_run


def _user_middleware(user):
    @web.middleware
    async def middleware(request, handler):
        request["user"] = user
        return await handler(request)

    return middleware


async def create_client(
    aiohttp_client, db, *, user=None, config="", publisher_url=None
):
    config = read_config_string(config)
    app = create_app(
        config=config,
        publisher_url=publisher_url,
        runner_url=None,
        differ_url=None,
        db=db,
    )
    app["external_url"] = URL("http://example.com/")
    # create_app leaves the jinja globals to its caller, and the production
    # caller is py/janitor/site/simple.py.
    aiohttp_jinja2.get_env(app).globals.update(TEMPLATE_ENV)
    app.middlewares.insert(0, _user_middleware(user))
    return await aiohttp_client(app)


async def create_api_client(aiohttp_client, db, *, user, runner_url=None):
    config = read_config_string("oauth2_provider {}\n")
    app = create_api_app(
        config=config, publisher_url=None, runner_url=runner_url, db=db
    )
    app.middlewares.insert(0, _user_middleware(user))
    return await aiohttp_client(app)


async def _start_fake_runner(aiohttp_server, *, admin_workers):
    """In-process stand-in for the runner's `/admin/workers` endpoint."""

    async def handle_admin_workers(request):
        return web.json_response({"workers": admin_workers})

    app = web.Application()
    app.router.add_get("/admin/workers", handle_admin_workers)
    server = await aiohttp_server(app)
    return str(server.make_url("/"))


async def _start_fake_publisher(aiohttp_server, *, publishes):
    """In-process stand-in for the publisher's `/publish` list endpoint."""

    async def handle_publish(request):
        mp_url = request.query.get("merge_proposal_url")
        limit = request.query.get("limit")
        rows = publishes
        if mp_url is not None:
            rows = [p for p in rows if p.get("merge_proposal_url") == mp_url]
        if limit is not None:
            rows = rows[: int(limit)]
        return web.json_response({"publishes": rows})

    app = web.Application()
    app.router.add_get("/publish", handle_publish)
    server = await aiohttp_server(app)
    return str(server.make_url("/"))


def test_render_merge_proposal():
    env = Environment(loader=template_loader)
    template = env.get_template("cupboard/merge-proposal.html")
    template.render(
        proposal={
            "url": "https://github.com/jelmer/example/pulls/1",
            "package": "zz",
        }
    )


def test_render_run():
    env = Environment(loader=template_loader)
    template = env.get_template("cupboard/run.html")
    config = read_config_string("""\
campaign {
  name: "some-fixes"
}
""")
    campaign = config.campaign[0]
    template.render(
        worker_link_is_global=worker_link_is_global,
        run={
            "start_time": utcnow(),
            "finish_time": utcnow(),
        },
        success_probability=0.2,
        classify_result_code=classify_result_code,
        show_debdiff=lambda: "",
        campaign=campaign,
        config=config,
        publish_blockers=dict,
        format_timestamp=format_timestamp,
        format_duration=format_duration,
    )


def test_classify_result_code_debian_validate():
    assert (
        classify_result_code("apt-repository-setup-failure", None)
        == "transient-failure"
    )
    assert (
        classify_result_code("vcs-up-to-date-check-failed", None) == "transient-failure"
    )
    assert classify_result_code("missing-dependency", None) == "bug"


async def test_history(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/cupboard/history")
    assert resp.status == 200


async def test_queue(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/cupboard/queue")
    assert resp.status == 200


async def test_publish_history(aiohttp_client, aiohttp_server, db):
    publisher_url = await _start_fake_publisher(aiohttp_server, publishes=[])
    client = await create_client(aiohttp_client, db, publisher_url=publisher_url)
    resp = await client.get("/cupboard/publish")
    assert resp.status == 200


ADMIN_GROUP_CONFIG = 'oauth2_provider { admin_group: "admins" }\n'


@pytest.mark.parametrize("user", [None, {"email": "user@example.com", "groups": []}])
async def test_reprocess_logs_requires_admin(aiohttp_client, db, user):
    client = await create_client(
        aiohttp_client, db, user=user, config=ADMIN_GROUP_CONFIG
    )
    resp = await client.get(
        "/cupboard/reprocess-logs",
        headers={"Sec-Fetch-Mode": "navigate"},
        allow_redirects=False,
    )
    if user is None:
        page = client.make_url("/cupboard/reprocess-logs")
        assert resp.status == 302
        assert resp.headers["Location"] == f"/login?url={page}"
    else:
        assert resp.status == 401
    resp = await client.get("/cupboard/reprocess-logs", allow_redirects=False)
    assert resp.status == 401


async def test_reprocess_logs_admin(aiohttp_client, db):
    client = await create_client(
        aiohttp_client,
        db,
        user={"email": "admin@example.com", "groups": ["admins"]},
        config=ADMIN_GROUP_CONFIG,
    )
    resp = await client.get("/cupboard/reprocess-logs")
    assert resp.status == 200


def test_render_changeset():
    env = Environment(loader=template_loader)
    template = env.get_template("cupboard/changeset.html")
    template.render(changeset="changeset", url=URL("https://example/com"), URL=URL)


def test_render_broken_mps():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/broken-merge-proposals.html")


def test_render_changeset_list():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/changeset-list.html")


def test_render_rejected():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/rejected.html")


def test_workers():
    env = Environment(loader=template_loader)
    template = env.get_template("cupboard/workers.html")
    template.render(worker_link_is_global=worker_link_is_global)


def test_start():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/start.html")


def test_reprocess_logs():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/reprocess-logs.html")


def test_never_processed():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/never-processed.html")


def test_result_code_index():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/result-code-index.html")


def test_result_code():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/result-code.html")


def test_failure_stage():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/failure-stage-index.html")


def test_publish():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/publish.html")


def test_run():
    env = Environment(loader=template_loader)
    env.get_template("cupboard/run.html")


async def test_codebase(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_codebase(conn, "bar")
        await conn.execute(
            "UPDATE codebase SET branch = 'main', vcs_type = 'git', value = 20 "
            "WHERE name = 'foo'"
        )
        await conn.execute(
            "INSERT INTO named_publish_policy (name, per_branch_policy, rate_limit_bucket) "
            "VALUES ('lintian', ARRAY[ROW('main', 'propose', NULL)::branch_publish_policy], "
            "'default')"
        )
        await conn.execute(
            "INSERT INTO candidate (codebase, suite, command, value, publish_policy) "
            "VALUES ('foo', 'lintian-fixes', 'true', 30, 'lintian')"
        )
        now = utcnow()
        await _insert_run(
            conn,
            run_id="older",
            codebase="foo",
            campaign="unchanged",
            start_time=now - timedelta(hours=2),
            finish_time=now - timedelta(hours=1),
        )
        await _insert_run(
            conn,
            run_id="newer",
            codebase="foo",
            campaign="lintian-fixes",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
            result_code="nothing-to-do",
        )
        foo = await generate_codebase(conn, "foo")
        bar = await generate_codebase(conn, "bar")

    assert dict(foo["codebase"]) == {
        "name": "foo",
        "url": "https://example.com/foo.git",
        "web_url": None,
        "branch": "main",
        "subpath": None,
        "vcs_type": "git",
        "vcs_last_revision": None,
        "last_scanned": None,
        "value": 20,
        "inactive": False,
    }
    assert [dict(c) for c in foo["candidates"]] == [
        {
            "campaign": "lintian-fixes",
            "command": "true",
            "context": None,
            "value": 30,
            "success_chance": None,
            "publish_policy": "lintian",
            "per_branch_policy": [("main", "propose", None)],
            "rate_limit_bucket": "default",
        }
    ]
    assert [dict(r) for r in foo["runs"]] == [
        {
            "id": "newer",
            "campaign": "lintian-fixes",
            "finish_time": now,
            "result_code": "nothing-to-do",
            "failure_transient": None,
        },
        {
            "id": "older",
            "campaign": "unchanged",
            "finish_time": now - timedelta(hours=1),
            "result_code": "success",
            "failure_transient": None,
        },
    ]
    assert (bar["candidates"], bar["runs"]) == ([], [])

    resp = await client.get("/cupboard/c/foo/", allow_redirects=False)
    assert resp.status == 200

    resp = await client.get(
        "/cupboard/c/", params={"codebase": "foo"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/cupboard/c/foo/"


async def test_codebase_unknown_returns_404(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/cupboard/c/nonexistent/", allow_redirects=False)
    assert resp.status == 404

    resp = await client.get("/cupboard/c/", allow_redirects=False)
    assert resp.status == 404


async def test_run_redirect(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        now = utcnow()
        await _insert_run(
            conn,
            run_id="somerun",
            codebase="foo",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
        )

    resp = await client.get("/cupboard/run/somerun/", allow_redirects=False)
    assert resp.status == 308
    assert resp.headers["Location"] == "/cupboard/c/foo/somerun/"


async def test_run_redirect_unknown_run_returns_404(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/cupboard/run/nonexistent/", allow_redirects=False)
    assert resp.status == 404


@pytest.mark.parametrize(
    "error,expected",
    [
        (FileNotFoundError("Log not found"), None),
        (LogRetrievalError("timed out"), b"timed out"),
        (PermissionError("Permission denied"), b"Permission denied"),
    ],
)
async def test_read_log(error, expected):
    class LogFileManager:
        async def get_log(self, codebase, run_id, name):
            raise error

    assert await read_log(LogFileManager(), "foo", "somerun", "worker.log") == expected


async def test_evaluate_unknown_run_returns_404(db):
    from janitor.site.cupboard.review import generate_evaluate

    with pytest.raises(web.HTTPNotFound):
        await generate_evaluate(db, {}, None, None, "nonexistent", None)


ADMIN_USER = {"email": "admin@example.com", "groups": []}


async def test_workers_list_requires_admin(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=None)
    resp = await client.get("/workers")
    assert resp.status == 401


async def test_workers_list_empty(aiohttp_client, aiohttp_server, db):
    runner_url = await _start_fake_runner(aiohttp_server, admin_workers=[])
    client = await create_api_client(
        aiohttp_client, db, user=ADMIN_USER, runner_url=runner_url
    )
    resp = await client.get("/workers")
    assert resp.status == 200
    assert await resp.json() == []


async def test_workers_list(aiohttp_client, aiohttp_server, db):
    runner_url = await _start_fake_runner(
        aiohttp_server,
        admin_workers=[
            {"name": "alice", "link": "https://public.example.com/", "run_count": 0},
            {"name": "bob", "link": "http://10.0.0.1/", "run_count": 0},
        ],
    )
    client = await create_api_client(
        aiohttp_client, db, user=ADMIN_USER, runner_url=runner_url
    )
    resp = await client.get("/workers")
    assert resp.status == 200
    body = await resp.json()
    assert body == [
        {"name": "alice", "link": "https://public.example.com/", "run_count": 0},
        {"name": "bob", "link": None, "run_count": 0},
    ]


async def test_workers_list_counts_runs(aiohttp_client, aiohttp_server, db):
    runner_url = await _start_fake_runner(
        aiohttp_server,
        admin_workers=[{"name": "alice", "link": None, "run_count": 1}],
    )
    client = await create_api_client(
        aiohttp_client, db, user=ADMIN_USER, runner_url=runner_url
    )
    resp = await client.get("/workers")
    assert resp.status == 200
    assert await resp.json() == [{"name": "alice", "link": None, "run_count": 1}]


async def test_workers_add_requires_admin(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=None)
    resp = await client.post("/workers", data={"name": "alice"})
    assert resp.status == 401


async def test_workers_add_requires_name(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    resp = await client.post("/workers", data={})
    assert resp.status == 400


async def test_workers_add(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    resp = await client.post(
        "/workers", data={"name": "alice", "link": "https://alice.example.com/"}
    )
    assert resp.status == 201
    body = await resp.json()
    assert body["name"] == "alice"
    assert body["link"] == "https://alice.example.com/"
    assert isinstance(body["password"], str) and body["password"]

    async with db.acquire() as conn:
        row = await conn.fetchrow(
            "SELECT name, link, password FROM worker WHERE name = 'alice'"
        )
    assert row["name"] == "alice"
    assert row["link"] == "https://alice.example.com/"
    # password is stored hashed, not in plaintext
    assert row["password"] != body["password"]


async def test_workers_add_without_link(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    resp = await client.post("/workers", data={"name": "alice"})
    assert resp.status == 201
    body = await resp.json()
    assert body["link"] is None

    async with db.acquire() as conn:
        row = await conn.fetchrow("SELECT link FROM worker WHERE name = 'alice'")
    assert row["link"] is None


async def test_workers_add_duplicate(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    async with db.acquire() as conn:
        await conn.execute("INSERT INTO worker (name, password) VALUES ('alice', 'x')")
    resp = await client.post("/workers", data={"name": "alice"})
    assert resp.status == 409


async def test_workers_delete_requires_admin(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=None)
    resp = await client.delete("/workers/alice")
    assert resp.status == 401


async def test_workers_delete(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    async with db.acquire() as conn:
        await conn.execute("INSERT INTO worker (name, password) VALUES ('alice', 'x')")
    resp = await client.delete("/workers/alice")
    assert resp.status == 200
    assert await resp.json() == {"name": "alice"}

    async with db.acquire() as conn:
        count = await conn.fetchval("SELECT count(*) FROM worker WHERE name = 'alice'")
    assert count == 0


async def test_workers_delete_unknown(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    resp = await client.delete("/workers/nonexistent")
    assert resp.status == 404


async def test_history_limit_zero_lists_no_runs(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        now = utcnow()
        await _insert_run(
            conn,
            run_id="somerun",
            codebase="foo",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
        )

    resp = await client.get("/cupboard/history")
    assert resp.status == 200
    assert "somerun" in await resp.text()

    resp = await client.get("/cupboard/history?limit=0")
    assert resp.status == 200
    assert "somerun" not in await resp.text()


async def test_write_history_shapes_publisher_rows_for_the_template(aiohttp_server):
    from janitor.site.cupboard.publish import write_history

    publishes = [
        {
            "id": "p-recent",
            "timestamp": "2026-02-01T12:00:00+00:00",
            "codebase": "foo",
            "codebase_web_url": "https://browse.invalid/foo",
            "branch_name": "fix-1",
            "mode": "propose",
            "merge_proposal_url": "https://forge.invalid/foo/pulls/1",
            "result_code": "success",
            "description": "ok",
            "revision": "rev-1",
            "requester": None,
        },
        {
            "id": "p-older",
            "timestamp": "2026-01-01T12:00:00+00:00",
            "codebase": "foo",
            "codebase_web_url": "https://browse.invalid/foo",
            "branch_name": None,
            "mode": "push",
            "merge_proposal_url": None,
            "result_code": "publish-failed",
            "description": None,
            "revision": "rev-0",
            "requester": None,
        },
    ]
    publisher_url = await _start_fake_publisher(aiohttp_server, publishes=publishes)

    async with ClientSession() as client:
        result = await write_history(client, publisher_url, limit=100)

    expected_rows = [
        (
            "p-recent",
            datetime(2026, 2, 1, 12, 0, tzinfo=timezone.utc),
            "fix-1",
            "propose",
            "https://forge.invalid/foo/pulls/1",
            "success",
            "ok",
            "https://browse.invalid/foo",
            "foo",
        ),
        (
            "p-older",
            datetime(2026, 1, 1, 12, 0, tzinfo=timezone.utc),
            None,
            "push",
            None,
            "publish-failed",
            None,
            "https://browse.invalid/foo",
            "foo",
        ),
    ]
    assert result == {"count": 100, "history": expected_rows}


async def test_write_history_passes_limit_through_to_publisher(aiohttp_server):
    from janitor.site.cupboard.publish import write_history

    captured = {}

    async def handle_publish(request):
        captured["limit"] = request.query.get("limit")
        return web.json_response({"publishes": []})

    app = web.Application()
    app.router.add_get("/publish", handle_publish)
    server = await aiohttp_server(app)

    async with ClientSession() as client:
        result = await write_history(client, str(server.make_url("/")), limit=7)

    assert captured == {"limit": "7"}
    assert result == {"count": 7, "history": []}


@pytest.fixture
def reprocess_logs_stub(monkeypatch):
    # The handler imports this module on each request and only hands the rows on.
    async def reprocess_run_logs(**kwargs):
        return None

    stub = types.ModuleType("janitor.reprocess_logs")
    vars(stub).update(
        reprocess_run_logs=reprocess_run_logs,
        process_dist_log=None,
        process_sbuild_log=None,
    )
    monkeypatch.setitem(sys.modules, "janitor.reprocess_logs", stub)


async def test_reprocess_logs_bulk_filters_by_campaign(
    aiohttp_client, db, reprocess_logs_stub
):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_codebase(conn, "bar")
        now = utcnow()
        await _insert_run(
            conn,
            run_id="matching",
            codebase="foo",
            campaign="lintian-fixes",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
            result_code="build-failed",
        )
        await _insert_run(
            conn,
            run_id="other-campaign",
            codebase="bar",
            campaign="multiarch-hints",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
            result_code="build-failed",
        )

    resp = await client.post("/reprocess-logs", data={"campaign": "lintian-fixes"})
    assert resp.status == 200
    body = await resp.json()
    assert [row["log_id"] for row in body] == ["matching"]


async def test_reprocess_logs_bulk_filters_by_description_re(
    aiohttp_client, db, reprocess_logs_stub
):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_codebase(conn, "bar")
        now = utcnow()
        await _insert_run(
            conn,
            run_id="matching",
            codebase="foo",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
            result_code="build-failed",
            description="sbuild timed out",
        )
        await _insert_run(
            conn,
            run_id="non-matching",
            codebase="bar",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
            result_code="build-failed",
            description="missing build dependency",
        )

    resp = await client.post("/reprocess-logs", data={"description_re": "timed out"})
    assert resp.status == 200
    body = await resp.json()
    assert [row["log_id"] for row in body] == ["matching"]


async def test_reprocess_logs_bulk_filters_by_min_age(
    aiohttp_client, db, reprocess_logs_stub
):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_codebase(conn, "bar")
        now = utcnow()
        await _insert_run(
            conn,
            run_id="old-enough",
            codebase="foo",
            start_time=now - timedelta(days=10, minutes=30),
            finish_time=now - timedelta(days=10),
            result_code="build-failed",
        )
        await _insert_run(
            conn,
            run_id="too-recent",
            codebase="bar",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
            result_code="build-failed",
        )

    resp = await client.post("/reprocess-logs", data={"min_age": "5"})
    assert resp.status == 200
    body = await resp.json()
    assert [row["log_id"] for row in body] == ["old-enough"]


async def test_reprocess_logs_bulk_invalid_regex(
    aiohttp_client, db, reprocess_logs_stub
):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    resp = await client.post("/reprocess-logs", data={"description_re": "("})
    assert resp.status == 400


async def test_reprocess_logs_bulk_selects_every_unpack_code(db):
    async with db.acquire() as conn:
        now = utcnow()
        for run_id, result_code in [
            ("patch", "unpack-patch-application-failed"),
            ("upstream", "unpack-unexpected-local-upstream-changes"),
            ("other", "success"),
        ]:
            await _insert_codebase(conn, run_id)
            await _insert_run(
                conn,
                run_id=run_id,
                codebase=run_id,
                start_time=now - timedelta(minutes=30),
                finish_time=now,
                result_code=result_code,
            )
        rows = await select_runs_to_reprocess(conn)

    assert sorted(row["id"] for row in rows) == ["patch", "upstream"]


async def test_result_file_unreadable_log_returns_502(tmp_path):
    log = tmp_path / "pkg" / "run-id" / "build.log"
    log.parent.mkdir(parents=True)
    log.write_text("log")
    log.chmod(0)
    app = web.Application()
    app["logfile_manager"] = get_log_manager(str(tmp_path))
    request = make_mocked_request(
        "GET",
        "/",
        app=app,
        match_info={"codebase": "pkg", "run_id": "run-id", "filename": "build.log"},
    )
    with pytest.raises(web.HTTPBadGateway) as e:
        await handle_result_file(request)
    assert str(e.value.__cause__) == "Unable to read build.log"


async def test_api_swagger_documents_all_routes(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=None)
    resp = await client.get("/swagger.json")
    assert resp.status == 200
    spec = await resp.json()
    documented = {}
    for path, ops in spec["paths"].items():
        for method, op in ops.items():
            if method == "head" or not op["responses"]:
                continue
            documented[(path, method)] = {
                p["name"] for p in op["parameters"] if p["in"] == "path"
            }
    assert documented == {
        (route.resource.canonical, route.method.lower()): set(
            re.findall(r"\{(\w+)\}", route.resource.canonical)
        )
        for route in client.app.router.routes()
        if route.method != "HEAD" and not (route.name or "").startswith("swagger.")
    }
