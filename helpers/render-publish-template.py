#!/usr/bin/python3

import argparse
import asyncio
import logging
import os
import sys

from janitor.config import read_config
from janitor.publish_one import load_template_env

sys.path.insert(0, os.path.dirname(__file__))

from janitor import state  # noqa: E402
from janitor.debian.debdiff import debdiff_is_empty, markdownify_debdiff  # noqa: E402

loop = asyncio.get_event_loop()

parser = argparse.ArgumentParser()
parser.add_argument(
    "--config", type=str, default="janitor.conf", help="Path to configuration."
)
parser.add_argument("-r", "--run-id", type=str, help="Run id to process")
parser.add_argument("--role", type=str, help="Role", default="main")
parser.add_argument("--format", type=str, choices=["md", "txt"], default="md")

parser.add_argument("--template-env-path", type=str, help="Path to template env")

args = parser.parse_args()

logging.basicConfig(level=logging.INFO, format="%(message)s")

try:
    with open(args.config) as f:
        config = read_config(f)
except FileNotFoundError:
    parser.error(f"config path {args.config} does not exist")

template_env = load_template_env(args.template_env_path)


async def process_build(db_location, run_id, role, format):
    async with state.create_pool(db_location) as conn:
        query = """
SELECT
  package.name AS package,
  campaign,
  id AS log_id,
  result AS _result
FROM run
LEFT JOIN package ON run.codebase = package.codebase
WHERE
  id = $1
"""
        row = await conn.fetchrow(query, run_id)
        vs = {}
        vs.update(row)
        if row["_result"]:
            vs.update(row["_result"])
        vs["external_url"] = "https://janitor.debian.net/"
        vs["markdownify_debdiff"] = markdownify_debdiff
        vs["debdiff_is_empty"] = debdiff_is_empty
        print(template_env.get_template(vs["campaign"] + "." + format).render(vs))


loop.run_until_complete(
    process_build(config.database_location, args.run_id, args.role, args.format)
)
