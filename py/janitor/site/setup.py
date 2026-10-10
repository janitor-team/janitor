#!/usr/bin/python3
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


def setup_redis(app):
    from redis.asyncio import Redis

    async def connect_redis(app):
        app["redis"] = Redis.from_url(app["config"].redis_location)

    async def disconnect_redis(app):
        await app["redis"].close()

    app.on_startup.append(connect_redis)
    app.on_cleanup.append(disconnect_redis)


def setup_gpg(app):
    import tempfile

    import gpg

    async def start_gpg_context(app):
        gpg_home = tempfile.TemporaryDirectory(prefix="janitor-gpg")
        gpg_home.__enter__()
        gpg_context = gpg.Context(home_dir=gpg_home.name)
        app["gpg"] = gpg_context.__enter__()

        async def cleanup_gpg(app):
            gpg_context.__exit__(None, None, None)
            gpg_home.__exit__(None, None, None)

        app.on_cleanup.append(cleanup_gpg)

    app.on_startup.append(start_gpg_context)


def setup_postgres(app):
    from ..state import create_pool

    async def connect_postgres(app):
        database = await create_pool(app["config"].database_location)
        app.database = database
        app["pool"] = database

    app.on_startup.append(connect_postgres)


def setup_logfile_manager(app):
    from ..logs import get_log_manager

    async def startup_logfile_manager(app):
        app["logfile_manager"] = get_log_manager(app["config"].logs_location)
        await app["logfile_manager"].__aenter__()

    app.on_startup.append(startup_logfile_manager)

    async def teardown_logfile_manager(app):
        await app["logfile_manager"].__aexit__(None, None, None)

    app.on_cleanup.append(teardown_logfile_manager)


def setup_artifact_manager(app):
    from ..artifacts import get_artifact_manager

    async def startup_artifact_manager(app):
        app["artifact_manager"] = get_artifact_manager(app["config"].artifact_location)
        await app["artifact_manager"].__aenter__()

    async def turndown_artifact_manager(app):
        await app["artifact_manager"].__aexit__(None, None, None)

    app.on_startup.append(startup_artifact_manager)
    app.on_cleanup.append(turndown_artifact_manager)
