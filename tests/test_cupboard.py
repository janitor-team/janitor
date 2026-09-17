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

from datetime import datetime, timedelta

import pytest
from aiohttp import web
from jinja2 import Environment
from yarl import URL

from janitor.config import read_string as read_config_string
from janitor.runner import store_change_set, store_run
from janitor.site import (
    classify_result_code,
    format_duration,
    format_timestamp,
    template_loader,
    worker_link_is_global,
)
from janitor.site.cupboard import create_app
from janitor.site.cupboard.api import create_app as create_api_app


@web.middleware
async def dummy_user_middleware(request, handler):
    request["user"] = None
    resp = await handler(request)
    return resp


def _user_middleware(user):
    @web.middleware
    async def middleware(request, handler):
        request["user"] = user
        return await handler(request)

    return middleware


async def create_client(aiohttp_client, db):
    config = read_config_string("")
    app = create_app(
        config=config, publisher_url=None, runner_url=None, differ_url=None, db=db
    )
    app["external_url"] = URL("http://example.com/")
    app.middlewares.insert(0, dummy_user_middleware)
    return await aiohttp_client(app)


async def create_api_client(aiohttp_client, db, *, user):
    config = read_config_string("oauth2_provider {}\n")
    app = create_api_app(config=config, publisher_url=None, runner_url=None, db=db)
    app.middlewares.insert(0, _user_middleware(user))
    return await aiohttp_client(app)


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
            "start_time": datetime.utcnow(),
            "finish_time": datetime.utcnow(),
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


async def test_history(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/cupboard/history")
    assert resp.status == 200


async def test_queue(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/cupboard/queue")
    assert resp.status == 200


async def test_publish_history(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/cupboard/publish")
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


async def _insert_codebase(conn, name):
    await conn.execute(
        "INSERT INTO codebase (name, branch_url, url) VALUES ($1, $2, $2)",
        name,
        f"https://example.com/{name}.git",
    )


async def _insert_run(
    conn, *, run_id, codebase, campaign="mycampaign", start_time, finish_time
):
    await store_change_set(conn, run_id, campaign=campaign)
    await store_run(
        conn,
        run_id=run_id,
        codebase=codebase,
        campaign=campaign,
        vcs_type="git",
        subpath="",
        start_time=start_time,
        finish_time=finish_time,
        command="true",
        result_code="success",
        codemod_result={},
        main_branch_revision=b"revid",
        revision=b"revid",
        description=None,
        context=None,
        instigated_context=None,
        logfilenames=[],
        value=1,
        change_set=run_id,
        worker_name=None,
        branch_url=f"https://example.com/{codebase}.git",
    )


async def test_codebase_redirect_to_latest_run(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        now = datetime.utcnow()
        await _insert_run(
            conn,
            run_id="older",
            codebase="foo",
            start_time=now - timedelta(hours=2),
            finish_time=now - timedelta(hours=1),
        )
        await _insert_run(
            conn,
            run_id="newer",
            codebase="foo",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
        )

    resp = await client.get("/cupboard/c/foo/", allow_redirects=False)
    assert resp.status == 302
    assert resp.headers["Location"] == "/cupboard/c/foo/newer/"


async def test_codebase_redirect_prefers_finished_run(aiohttp_client, db):
    # finish_time DESC NULLS LAST: a finished run wins over an in-progress one
    # (finish_time IS NULL) that started later
    client = await create_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        now = datetime.utcnow()
        await _insert_run(
            conn,
            run_id="done",
            codebase="foo",
            start_time=now - timedelta(hours=1),
            finish_time=now - timedelta(minutes=30),
        )
        await _insert_run(
            conn,
            run_id="running",
            codebase="foo",
            start_time=now,
            finish_time=None,
        )

    resp = await client.get("/cupboard/c/foo/", allow_redirects=False)
    assert resp.status == 302
    assert resp.headers["Location"] == "/cupboard/c/foo/done/"


async def test_codebase_redirect_no_runs_returns_404(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")

    resp = await client.get("/cupboard/c/foo/", allow_redirects=False)
    assert resp.status == 404


async def test_codebase_redirect_unknown_codebase_returns_404(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/cupboard/c/nonexistent/", allow_redirects=False)
    assert resp.status == 404


async def test_run_redirect(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        now = datetime.utcnow()
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


async def test_evaluate_unknown_run_returns_404(db):
    from janitor.site.cupboard.review import generate_evaluate

    with pytest.raises(web.HTTPNotFound):
        await generate_evaluate(db, {}, None, None, "nonexistent", None)


ADMIN_USER = {"email": "admin@example.com", "groups": []}


async def test_workers_list_requires_admin(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=None)
    resp = await client.get("/workers")
    assert resp.status == 401


async def test_workers_list_empty(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    resp = await client.get("/workers")
    assert resp.status == 200
    assert await resp.json() == []


async def test_workers_list(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    async with db.acquire() as conn:
        await conn.execute(
            "INSERT INTO worker (name, password, link) VALUES "
            "($1, 'x', $2), ($3, 'x', $4)",
            "alice",
            "https://public.example.com/",
            "bob",
            "http://10.0.0.1/",
        )
    resp = await client.get("/workers")
    assert resp.status == 200
    body = await resp.json()
    assert body == [
        {"name": "alice", "link": "https://public.example.com/", "run_count": 0},
        {"name": "bob", "link": None, "run_count": 0},
    ]


async def test_workers_list_counts_runs(aiohttp_client, db):
    client = await create_api_client(aiohttp_client, db, user=ADMIN_USER)
    async with db.acquire() as conn:
        await conn.execute(
            "INSERT INTO worker (name, password) VALUES ($1, 'x')", "alice"
        )
        await _insert_codebase(conn, "foo")
        now = datetime.utcnow()
        await _insert_run(
            conn,
            run_id="r1",
            codebase="foo",
            start_time=now - timedelta(hours=1),
            finish_time=now,
        )
        await conn.execute("UPDATE run SET worker = 'alice' WHERE id = 'r1'")

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
