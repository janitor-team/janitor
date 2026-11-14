from datetime import timedelta

import aiohttp_jinja2
import aiozipkin
from aiohttp import web
from aiohttp.test_utils import unused_port
from bs4 import BeautifulSoup
from yarl import URL

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.site import TEMPLATE_ENV
from janitor.site.cupboard import create_app, parse_review_form

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
    port = unused_port()
    app["external_url"] = URL(f"http://127.0.0.1:{port}/")
    aiohttp_jinja2.get_env(app).globals.update(TEMPLATE_ENV)
    app.middlewares.insert(0, _user_middleware(REVIEWER))
    endpoint = aiozipkin.create_endpoint("janitor.site", ipv4="127.0.0.1", port=80)
    aiozipkin.setup(app, await aiozipkin.create_custom(endpoint))
    return await aiohttp_client(app, server_kwargs={"port": port})


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


async def test_review_post_keeps_required_only(aiohttp_client, db):
    client = await create_publishable_client(aiohttp_client, db)
    async with db.acquire() as conn:
        for run_id, codebase, value, publish_status in [
            ("run-x", "foo", 2, "unknown"),
            ("run-a", "baz", 1, "needs-manual-review"),
        ]:
            await insert_review_run(
                conn,
                run_id,
                codebase=codebase,
                mode="propose",
                value=value,
                publish_status=publish_status,
            )

    resp = await client.post("/cupboard/review", data={"publishable_only": "true"})
    assert resp.status == 200
    assert shown_run(await resp.text()) == ["run-a"]

    resp = await client.post(
        "/cupboard/review", data={"required_only": "false", "publishable_only": "true"}
    )
    assert resp.status == 200
    assert shown_run(await resp.text()) == ["run-x"]


async def test_review_post_publishable_only_unchecked(aiohttp_client, db):
    client = await create_publishable_client(aiohttp_client, db)
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


def test_parse_review_form_reschedule():
    assert parse_review_form({"verdict": "Reschedule"}) == ("reschedule", None)


def test_parse_review_form_comment():
    assert parse_review_form({"verdict": "Reject", "review_comment": "broken"}) == (
        "rejected",
        "broken",
    )


def test_parse_review_form_empty_comment():
    assert parse_review_form({"verdict": "Approve", "review_comment": ""}) == (
        "approved",
        None,
    )


async def test_review_ignores_host_header(aiohttp_client, aiohttp_server, db):
    contacted = []

    async def attacker(request):
        contacted.append(request.path)
        return web.Response(text="<script>evil</script>")

    attacker_app = web.Application()
    attacker_app.router.add_get("/{tail:.*}", attacker)
    server = await aiohttp_server(attacker_app)
    client = await create_publishable_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await insert_build_only_run(conn, "somerun")

    resp = await client.get(
        "/cupboard/review?publishable_only=false&required_only=false",
        headers={"Host": f"{server.host}:{server.port}"},
    )
    assert resp.status == 200
    assert contacted == []
    soup = BeautifulSoup(await resp.text(), "html.parser")
    command = soup.select_one("#evaluate #command")
    assert command is not None
    assert command.get_text() == "true"
