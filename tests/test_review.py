from datetime import timedelta

import aiozipkin
from aiohttp import web
from bs4 import BeautifulSoup
from jinja2 import Environment

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.site import (
    classify_result_code,
    format_duration,
    format_timestamp,
    template_loader,
    worker_link_is_global,
)
from janitor.site.api import create_app

from .test_cupboard import _insert_codebase, _insert_run, _user_middleware

REVIEWER = {"email": "reviewer@example.com", "name": "Reviewer", "groups": []}


async def create_run_api_client(aiohttp_client, db, runner_posts):
    runner_app = web.Application()

    async def _handle_run(request):
        runner_posts.append((request.match_info["run_id"], await request.json()))
        return web.json_response({})

    runner_app.router.add_post("/runs/{run_id}", _handle_run)
    runner_client = await aiohttp_client(runner_app)

    app = create_app(
        publisher_url=None,
        runner_url=str(runner_client.make_url("/")),
        vcs_managers={},
        differ_url=None,
        config=read_config_string("oauth2_provider {}\n"),
        db=db,
    )
    app.middlewares.insert(0, _user_middleware(REVIEWER))
    endpoint = aiozipkin.create_endpoint("janitor.site", ipv4="127.0.0.1", port=80)
    aiozipkin.setup(app, await aiozipkin.create_custom(endpoint))
    return await aiohttp_client(app)


async def test_run_post_unknown_verdict(aiohttp_client, db):
    runner_posts: list[tuple[str, dict]] = []
    client = await create_run_api_client(aiohttp_client, db, runner_posts)
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

    resp = await client.post("/run/somerun", data={"verdict": "approved"})
    assert resp.status == 200
    for verdict in ("unreviewed", "bogus"):
        resp = await client.post("/run/somerun", data={"verdict": verdict})
        assert resp.status == 400
        assert await resp.text() == f"unknown verdict: {verdict}"

    async with db.acquire() as conn:
        reviews = await conn.fetch("SELECT run_id, reviewer, verdict FROM review")
    assert [tuple(r) for r in reviews] == [
        ("somerun", "reviewer@example.com", "approved")
    ]
    assert runner_posts == [("somerun", {"publish_status": "approved"})]


def test_render_run_unreviewed_option_disabled():
    env = Environment(loader=template_loader)
    config = read_config_string('campaign {\n  name: "some-fixes"\n}\n')
    html = env.get_template("cupboard/run.html").render(
        worker_link_is_global=worker_link_is_global,
        run={
            "start_time": utcnow(),
            "finish_time": utcnow(),
            "result_code": "success",
        },
        success_probability=0.2,
        classify_result_code=classify_result_code,
        show_debdiff=lambda: "",
        campaign=config.campaign[0],
        config=config,
        publish_blockers=dict,
        format_timestamp=format_timestamp,
        format_duration=format_duration,
        is_qa_reviewer=True,
        user=REVIEWER,
        reviews=[],
    )
    option = BeautifulSoup(html, "html.parser").find("option", value="")
    assert option is not None
    assert "disabled" in option.attrs
    assert option["selected"] == "true"
    assert option.get_text(strip=True) == "Unreviewed"
