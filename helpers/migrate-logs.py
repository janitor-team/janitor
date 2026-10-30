#!/usr/bin/python3

import argparse
import asyncio
import os
import tempfile

import silver_platter  # noqa: E402, F401

from janitor import state  # noqa: E402
from janitor.config import read_config  # noqa: E402
from janitor.logs import get_log_manager  # noqa: E402

BUILD_LOG_FILENAME = "build.log"

parser = argparse.ArgumentParser()
parser.add_argument(
    "--config", type=str, default="janitor.conf", help="Path to configuration."
)
parser.add_argument("from_location", type=str)
parser.add_argument("to_location", type=str)
args = parser.parse_args()

try:
    with open(args.config) as f:
        config = read_config(f)
except FileNotFoundError:
    parser.error(f"config path {args.config} does not exist")

from_manager = get_log_manager(args.from_location)
to_manager = get_log_manager(args.to_location)


async def reprocess_run(pool, codebase, log_id, logfilenames):
    if logfilenames is None:
        logfilenames = []
        if await from_manager.has_log(codebase, log_id, "worker.log"):
            logfilenames.append("worker.log")
        if await from_manager.has_log(codebase, log_id, BUILD_LOG_FILENAME):
            logfilenames.append(BUILD_LOG_FILENAME)
        i = 1
        while await from_manager.has_log(codebase, log_id, f"{BUILD_LOG_FILENAME}.{i}"):
            log_name = f"{BUILD_LOG_FILENAME}.{i}"
            logfilenames.append(log_name)
            i += 1

        async with pool.acquire() as conn:
            await conn.execute(
                "UPDATE run SET logfilenames = $1 WHERE id = $2", logfilenames, log_id
            )

    print(f"Processing {log_id} ({logfilenames!r})")
    with tempfile.TemporaryDirectory() as d:
        for name in logfilenames:
            try:
                log = await from_manager.get_log(codebase, log_id, name)
            except FileNotFoundError:
                continue
            path = os.path.join(d, name)
            with open(path, "wb") as f:
                f.write(await log.read())
            await to_manager.import_log(codebase, log_id, path)
            await from_manager.delete_log(codebase, log_id, name)


async def process_all_build_failures(db_location):
    todo = []
    async with state.create_pool(db_location) as pool, pool.acquire() as conn:
        async with conn.transaction():
            async for row in conn.cursor("SELECT codebase, id, logfilenames FROM run"):
                todo.append(
                    reprocess_run(pool, row["codebase"], row["id"], row["logfilenames"])
                )
    for i in range(0, len(todo), 100):
        await asyncio.gather(*todo[i : i + 100])


asyncio.run(process_all_build_failures(config.database_location))
