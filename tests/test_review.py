from datetime import timedelta
from html.parser import HTMLParser

import aiohttp_jinja2
import aiozipkin
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
campaign { name: "campaign-c" }
"""

REVIEWER = {"email": "reviewer@example.com", "name": "Reviewer", "groups": []}


async def create_review_client(aiohttp_client, db):
    app = create_app(
        config=read_config_string(CONFIG),
        publisher_url=None,
        runner_url=None,
        differ_url=None,
        db=db,
    )
    app["external_url"] = URL("http://example.com/")
    # create_app leaves the jinja globals to its caller, and the production
    # caller is py/janitor/site/simple.py, which also calls aiozipkin.setup().
    aiohttp_jinja2.get_env(app).globals.update(TEMPLATE_ENV)
    app.middlewares.insert(0, _user_middleware(REVIEWER))
    endpoint = aiozipkin.create_endpoint("janitor.site", ipv4="127.0.0.1", port=80)
    aiozipkin.setup(app, await aiozipkin.create_custom(endpoint))
    return await aiohttp_client(app)


async def _insert_needs_review_run(conn, *, run_id, codebase, campaign, value):
    await conn.execute(
        "INSERT INTO named_publish_policy (name, per_branch_policy) VALUES "
        "('mypolicy', ARRAY[ROW('main', 'propose', NULL)::branch_publish_policy]) "
        "ON CONFLICT DO NOTHING"
    )
    await _insert_codebase(conn, codebase)
    await conn.execute(
        "INSERT INTO candidate (suite, command, codebase, publish_policy) "
        "VALUES ($1, 'true', $2, 'mypolicy')",
        campaign,
        codebase,
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


async def _insert_needs_review_runs(conn):
    for run_id, codebase, campaign, value in [
        ("run-a", "foo", "campaign-a", 3),
        ("run-b", "bar", "campaign-b", 2),
        ("run-c", "baz", "campaign-c", 1),
    ]:
        await _insert_needs_review_run(
            conn, run_id=run_id, codebase=codebase, campaign=campaign, value=value
        )


class AttributeFinder(HTMLParser):
    def __init__(self, element_id, attribute) -> None:
        super().__init__()
        self.element_id = element_id
        self.attribute = attribute
        self.value = None

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if self.value is None and attrs.get("id") == self.element_id:
            self.value = attrs.get(self.attribute)


def find_attribute(text, element_id, attribute):
    finder = AttributeFinder(element_id, attribute)
    finder.feed(text)
    return finder.value


async def test_needs_review_filters_on_suites(aiohttp_client, db):
    client = await create_review_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await _insert_needs_review_runs(conn)

    resp = await client.get(
        "/cupboard/api/needs-review?suite=campaign-a&suite=campaign-b"
    )
    assert resp.status == 200
    assert sorted(entry["id"] for entry in await resp.json()) == ["run-a", "run-b"]


async def test_review_refill_keeps_campaign_filter(aiohttp_client, db):
    client = await create_review_client(aiohttp_client, db)
    async with db.acquire() as conn:
        await _insert_needs_review_runs(conn)

    resp = await client.get(
        "/cupboard/review?suite=campaign-a&suite=campaign-b&required_only=false"
    )
    assert resp.status == 200
    needs_review_url = find_attribute(
        await resp.text(), "review", "data-needs-review-url"
    )
    assert needs_review_url is not None
    resp = await client.get(needs_review_url)
    assert resp.status == 200
    assert sorted(entry["id"] for entry in await resp.json()) == ["run-a", "run-b"]
