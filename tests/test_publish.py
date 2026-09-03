#!/usr/bin/python
# Copyright (C) 2026 Jelmer Vernooij <jelmer@jelmer.uk>
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

from janitor.config import read_string as read_config_string
from janitor.publish import create_app


async def create_client(aiohttp_client, db):
    config = read_config_string("")
    app = await create_app(vcs_managers={}, db=db, redis=None, config=config)
    return await aiohttp_client(app)


async def test_policy_get_not_found(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/policy/does-not-exist")
    assert resp.status == 404


async def test_policy_get(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.put(
        "/policy/lintian-fixes",
        json={
            "rate_limit_bucket": "default",
            "per_branch": {
                "main": {"mode": "propose", "max_frequency_days": 7},
            },
        },
    )
    assert resp.status == 200

    resp = await client.get("/policy/lintian-fixes")
    assert resp.status == 200
    assert await resp.json() == {
        "rate_limit_bucket": "default",
        "per_branch": {
            "main": {"mode": "propose", "max_frequency_days": 7},
        },
    }
