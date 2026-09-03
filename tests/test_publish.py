#!/usr/bin/python
# Copyright (C) 2022 Jelmer Vernooij <jelmer@jelmer.uk>
#
# This program is free software; you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation; either version 2 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program; if not, write to the Free Software
# Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301 USA

from datetime import datetime

from fakeredis.aioredis import FakeRedis

from janitor.config import read_string as read_config_string
from janitor.publish import create_app
from janitor.runner import store_change_set, store_run
from janitor.vcs import get_vcs_managers


async def _insert_codebase(conn, name):
    await conn.execute(
        "INSERT INTO codebase (name, branch_url, url) VALUES ($1, $2, $2)",
        name,
        f"https://example.com/{name}.git",
    )


async def _insert_run(conn, *, run_id, codebase, campaign="mycampaign"):
    now = datetime.utcnow()
    await store_change_set(conn, run_id, campaign=campaign)
    await store_run(
        conn,
        run_id=run_id,
        codebase=codebase,
        campaign=campaign,
        vcs_type="git",
        subpath="",
        start_time=now,
        finish_time=now,
        command="true",
        result_code="success",
        codemod_result={},
        main_branch_revision=b"revid",
        revision=b"revid",
        description=None,
        context=None,
        instigated_context=None,
        logfilenames=[],
        value=1,
        change_set=run_id,
        worker_name=None,
        branch_url=f"https://example.com/{codebase}.git",
        # Without at least one result branch, `roles` ends up empty and the
        # publish_policy.get() call this guards never actually runs - the
        # pre-fix crash needs a role to look a (missing) policy up for.
        result_branches=[("main", "main", b"oldrev", b"newrev")],
    )


async def create_client(aiohttp_client, db, tmp_path):
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    config = read_config_string("")
    app = await create_app(
        vcs_managers=get_vcs_managers(str(vcs)),
        db=db,
        redis=FakeRedis(),
        config=config,
    )
    return await aiohttp_client(app)


async def test_publish_request_without_publish_policy_returns_404(
    aiohttp_client, db, tmp_path
):
    client = await create_client(aiohttp_client, db, tmp_path)
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_run(conn, run_id="somerun", codebase="foo")

    # No candidate row exists for foo/mycampaign, so there is no publish
    # policy to look up. Before the fix, this crashed with an
    # AttributeError (None has no .get) instead of a clean 404.
    resp = await client.post("/mycampaign/foo/publish")
    assert resp.status == 404, await resp.text()
    assert await resp.json() == {"reason": "no publish policy for foo/mycampaign"}
