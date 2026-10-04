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


import json
import os
import tempfile

from janitor.artifacts import LocalArtifactManager
from janitor.differ import create_app, precache


async def create_client(aiohttp_client, db):
    td = tempfile.TemporaryDirectory().name
    atd = tempfile.TemporaryDirectory().name
    afm = LocalArtifactManager(atd)
    return await aiohttp_client(create_app(td, afm, db=db))


async def test_health(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)

    resp = await client.get("/health")
    assert resp.status == 200
    text = await resp.text()
    assert text == "ok"


async def test_ready(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)

    resp = await client.get("/ready")
    assert resp.status == 200
    text = await resp.text()
    assert text == "ok"


async def test_precache_all(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)

    resp = await client.post("/precache-all")
    assert resp.status == 200
    assert {"count": 0} == await resp.json()


class _ArtifactManager:
    async def retrieve_artifacts(
        self, run_id, local_path, filter_fn=None, timeout=None
    ):
        with open(os.path.join(local_path, "pkg_1.0_all.deb"), "wb") as f:
            f.write(run_id.encode())


async def test_precache_runs_diffoscope():
    with tempfile.TemporaryDirectory() as td:
        path = os.path.join(td, "diffoscope.json")
        await precache(
            _ArtifactManager(),
            "old",
            "new",
            task_memory_limit=1500,
            task_timeout=60,
            diffoscope_cache_path=lambda old_id, new_id: path,
            diffoscope_command="true",
        )
        with open(path) as f:
            assert json.load(f)["source1"] == "old version"
