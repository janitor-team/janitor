#!/usr/bin/python3
# Copyright (C) 2019-2020 Jelmer Vernooij <jelmer@jelmer.uk>
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
import logging
import os
import sys
import tempfile
from typing import Optional

from aiohttp import web
from aiohttp.web_middlewares import normalize_path_middleware
from aiohttp_openmetrics import Counter, setup_metrics
from redis.asyncio import Redis
from silver_platter import debian as sp_debian

from ..artifacts import ArtifactsMissing, get_artifact_manager
from ..config import read_config

DebsignFailure = sp_debian.DebsignFailure
DputFailure = sp_debian.DputFailure
debsign = sp_debian.debsign
dput_changes = sp_debian.dput_changes

logger = logging.getLogger("janitor.debian.auto_upload")
debsign_failed_count = Counter(
    "debsign_failed", "Number of packages for which signing failed."
)
upload_failed_count = Counter(
    "upload_failed", "Number of packages for which uploading failed."
)


async def run_web_server(listen_addr, port, config):
    trailing_slash_redirect = normalize_path_middleware(append_slash=True)
    app = web.Application(middlewares=[trailing_slash_redirect])
    app.config = config
    setup_metrics(app)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, listen_addr, port)
    await site.start()


async def upload_build_result(
    log_id,
    artifact_manager,
    dput_host,
    debsign_keyid: Optional[str] = None,
    source_only: bool = False,
):
    logging.info("Uploading results for %s", log_id, extra={"run_id": log_id})
    with tempfile.TemporaryDirectory(prefix="janitor-auto-upload") as td:
        try:
            await artifact_manager.retrieve_artifacts(log_id, td)
        except ArtifactsMissing:
            logging.error(
                "artifacts for build %s are missing", log_id, extra={"run_id": log_id}
            )
            return
        changes_filenames = []
        # Work around https://bugs.debian.org/389908:
        umask = os.umask(0)
        os.umask(umask)
        for entry in os.scandir(td):
            os.chmod(entry.path, 0o644 & ~umask)
            if not entry.name.endswith(".changes"):
                continue
            if source_only and not entry.name.endswith("_source.changes"):
                continue
            changes_filenames.append(entry.name)

        if not changes_filenames:
            logging.error(
                "no changes filename in build artifacts", extra={"run_id": log_id}
            )
            return

        failures = False
        for changes_filename in changes_filenames:
            logging.info("Running debsign", extra={"run_id": log_id})
            try:
                await debsign(td, changes_filename, debsign_keyid)
            except DebsignFailure as e:
                logging.error(
                    "Error (exit code %d) signing %s for %s: %s",
                    e.returncode,
                    changes_filename,
                    log_id,
                    e.reason,
                    extra={"run_id": log_id},
                )
                failures = True
                debsign_failed_count.inc()
            else:
                logging.info(
                    "Successfully signed %s for %s",
                    changes_filename,
                    log_id,
                    extra={"run_id": log_id},
                )

            logging.debug("Running dput.", extra={"run_id": log_id})
            try:
                await dput_changes(td, changes_filename, dput_host)
            except DputFailure as e:
                upload_failed_count.inc()
                logging.error(
                    "Error (exit code %d) uploading %s for %s: %s",
                    e.returncode,
                    changes_filename,
                    log_id,
                    e.reason,
                    extra={"run_id": log_id},
                )
                failures = True

        if not failures:
            logging.info(
                "Successfully uploaded run %s", log_id, extra={"run_id": log_id}
            )


def is_debian_upload_target(result: dict, distributions: Optional[list[str]]) -> bool:
    # a non-success run publishes "target": {}, so "code" has to be
    # checked before "target"["name"] to avoid a KeyError
    if result["code"] != "success":
        return False
    if result["target"]["name"] != "debian":
        return False
    return (
        not distributions
        or result["target"]["details"]["build_distribution"] in distributions
    )


async def listen_to_runner(
    redis,
    artifact_manager,
    dput_host,
    debsign_keyid: Optional[str] = None,
    distributions: Optional[list[str]] = None,
    source_only: bool = False,
):
    async def handle_result_message(msg):
        result = json.loads(msg["data"])

        if is_debian_upload_target(result, distributions):
            await upload_build_result(
                result["log_id"],
                artifact_manager,
                dput_host,
                debsign_keyid=debsign_keyid,
                source_only=source_only,
            )

    try:
        async with redis.pubsub(ignore_subscribe_messages=True) as ch:
            await ch.subscribe("result", result=handle_result_message)
            await ch.run()
    finally:
        await redis.close()


async def backfill(
    db,
    artifact_manager,
    dput_host,
    debsign_keyid=None,
    distributions=None,
    source_only=False,
):
    async with db.acquire() as conn:
        query = "SELECT DISTINCT ON (distribution, source) distribution, source, run_id FROM debian_build"
        args = []
        if distributions:
            query += " WHERE distribution = ANY($1::text[])"
            args.append(distributions)
        query += " ORDER BY distribution, source, version DESC"
        print(query)
        for row in await conn.fetch(query, *args):
            await upload_build_result(
                row["run_id"],
                artifact_manager,
                dput_host,
                debsign_keyid=debsign_keyid,
                source_only=source_only,
            )


async def main_async(argv=None):
    import argparse

    parser = argparse.ArgumentParser(
        prog="janitor.debian.auto_upload",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument("--port", type=int, help="Listen port", default=9933)
    parser.add_argument(
        "--listen-address", type=str, help="Listen address", default="localhost"
    )
    parser.add_argument(
        "--config", type=str, default="janitor.conf", help="Path to configuration"
    )
    parser.add_argument("--dput-host", type=str, help="dput host to upload to")
    parser.add_argument("--debsign-keyid", type=str, help="Key ID to use for signing")
    parser.add_argument(
        "--backfill", action="store_true", help="Upload previously built packages"
    )
    parser.add_argument(
        "--source-only", action="store_true", help="Only upload source-only changes"
    )
    parser.add_argument(
        "--distribution", action="append", help="Build distributions to upload"
    )
    parser.add_argument(
        "--verbose", action="store_true", help="Show more detailed output"
    )

    args = parser.parse_args()

    if args.verbose:
        logging.basicConfig(level=logging.DEBUG)
    else:
        logging.basicConfig(level=logging.INFO)

    try:
        with open(args.config) as f:
            config = read_config(f)
    except FileNotFoundError:
        parser.error(f"config path {args.config} does not exist")

    artifact_manager = get_artifact_manager(config.artifact_location)

    loop = asyncio.get_running_loop()

    tasks = [
        loop.create_task(
            run_web_server(
                args.listen_address,
                args.port,
                config,
            )
        )
    ]

    def log_result(future):
        try:
            future.result()
        except BaseException:
            logging.exception("listening to runner failed")
            sys.exit(1)

    redis = Redis.from_url(config.redis_location)
    runner_task = loop.create_task(
        listen_to_runner(
            redis,
            artifact_manager,
            args.dput_host,
            args.debsign_keyid,
            args.distribution,
            source_only=args.source_only,
        )
    )
    runner_task.add_done_callback(log_result)
    tasks.append(runner_task)

    if args.backfill:
        from .. import state

        db = await state.create_pool(config.database_location)
        backfill_task = loop.create_task(
            backfill(
                db,
                artifact_manager,
                args.dput_host,
                args.debsign_keyid,
                args.distribution,
                source_only=args.source_only,
            )
        )
        backfill_task.add_done_callback(log_result)
        tasks.append(backfill_task)

    await asyncio.wait(tasks, return_when=asyncio.ALL_COMPLETED)


def main():
    sys.exit(asyncio.run(main_async(sys.argv[1:])))


if __name__ == "__main__":
    main()
