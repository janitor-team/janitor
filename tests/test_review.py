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


def hidden_fields(text):
    soup = BeautifulSoup(text, "html.parser")
    return [
        (field["name"], field.get("value", ""))
        for field in soup.find_all("input", type="hidden")
    ]


async def insert_campaign_runs(conn):
    for run_id, codebase, campaign, mode, value in [
        ("run-a", "foo", "campaign-a", "propose", 3),
        ("run-b", "bar", "campaign-b", "propose", 2),
        ("run-c", "baz", "campaign-c", "propose", 1),
        ("run-d", "qux", "campaign-b", "build-only", 0),
    ]:
        await insert_review_run(
            conn, run_id, codebase=codebase, campaign=campaign, mode=mode, value=value
        )


async def test_review_post_keeps_campaign_filter(aiohttp_client, db):
    client = await create_review_client(
        aiohttp_client, db, campaigns=["campaign-a", "campaign-b"]
    )
    async with db.acquire() as conn:
        for run_id, codebase, campaign, value in [
            ("run-a", "foo", "campaign-a", 3),
            ("run-b1", "bar", "campaign-b", 2),
            ("run-b2", "baz", "campaign-b", 1),
        ]:
            await insert_review_run(
                conn,
                run_id,
                codebase=codebase,
                campaign=campaign,
                mode="propose",
                value=value,
            )

    resp = await client.get("/cupboard/review?suite=campaign-b")
    text = await resp.text()
    assert shown_run(text) == ["run-b1"]
    fields = hidden_fields(text)
    assert [value for name, value in fields if name == "suite"] == ["campaign-b"]

    resp = await client.post("/cupboard/review", data=[*fields, ("verdict", "Abstain")])
    assert resp.status == 200
    assert shown_run(await resp.text()) == ["run-b2"]


async def test_review_post_keeps_required_only(aiohttp_client, db):
    client = await create_review_client(aiohttp_client, db)
    async with db.acquire() as conn:
        for run_id, codebase, value, publish_status in [
            ("run-x", "foo", 3, "unknown"),
            ("run-y", "bar", 2, "unknown"),
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

    resp = await client.get("/cupboard/review?required_only=false")
    text = await resp.text()
    assert shown_run(text) == ["run-x"]
    data = [*hidden_fields(text), ("publishable_only", "true"), ("verdict", "Abstain")]
    resp = await client.post("/cupboard/review", data=data)
    assert resp.status == 200
    assert shown_run(await resp.text()) == ["run-y"]


async def test_review_rejects_bad_bool_params(aiohttp_client, db):
    client = await create_review_client(aiohttp_client, db)
    for name in ["publishable_only", "required_only"]:
        resp = await client.get("/cupboard/review", params={name: "yes"})
        assert (resp.status, await resp.text()) == (
            400,
            f"{name} must be true or false",
        )
        resp = await client.post("/cupboard/review", data={name: "yes"})
        assert (resp.status, await resp.text()) == (
            400,
            f"{name} must be true or false",
        )


async def test_needs_review_filters_on_suites(aiohttp_client, db):
    client = await create_review_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await insert_campaign_runs(conn)

    resp = await client.get(
        "/cupboard/api/needs-review?suite=campaign-a&suite=campaign-b"
    )
    assert resp.status == 200
    assert [entry["id"] for entry in await resp.json()] == ["run-a", "run-b"]

    resp = await client.get("/cupboard/api/campaign-a/needs-review?suite=campaign-b")
    assert (resp.status, await resp.text()) == (
        400,
        "use either a campaign in the path or suite arguments",
    )


async def test_review_refill_keeps_filter(aiohttp_client, db):
    campaigns = ["campaign-a", "campaign-b", "campaign-c"]
    client = await create_review_client(aiohttp_client, db, campaigns=campaigns)
    async with db.acquire() as conn:
        await insert_campaign_runs(conn)

    resp = await client.get(
        "/cupboard/review?suite=campaign-a&suite=campaign-b&publishable_only=false"
    )
    assert resp.status == 200
    text = await resp.text()
    assert shown_run(text) == ["run-a"]
    (review,) = BeautifulSoup(text, "html.parser").find_all(id="review")
    resp = await client.get(review["data-needs-review-url"])
    assert resp.status == 200
    assert [entry["id"] for entry in await resp.json()] == ["run-a", "run-b", "run-d"]


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
