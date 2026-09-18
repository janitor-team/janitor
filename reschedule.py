#!/usr/bin/env python3

# Copyright (C) 2020 Jelmer Vernooij <jelmer@jelmer.uk>
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

import argparse
import asyncio
import logging
import sys

from aiohttp import ClientSession
from yarl import URL

parser = argparse.ArgumentParser("reschedule")
parser.add_argument("result_code", type=str)
parser.add_argument("description_re", type=str, nargs="?")
parser.add_argument("--refresh", action="store_true", help="Force run from scratch.")
parser.add_argument("--offset", type=int, default=0, help="Schedule offset.")
parser.add_argument(
    "--rejected", action="store_true", help="Process rejected runs only."
)
parser.add_argument("--campaign", type=str, help="Campaign to process.")
parser.add_argument(
    "--min-age", type=int, default=0, help="Only reschedule runs older than N days."
)
parser.add_argument(
    "--base-url", type=str, default="https://janitor.debian.net", help="Instance URL"
)
args = parser.parse_args()

logging.basicConfig()


async def main(base_url, result_code, campaign, description_re, rejected, min_age=0):
    params = {"result_code": result_code}
    if campaign:
        params["campaign"] = campaign
    if description_re:
        params["description_re"] = description_re
    if rejected:
        params["rejected"] = "1"
    if min_age:
        params["min_age"] = str(min_age)
    url = URL(base_url) / "cupboard/api/mass-reschedule"
    async with ClientSession() as session, session.post(url, data=params) as resp:
        if resp.status != 200:
            logging.fatal("rescheduling failed: %d", resp.status)
            return 1
        for entry in await resp.json():
            logging.info("%r", entry)


sys.exit(
    asyncio.run(
        main(
            args.base_url,
            args.result_code,
            args.campaign,
            args.description_re,
            args.rejected,
            args.min_age,
        )
    )
)
