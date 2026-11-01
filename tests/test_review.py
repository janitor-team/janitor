from datetime import timedelta

import aiohttp_jinja2
import aiozipkin
from aiohttp import web
from bs4 import BeautifulSoup
from jinja2 import Environment
from yarl import URL

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.review import VERDICTS
from janitor.site import (
    TEMPLATE_ENV,
    classify_result_code,
    format_duration,
    format_timestamp,
    template_loader,
    worker_link_is_global,
)
from janitor.site.api import create_app as create_site_api_app
from janitor.site.cupboard import REVIEW_FORM_VERDICTS, create_app

from .test_cupboard import _insert_codebase, _insert_run, _user_middleware


async def create_runner_url(aiohttp_client, runner_posts):
    runner_app = web.Application()

    async def handle_run(request):
        runner_posts.append((request.match_info["run_id"], await request.json()))
        return web.json_response({})

    runner_app.router.add_post("/runs/{run_id}", handle_run)
    runner_client = await aiohttp_client(runner_app)
    return str(runner_client.make_url("/"))


REVIEWER = {"email": "reviewer@example.com", "name": "Reviewer", "groups": []}


async def create_review_client(aiohttp_client, db, campaigns=(), runner_url=None):
    config = "oauth2_provider {}\n" + "".join(
        f'campaign {{ name: "{campaign}" }}\n' for campaign in campaigns
    )
    app = create_app(
        config=read_config_string(config),
        publisher_url=None,
        runner_url=runner_url,
        differ_url=None,
        db=db,
    )
    app["external_url"] = URL("http://example.com/")
    aiohttp_jinja2.get_env(app).globals.update(TEMPLATE_ENV)
    app.middlewares.insert(0, _user_middleware(REVIEWER))
    endpoint = aiozipkin.create_endpoint("janitor.site", ipv4="127.0.0.1", port=80)
    aiozipkin.setup(app, await aiozipkin.create_custom(endpoint))
    return await aiohttp_client(app)


async def insert_review_run(
    conn,
    run_id,
    *,
    codebase="foo",
    campaign="mycampaign",
    mode="build-only",
    value=1,
    publish_status="needs-manual-review",
):
    await conn.execute(
        "INSERT INTO named_publish_policy (name, per_branch_policy) VALUES "
        "($1, ARRAY[ROW('main', $2, NULL)::branch_publish_policy]) "
        "ON CONFLICT DO NOTHING",
        mode,
        mode,
    )
    await _insert_codebase(conn, codebase)
    await conn.execute(
        "INSERT INTO candidate (suite, command, codebase, publish_policy) "
        "VALUES ($1, 'true', $2, $3)",
        campaign,
        codebase,
        mode,
    )
    now = utcnow()
    await _insert_run(
        conn,
        run_id=run_id,
        codebase=codebase,
        campaign=campaign,
        start_time=now - timedelta(minutes=30),
        finish_time=now,
    )
    # No VCS is set up here, so the evaluate page would fail on vcs_managers["git"].
    await conn.execute(
        "UPDATE run SET vcs_type = NULL, value = $2, publish_status = $3 WHERE id = $1",
        run_id,
        value,
        publish_status,
    )
    await conn.execute(
        "INSERT INTO new_result_branch (run_id, role, remote_name, base_revision, "
        "revision) VALUES ($1, 'main', 'main', 'base', 'revid')",
        run_id,
    )


def shown_run(text):
    soup = BeautifulSoup(text, "html.parser")
    return [field["value"] for field in soup.find_all("input", id="run-id")]


async def test_review_post_publishable_only_unchecked(aiohttp_client, db):
    client = await create_review_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await insert_review_run(conn, "somerun")

    resp = await client.get("/cupboard/review?required_only=false")
    assert shown_run(await resp.text()) == []

    resp = await client.post("/cupboard/review", data={"publishable_only": "true"})
    assert resp.status == 200
    assert shown_run(await resp.text()) == []

    resp = await client.post("/cupboard/review", data={"run_id": "somerun"})
    assert resp.status == 200
    assert shown_run(await resp.text()) == ["somerun"]


async def create_run_api_client(aiohttp_client, db, runner_url):
    app = create_site_api_app(
        publisher_url=None,
        runner_url=runner_url,
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
    runner_url = await create_runner_url(aiohttp_client, runner_posts)
    client = await create_run_api_client(aiohttp_client, db, runner_url)
    async with db.acquire() as conn:
        await insert_review_run(conn, "somerun")

    resp = await client.post("/run/somerun", data={"verdict": "approved"})
    assert resp.status == 200
    for verdict in ("unreviewed", "bogus"):
        resp = await client.post("/run/somerun", data={"verdict": verdict})
        assert (resp.status, await resp.text()) == (400, f"unknown verdict: {verdict}")

    async with db.acquire() as conn:
        reviews = await conn.fetch("SELECT run_id, reviewer, verdict FROM review")
    assert [tuple(r) for r in reviews] == [
        ("somerun", "reviewer@example.com", "approved")
    ]
    assert runner_posts == [("somerun", {"publish_status": "approved"})]


async def test_review_post_unknown_verdict(aiohttp_client, db):
    client = await create_review_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await insert_review_run(conn, "somerun")

    resp = await client.post(
        "/cupboard/review", data={"run_id": "somerun", "verdict": "Bogus"}
    )
    assert (resp.status, await resp.text()) == (400, "unknown verdict: Bogus")
    async with db.acquire() as conn:
        assert await conn.fetch("SELECT run_id FROM review") == []


def test_review_form_verdicts_known():
    assert set(REVIEW_FORM_VERDICTS.values()) - set(VERDICTS) == set()


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
    (option,) = BeautifulSoup(html, "html.parser").find_all(
        "option", value="unreviewed"
    )
    assert (option.get("disabled"), option.get("selected")) == ("", "true")
