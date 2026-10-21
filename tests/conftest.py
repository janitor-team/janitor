from collections.abc import AsyncGenerator
from pathlib import Path

import asyncpg
import pytest_asyncio
import testing.postgresql

from janitor.state import create_pool

pytest_plugins = ["aiohttp"]

_SCHEMA_DIR = Path(__file__).resolve().parent.parent / "schema"


@pytest_asyncio.fixture()
async def database_url():
    with testing.postgresql.Postgresql() as postgresql:
        conn = await asyncpg.connect(postgresql.url())
        try:
            await conn.execute((_SCHEMA_DIR / "state.sql").read_text())
            await conn.execute((_SCHEMA_DIR / "debian" / "debian.sql").read_text())
        finally:
            await conn.close()

        yield postgresql.url()


@pytest_asyncio.fixture()
async def db(database_url):
    db = await create_pool(database_url)

    yield db

    await db.close()


@pytest_asyncio.fixture()
async def con(db: asyncpg.Pool) -> AsyncGenerator[asyncpg.Connection, None]:
    async with db.acquire() as con:
        yield con


async def _insert_codebase(conn, name):
    await conn.execute(
        "INSERT INTO codebase (name, branch_url, url) VALUES ($1, $2, $2)",
        name,
        f"https://example.com/{name}.git",
    )


async def _insert_run(
    conn,
    *,
    run_id,
    codebase,
    campaign="mycampaign",
    start_time,
    finish_time,
    result_code="success",
    description=None,
):
    await conn.execute(
        "INSERT INTO change_set (id, campaign) VALUES ($1, $2)", run_id, campaign
    )
    await conn.execute(
        "INSERT INTO run (id, command, result_code, start_time, finish_time, "
        "main_branch_revision, revision, result, suite, vcs_type, branch_url, "
        "subpath, logfilenames, value, change_set, codebase, description) "
        "VALUES ($1, 'true', $7, $2, $3, 'revid', 'revid', '{}', $4, "
        "'git', $5, '', '{}', 1, $1, $6, $8)",
        run_id,
        start_time,
        finish_time,
        campaign,
        f"https://example.com/{codebase}.git",
        codebase,
        result_code,
        description,
    )


async def test_db_returns_janitor_db(db) -> None:
    assert isinstance(db, asyncpg.Pool)
