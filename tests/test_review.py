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


async def create_publishable_client(aiohttp_client, db):
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


async def insert_build_only_run(conn, run_id):
    await conn.execute(
        "INSERT INTO named_publish_policy (name, per_branch_policy) VALUES "
        "('mypolicy', ARRAY[ROW('main', 'build-only', NULL)::branch_publish_policy])"
    )
    await _insert_codebase(conn, "foo")
    await conn.execute(
        "INSERT INTO candidate (suite, command, codebase, publish_policy) "
        "VALUES ('mycampaign', 'true', 'foo', 'mypolicy')"
    )
    now = utcnow()
    await _insert_run(
        conn,
        run_id=run_id,
        codebase="foo",
        start_time=now - timedelta(minutes=30),
        finish_time=now,
    )
    await conn.execute("UPDATE run SET vcs_type = NULL WHERE id = $1", run_id)
    await conn.execute(
        "INSERT INTO new_result_branch (run_id, role, remote_name, base_revision, "
        "revision) VALUES ($1, 'main', 'main', 'base', 'revid')",
        run_id,
    )


def shown_run(text):
    soup = BeautifulSoup(text, "html.parser")
    return [field["value"] for field in soup.find_all("input", id="run-id")]


async def test_review_post_publishable_only_unchecked(aiohttp_client, db):
    client = await create_publishable_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await insert_build_only_run(conn, "somerun")

    resp = await client.get("/cupboard/review?required_only=false")
    assert shown_run(await resp.text()) == []

    resp = await client.post("/cupboard/review", data={"publishable_only": "true"})
    assert resp.status == 200
    assert shown_run(await resp.text()) == []

    resp = await client.post("/cupboard/review", data={"run_id": "somerun"})
    assert resp.status == 200
    assert shown_run(await resp.text()) == ["somerun"]
