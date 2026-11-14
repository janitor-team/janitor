from datetime import timedelta

import aiohttp_jinja2
import aiozipkin
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


async def test_review_refill_keeps_filter(aiohttp_client, db):
    client = await create_publishable_client(aiohttp_client, db)
    async with db.acquire() as conn:
        for run_id, codebase, campaign, mode, value in [
            ("run-a", "foo", "campaign-a", "propose", 3),
            ("run-b", "bar", "campaign-b", "propose", 2),
            ("run-c", "baz", "campaign-c", "propose", 1),
            ("run-d", "qux", "campaign-b", "build-only", 0),
        ]:
            await insert_review_run(
                conn,
                run_id,
                codebase=codebase,
                campaign=campaign,
                mode=mode,
                value=value,
            )

    for publishable, expected in [
        ("", ["run-a", "run-b"]),
        ("&publishable_only=false", ["run-a", "run-b", "run-d"]),
    ]:
        resp = await client.get(
            "/cupboard/review?suite=campaign-a&suite=campaign-b" + publishable
        )
        assert resp.status == 200
        text = await resp.text()
        assert shown_run(text) == ["run-a"]
        (review,) = BeautifulSoup(text, "html.parser").find_all(id="review")
        resp = await client.get(review["data-needs-review-url"])
        assert resp.status == 200
        assert [entry["id"] for entry in await resp.json()] == expected

    resp = await client.get("/cupboard/api/campaign-a/needs-review?campaign=campaign-b")
    assert (resp.status, await resp.text()) == (
        400,
        "use either a campaign in the path or campaign arguments",
    )


def shown_run(text):
    soup = BeautifulSoup(text, "html.parser")
    return [field["value"] for field in soup.find_all("input", id="run-id")]


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
