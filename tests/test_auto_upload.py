#!/usr/bin/python3
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
import json

from fakeredis.aioredis import FakeRedis

from janitor.debian.auto_upload import listen_to_runner


async def test_listen_to_runner_survives_result_with_no_build_target():
    # runner.py serializes "target": {} for any run with no builder_result
    # (a failed or non-build run) - listen_to_runner must not crash on it.
    redis = FakeRedis()
    task = asyncio.ensure_future(
        listen_to_runner(redis, artifact_manager=None, dput_host=None)
    )
    try:
        await asyncio.sleep(0.1)
        await redis.publish("result", json.dumps({"target": {}, "log_id": "some-id"}))
        await asyncio.sleep(0.1)
        assert not task.done(), task.exception() if task.done() else None
    finally:
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
