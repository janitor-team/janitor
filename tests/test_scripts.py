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

import asyncio
import os
import sys

from aiohttp import web

TOPDIR = os.path.join(os.path.dirname(__file__), "..")


async def run_script(aiohttp_server, name, path, argv):
    posted = []

    async def handler(request):
        post = await request.post()
        posted.append(list(post.items()))
        return web.json_response([])

    app = web.Application()
    app.router.add_post(path, handler)
    server = await aiohttp_server(app)

    proc = await asyncio.create_subprocess_exec(
        sys.executable,
        os.path.join(TOPDIR, name),
        "--base-url",
        str(server.make_url("/")),
        *argv,
    )
    assert await asyncio.wait_for(proc.wait(), 60) == 0
    return posted


async def test_reschedule(aiohttp_server):
    posted = await run_script(
        aiohttp_server,
        "reschedule.py",
        "/cupboard/api/mass-reschedule",
        [
            "--campaign=lintian-fixes",
            "--refresh",
            "--offset=10",
            "--rejected",
            "--min-age=3",
            "build-failed",
            "some.*regex",
        ],
    )
    assert posted == [
        [
            ("result_code", "build-failed"),
            ("campaign", "lintian-fixes"),
            ("description_re", "some.*regex"),
            ("rejected", "1"),
            ("min_age", "3"),
            ("refresh", "1"),
            ("offset", "10"),
        ]
    ]


async def test_reprocess_build_results(aiohttp_server):
    posted = await run_script(
        aiohttp_server,
        "reprocess-build-results.py",
        "/cupboard/api/reprocess-logs",
        ["--dry-run", "--reschedule", "-r", "run-1", "-r", "run-2"],
    )
    assert posted == [
        [
            ("dry_run", "1"),
            ("reschedule", "1"),
            ("run_id", "run-1"),
            ("run_id", "run-2"),
        ]
    ]


async def test_reprocess_build_results_all(aiohttp_server):
    posted = await run_script(
        aiohttp_server,
        "reprocess-build-results.py",
        "/cupboard/api/reprocess-logs",
        [],
    )
    assert posted == [[]]
