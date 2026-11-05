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

import sys
import types
from datetime import timedelta

import aiohttp_jinja2
import pytest
from aiohttp import web
from aiohttp.test_utils import make_mocked_request
from bs4 import BeautifulSoup
from jinja2 import Environment
from yarl import URL

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.logs import LogRetrievalError, get_log_manager
from janitor.queue import Queue
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
from janitor.site.cupboard.publish import iter_publish_history
from janitor.site.pkg import read_log

from .conftest import _insert_codebase, _insert_run


def _user_middleware(user):
    @web.middleware
    async def middleware(request, handler):
        request["user"] = user
        return await handler(request)

    return middleware


async def create_client(aiohttp_client, db, *, user=None, config=""):
    config = read_config_string(config)
    app = create_app(
        config=config, publisher_url=None, runner_url=None, differ_url=None, db=db
    )
    app["external_url"] = URL("http://example.com/")
    # create_app leaves the jinja globals to its caller, and the production
    # caller is py/janitor/site/simple.py.
    aiohttp_jinja2.get_env(app).globals.update(TEMPLATE_ENV)
    app.middlewares.insert(0, _user_middleware(user))
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


def _codebases_and_page_links(html, rows, column):
    soup = BeautifulSoup(html, "html.parser")
    codebases = [
        tr.find_all("td")[column].get_text(strip=True) for tr in soup.select(rows)
    ]
    links = {
        a.get_text(): a["href"]
        for a in soup.find_all("a")
        if a.get_text() in ("Newer runs", "Older runs", "Previous page", "Next page")
    }
    return codebases, links


async def test_history_pages(aiohttp_client, db):
    async with db.acquire() as conn:
        now = utcnow()
        for i in range(5):
            await _insert_codebase(conn, f"cb{i}")
            await _insert_run(
                conn,
                run_id=f"run-{i}",
                codebase=f"cb{i}",
                start_time=now - timedelta(hours=i + 1),
                finish_time=now - timedelta(hours=i),
            )
    client = await create_client(aiohttp_client, db)

    async def get(path):
        resp = await client.get(path)
        assert resp.status == 200
        return _codebases_and_page_links(await resp.text(), "#history tbody tr", 1)

    assert await get("/cupboard/history") == (
        ["cb0", "cb1", "cb2", "cb3", "cb4"],
        {},
    )
    assert await get("/cupboard/history?limit=2") == (
        ["cb0", "cb1"],
        {"Older runs": "/cupboard/history?limit=2&offset=2"},
    )
    assert await get("/cupboard/history?limit=2&offset=2") == (
        ["cb2", "cb3"],
        {
            "Newer runs": "/cupboard/history?limit=2",
            "Older runs": "/cupboard/history?limit=2&offset=4",
        },
    )
    assert await get("/cupboard/history?offset=3&limit=2") == (
        ["cb3", "cb4"],
        {
            "Newer runs": "/cupboard/history?offset=1&limit=2",
            "Older runs": "/cupboard/history?offset=5&limit=2",
        },
    )
    assert await get("/cupboard/history?limit=2&offset=4") == (
        ["cb4"],
        {"Newer runs": "/cupboard/history?limit=2&offset=2"},
    )
    assert await get("/cupboard/history?limit=0&offset=2") == ([], {})
    resp = await client.get("/cupboard/history?limit=2&offset=2")
    assert "Runs 3 to 4:" in await resp.text()


async def test_history_pages_keep_runs_with_the_same_finish_time(aiohttp_client, db):
    async with db.acquire() as conn:
        now = utcnow()
        for name in ["z", "y"]:
            await _insert_codebase(conn, f"cb{name}")
            await _insert_run(
                conn,
                run_id=f"run-{name}",
                codebase=f"cb{name}",
                start_time=now - timedelta(hours=1),
                finish_time=now,
            )
    client = await create_client(aiohttp_client, db)
    pages = []
    for offset in [0, 1]:
        resp = await client.get(f"/cupboard/history?limit=1&offset={offset}")
        assert resp.status == 200
        pages.append(
            _codebases_and_page_links(await resp.text(), "#history tbody tr", 1)[0]
        )
    assert pages == [["cby"], ["cbz"]]


HISTORY_CONFIG = """\
campaign {
  name: "alpha"
}
campaign {
  name: "beta"
}
"""


async def _history_client(aiohttp_client, db):
    async with db.acquire() as conn:
        await conn.execute(
            "INSERT INTO worker (name, password) VALUES ('w1', 'x'), ('w2', 'x')"
        )
        now = utcnow()
        for i, (campaign, result_code, worker, transient, minutes) in enumerate(
            [
                ("alpha", "success", "w1", None, 10),
                ("beta", "build-failed", "w2", False, 30),
                ("alpha", "worker-timeout", "w1", True, 20),
                ("beta", "success", "w2", None, 40),
                ("alpha", "nothing-to-do", "w1", None, 50),
                ("beta", "nothing-new-to-do", "w2", None, 5),
            ]
        ):
            await _insert_codebase(conn, f"cb{i}")
            await _insert_run(
                conn,
                run_id=f"run-{i}",
                codebase=f"cb{i}",
                campaign=campaign,
                start_time=now - timedelta(hours=i, minutes=minutes),
                finish_time=now - timedelta(hours=i),
            )
            # nothing-new-to-do needs the run it resumed from
            await conn.execute(
                "UPDATE run SET worker = $1, failure_transient = $2, result_code = $3, "
                "resume_from = CASE WHEN $3 = 'nothing-new-to-do' THEN 'run-0' END "
                "WHERE id = $4",
                worker,
                transient,
                result_code,
                f"run-{i}",
            )
    return await create_client(aiohttp_client, db, config=HISTORY_CONFIG)


async def test_history_filters(aiohttp_client, db):
    client = await _history_client(aiohttp_client, db)

    async def codebases(query):
        resp = await client.get("/cupboard/history?" + query)
        assert resp.status == 200, await resp.text()
        return _codebases_and_page_links(await resp.text(), "#history tbody tr", 1)[0]

    everything = ["cb0", "cb1", "cb2", "cb3", "cb4", "cb5"]
    assert await codebases("") == everything
    assert await codebases("campaign=alpha") == ["cb0", "cb2", "cb4"]
    assert await codebases("campaign=gamma") == []
    assert await codebases("campaign=X%20Y") == []
    assert await codebases("result_code=success") == ["cb0", "cb3"]
    assert await codebases("codebase=cb1") == ["cb1"]
    assert await codebases("codebase=it%27s") == []
    assert await codebases("worker=w2") == ["cb1", "cb3", "cb5"]
    assert await codebases("transient=true") == ["cb2"]
    assert await codebases("transient=false") == ["cb0", "cb1", "cb3", "cb4", "cb5"]
    assert await codebases("hide_success=true") == ["cb1", "cb2"]
    assert await codebases("hide_success=false") == everything
    assert await codebases("hide_success=true&campaign=beta") == ["cb1"]
    empty = "campaign=&result_code=&codebase=&worker=&transient=&hide_success="
    assert await codebases(empty) == everything


async def test_history_sort(aiohttp_client, db):
    client = await _history_client(aiohttp_client, db)

    async def codebases(query):
        resp = await client.get("/cupboard/history?" + query)
        assert resp.status == 200, await resp.text()
        return _codebases_and_page_links(await resp.text(), "#history tbody tr", 1)[0]

    newest = ["cb0", "cb1", "cb2", "cb3", "cb4", "cb5"]
    assert await codebases("sort=finish_time") == newest
    assert await codebases("sort=finish_time&order=asc") == newest[::-1]
    assert await codebases("sort=codebase&order=desc") == newest[::-1]
    by_campaign = ["cb0", "cb2", "cb4", "cb1", "cb3", "cb5"]
    assert await codebases("sort=campaign") == by_campaign
    assert await codebases("sort=campaign&order=desc") == [
        "cb1",
        "cb3",
        "cb5",
        "cb0",
        "cb2",
        "cb4",
    ]
    by_duration = ["cb4", "cb3", "cb1", "cb2", "cb0", "cb5"]
    assert await codebases("sort=duration") == by_duration
    assert await codebases("sort=duration&order=asc") == by_duration[::-1]
    by_result = ["cb1", "cb5", "cb4", "cb0", "cb3", "cb2"]
    assert await codebases("sort=result_code") == by_result
    async with db.acquire() as conn:
        await conn.execute("UPDATE run SET codebase = 'cb0' WHERE id = 'run-5'")
    by_codebase = ["cb0", "cb0", "cb1", "cb2", "cb3", "cb4"]
    assert await codebases("sort=codebase") == by_codebase
    async with db.acquire() as conn:
        await conn.execute(
            "UPDATE run SET finish_time = finish_time + interval '4 hours' "
            "WHERE id = 'run-3'"
        )
    assert await codebases("sort=result_code") == [
        "cb1",
        "cb0",
        "cb4",
        "cb3",
        "cb0",
        "cb2",
    ]


async def test_history_links_keep_parameters(aiohttp_client, db):
    client = await _history_client(aiohttp_client, db)
    resp = await client.get(
        "/cupboard/history?hide_success=true&sort=duration&limit=1&offset=1"
    )
    assert resp.status == 200
    soup = BeautifulSoup(await resp.text(), "html.parser")
    headers = {a.get_text(): a["href"] for a in soup.select("#history thead th a")}
    assert headers == {
        "Finish time": "/cupboard/history?hide_success=true&sort=finish_time&limit=1&order=desc",
        "Codebase": "/cupboard/history?hide_success=true&sort=codebase&limit=1&order=asc",
        "Suite": "/cupboard/history?hide_success=true&sort=campaign&limit=1&order=asc",
        "Duration": "/cupboard/history?hide_success=true&sort=duration&limit=1&order=asc",
        "Result": "/cupboard/history?hide_success=true&sort=result_code&limit=1&order=asc",
    }
    duration = soup.select("#history thead th")[4]
    assert duration.get_text(" ", strip=True) == "Duration \u25bc"
    arrows = soup.select("#history thead")[0].get_text()
    assert (arrows.count("\u25b2"), arrows.count("\u25bc")) == (0, 1)
    assert _codebases_and_page_links(str(soup), "#history tbody tr", 1) == (
        ["cb2"],
        {
            "Previous page": "/cupboard/history?hide_success=true&sort=duration&limit=1",
            "Next page": "/cupboard/history?hide_success=true&sort=duration&limit=1&offset=2",
        },
    )
    hidden = soup.select("#history-filters input[type=hidden]")
    assert {i["name"]: i["value"] for i in hidden} == {"sort": "duration", "limit": "1"}
    assert soup.select("#filter-hide-success[checked]")
    links = {a.get_text(): a["href"] for a in soup.find_all("a")}
    assert links["Clear filters"] == "/cupboard/history?sort=duration&limit=1"

    assert "most recent first" not in soup.get_text()

    resp = await client.get("/cupboard/history?limit=1&campaign=&worker=w2&codebase=")
    soup = BeautifulSoup(await resp.text(), "html.parser")
    assert "most recent first" in soup.get_text()
    links = {a.get_text(): a["href"] for a in soup.find_all("a")}
    assert links["Older runs"] == "/cupboard/history?limit=1&worker=w2&offset=1"
    assert links["Codebase"] == (
        "/cupboard/history?limit=1&worker=w2&sort=codebase&order=asc"
    )
    resp = await client.get("/cupboard/history?limit=1&offset=1&campaign=&worker=w2")
    soup = BeautifulSoup(await resp.text(), "html.parser")
    links = {a.get_text(): a["href"] for a in soup.find_all("a")}
    assert links["Newer runs"] == "/cupboard/history?limit=1&worker=w2"

    resp = await client.get("/cupboard/history?sort=finish_time&order=asc&limit=1")
    soup = BeautifulSoup(await resp.text(), "html.parser")
    assert "most recent first" not in soup.get_text()
    finish_time = soup.select("#history thead th")[0]
    assert finish_time.get_text(" ", strip=True) == "Finish time \u25b2"
    hidden = soup.select("#history-filters input[type=hidden]")
    assert {i["name"]: i["value"] for i in hidden} == {
        "sort": "finish_time",
        "order": "asc",
        "limit": "1",
    }
    assert _codebases_and_page_links(str(soup), "#history tbody tr", 1)[1] == {
        "Next page": "/cupboard/history?sort=finish_time&order=asc&limit=1&offset=1"
    }


async def test_history_form_shows_the_filters(aiohttp_client, db):
    client = await _history_client(aiohttp_client, db)
    resp = await client.get(
        "/cupboard/history?campaign=beta&codebase=%22it%27s%22&transient=false"
    )
    assert resp.status == 200
    soup = BeautifulSoup(await resp.text(), "html.parser")
    assert [o["value"] for o in soup.select("#filter-campaign option[selected]")] == [
        "beta"
    ]
    assert [o["value"] for o in soup.select("#filter-transient option[selected]")] == [
        "false"
    ]
    assert [i["value"] for i in soup.select("#filter-codebase")] == ['"it\'s"']
    assert not soup.select("#filter-hide-success[checked]")

    resp = await client.get(
        "/cupboard/history?result_code=build-failed&worker=w2&transient=true"
    )
    soup = BeautifulSoup(await resp.text(), "html.parser")
    assert [i["value"] for i in soup.select("#filter-result-code")] == ["build-failed"]
    assert [i["value"] for i in soup.select("#filter-worker")] == ["w2"]
    assert [o["value"] for o in soup.select("#filter-transient option[selected]")] == [
        "true"
    ]

    resp = await client.get("/cupboard/history?campaign=gamma")
    soup = BeautifulSoup(await resp.text(), "html.parser")
    assert [o["value"] for o in soup.select("#filter-campaign option[selected]")] == [
        "gamma"
    ]


async def test_history_bad_parameters(aiohttp_client, db):
    client = await _history_client(aiohttp_client, db)
    answers = {}
    for query in [
        "sort=id",
        "sort=finish_time;DROP TABLE run",
        "sort=",
        "order=up",
        "transient=maybe",
        "hide_success=1",
    ]:
        resp = await client.get("/cupboard/history?" + query)
        answers[query] = (resp.status, await resp.text())
    sort_error = (
        400,
        "sort must be one of: finish_time, codebase, campaign, duration, result_code",
    )
    assert answers == {
        "sort=id": sort_error,
        "sort=finish_time;DROP TABLE run": sort_error,
        "sort=": sort_error,
        "order=up": (400, "order must be one of: asc, desc"),
        "transient=maybe": (400, "transient must be true or false"),
        "hide_success=1": (400, "hide_success must be true or false"),
    }


async def test_queue_pages(aiohttp_client, db):
    async with db.acquire() as conn:
        queue = Queue(conn)
        for name in ["cb0", "cb1", "cb2"]:
            await _insert_codebase(conn, name)
            await queue.add(codebase=name, campaign="mycampaign", command="true")
    client = await create_client(aiohttp_client, db)

    async def get(path):
        resp = await client.get(path)
        assert resp.status == 200
        return _codebases_and_page_links(
            await resp.text(), "div[name=coming-up] tbody tr", 0
        )

    assert await get("/cupboard/queue") == (["cb0", "cb1", "cb2"], {})
    assert await get("/cupboard/queue?limit=2") == (
        ["cb0", "cb1"],
        {"Next page": "/cupboard/queue?limit=2&offset=2"},
    )
    assert await get("/cupboard/queue?limit=2&offset=2") == (
        ["cb2"],
        {"Previous page": "/cupboard/queue?limit=2"},
    )
    assert await get("/cupboard/queue?limit=2&offset=1") == (
        ["cb1", "cb2"],
        {
            "Previous page": "/cupboard/queue?limit=2",
            "Next page": "/cupboard/queue?limit=2&offset=3",
        },
    )
    for bad in ["-1", "abc"]:
        resp = await client.get(f"/cupboard/queue?offset={bad}")
        assert resp.status == 400


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


ADMIN_GROUP_CONFIG = 'oauth2_provider { admin_group: "admins" }\n'


@pytest.mark.parametrize("user", [None, {"email": "user@example.com", "groups": []}])
async def test_reprocess_logs_requires_admin(aiohttp_client, db, user):
    client = await create_client(
        aiohttp_client, db, user=user, config=ADMIN_GROUP_CONFIG
    )
    resp = await client.get("/cupboard/reprocess-logs")
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
            "INSERT INTO candidate (codebase, suite, command) "
            "VALUES ('foo', 'lintian-fixes', 'true')"
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
    }
    assert [dict(c) for c in foo["candidates"]] == [{"campaign": "lintian-fixes"}]
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


async def test_codebase_unknown_returns_404(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/cupboard/c/nonexistent/", allow_redirects=False)
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
        now = utcnow()
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


async def test_publish_history_limit_zero_lists_nothing(con):
    await con.execute("INSERT INTO codebase (name) VALUES ('foo')")
    await con.execute(
        "INSERT INTO change_set (id, campaign) VALUES ('cs1', 'mycampaign')"
    )
    await con.execute(
        "INSERT INTO publish (id, change_set, target_branch_url, mode, "
        "result_code, codebase) VALUES "
        "('p1', 'cs1', 'https://example.com/foo.git', 'propose', 'success', 'foo'), "
        "('p2', 'cs1', 'https://example.com/foo.git', 'propose', 'success', 'foo')"
    )
    assert len(await iter_publish_history(con)) == 2
    assert len(await iter_publish_history(con, limit=1)) == 1
    assert await iter_publish_history(con, limit=0) == []


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
