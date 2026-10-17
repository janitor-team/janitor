from datetime import timedelta

import aiohttp_jinja2
import aiozipkin
from bs4 import BeautifulSoup
from yarl import URL

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.site import TEMPLATE_ENV
from janitor.site.cupboard import create_app

from .test_cupboard import _insert_codebase, _insert_run, _user_middleware

CONFIG = """\
oauth2_provider {}
campaign { name: "campaign-a" }
campaign { name: "campaign-b" }
"""

REVIEWER = {"email": "reviewer@example.com", "name": "Reviewer", "groups": []}


async def create_filter_client(aiohttp_client, db):
    app = create_app(
        config=read_config_string(CONFIG),
        publisher_url=None,
        runner_url=None,
        differ_url=None,
        db=db,
    )
    app["external_url"] = URL("http://example.com/")
    aiohttp_jinja2.get_env(app).globals.update(TEMPLATE_ENV)
    app.middlewares.insert(0, _user_middleware(REVIEWER))
    endpoint = aiozipkin.create_endpoint("janitor.site", ipv4="127.0.0.1", port=80)
    aiozipkin.setup(app, await aiozipkin.create_custom(endpoint))
    return await aiohttp_client(app)


async def insert_filter_run(conn, *, run_id, codebase, campaign, value):
    await conn.execute(
        "INSERT INTO named_publish_policy (name, per_branch_policy) VALUES "
        "($1, ARRAY[ROW('main', 'propose', NULL)::branch_publish_policy]) "
        "ON CONFLICT DO NOTHING",
        campaign,
    )
    await _insert_codebase(conn, codebase)
    await conn.execute(
        "INSERT INTO candidate (suite, command, codebase, publish_policy) "
        "VALUES ($1, 'true', $2, $3)",
        campaign,
        codebase,
        campaign,
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
    await conn.execute(
        "UPDATE run SET value = $2, vcs_type = NULL WHERE id = $1", run_id, value
    )
    await conn.execute(
        "INSERT INTO new_result_branch (run_id, role, remote_name, base_revision, "
        "revision) VALUES ($1, 'main', 'main', 'base', 'revid')",
        run_id,
    )


def shown_run(text):
    soup = BeautifulSoup(text, "html.parser")
    return [field["value"] for field in soup.find_all("input", id="run-id")]


async def test_review_post_keeps_campaign_filter(aiohttp_client, db):
    client = await create_filter_client(aiohttp_client, db)
    async with db.acquire() as conn:
        for run_id, codebase, campaign, value in [
            ("run-a", "foo", "campaign-a", 3),
            ("run-b1", "bar", "campaign-b", 2),
            ("run-b2", "baz", "campaign-b", 1),
        ]:
            await insert_filter_run(
                conn, run_id=run_id, codebase=codebase, campaign=campaign, value=value
            )

    resp = await client.get("/cupboard/review?suite=campaign-b&required_only=false")
    text = await resp.text()
    assert shown_run(text) == ["run-b1"]
    fields = [
        (field["name"], field.get("value", ""))
        for field in BeautifulSoup(text, "html.parser").find_all("input", type="hidden")
    ]
    assert fields == [
        ("run_id", "run-b1"),
        ("codebase", "bar"),
        ("suite", "campaign-b"),
    ]

    resp = await client.post("/cupboard/review", data=[*fields, ("verdict", "Abstain")])
    assert resp.status == 200
    assert shown_run(await resp.text()) == ["run-b2"]
