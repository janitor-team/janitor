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

REVIEWER = {"email": "reviewer@example.com", "name": "Reviewer", "groups": []}


async def create_required_client(aiohttp_client, db):
    app = create_app(
        config=read_config_string("oauth2_provider {}\n"),
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


async def insert_required_run(conn, *, run_id, codebase, value, publish_status):
    await conn.execute(
        "INSERT INTO named_publish_policy (name, per_branch_policy) VALUES "
        "('mypolicy', ARRAY[ROW('main', 'propose', NULL)::branch_publish_policy]) "
        "ON CONFLICT DO NOTHING"
    )
    await _insert_codebase(conn, codebase)
    await conn.execute(
        "INSERT INTO candidate (suite, command, codebase, publish_policy) "
        "VALUES ('mycampaign', 'true', $1, 'mypolicy')",
        codebase,
    )
    now = utcnow()
    await _insert_run(
        conn,
        run_id=run_id,
        codebase=codebase,
        start_time=now - timedelta(minutes=30),
        finish_time=now,
    )
    await conn.execute(
        "UPDATE run SET value = $2, vcs_type = NULL, publish_status = $3 WHERE id = $1",
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


async def test_review_post_keeps_required_only(aiohttp_client, db):
    client = await create_required_client(aiohttp_client, db)
    async with db.acquire() as conn:
        for run_id, codebase, value, publish_status in [
            ("run-x", "foo", 3, "unknown"),
            ("run-a", "bar", 2, "needs-manual-review"),
            ("run-b", "baz", 1, "needs-manual-review"),
        ]:
            await insert_required_run(
                conn,
                run_id=run_id,
                codebase=codebase,
                value=value,
                publish_status=publish_status,
            )

    resp = await client.get("/cupboard/review")
    text = await resp.text()
    assert shown_run(text) == ["run-a"]
    fields = [
        (field["name"], field.get("value", ""))
        for field in BeautifulSoup(text, "html.parser").find_all("input", type="hidden")
    ]

    data = [*fields, ("publishable_only", "true"), ("verdict", "Abstain")]
    resp = await client.post("/cupboard/review", data=data)
    assert resp.status == 200
    assert shown_run(await resp.text()) == ["run-b"]
