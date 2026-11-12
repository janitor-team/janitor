#!/usr/bin/env python3

# Copyright (C) 2019-2020 Jelmer Vernooij <jelmer@jelmer.uk>
#
# This program is free software; you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation; either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.

import argparse
import asyncio
import logging
import sys

from aiohttp import ClientSession
from yarl import URL

loop = asyncio.get_event_loop()

parser = argparse.ArgumentParser()
parser.add_argument(
    "--log-timeout",
    type=int,
    default=60,
    help="Default timeout when retrieving log files.",
)
parser.add_argument(
    "-r", "--run-id", type=str, action="append", help="Run id to process"
)
parser.add_argument(
    "--reschedule",
    action="store_true",
    help="Schedule rebuilds for runs for which result code has changed.",
)
parser.add_argument("--dry-run", action="store_true")
parser.add_argument(
    "--base-url", type=str, default="https://janitor.debian.net", help="Instance URL"
)

args = parser.parse_args()

logging.basicConfig(level=logging.INFO, format="%(message)s")


async def reprocess_logs(base_url, run_ids=None, dry_run=False, reschedule=False):
    params = {}
    if dry_run:
        params["dry_run"] = "1"
    if reschedule:
        params["reschedule"] = "1"
    if run_ids:
        params["run_ids"] = run_ids
    url = URL(base_url) / "cupboard/api/mass-reschedule"
    async with ClientSession() as session, session.post(url, params=params) as resp:
        if resp.status != 200:
            logging.fatal("rescheduling failed: %d", resp.status)
            return 1
        for entry in await resp.json():
            logging.info("%r", entry)


sys.exit(
    asyncio.run(
        reprocess_logs(
            args.base_url, args.run_id, dry_run=args.dry_run, reschedule=args.reschedule
        )
    )
)
