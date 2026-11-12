#!/usr/bin/python
# Copyright (C) 2018 Jelmer Vernooij <jelmer@jelmer.uk>
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

__all__ = [
    "iter_publishable_suites",
]

import datetime
import json
import logging
import traceback
from typing import Any, Optional

import asyncpg
import asyncpg.pool
from aiohttp.web import HTTPServiceUnavailable, middleware
from aiohttp_openmetrics import Counter
from breezy import urlutils
from debian.changelog import Version

insufficient_resources_counter = Counter(
    "postgres_insufficient_resources", "Database refused query due to resource issues"
)


async def init_types(conn):
    await conn.set_type_codec(
        "json", encoder=json.dumps, decoder=json.loads, schema="pg_catalog"
    )
    await conn.set_type_codec(
        "jsonb", encoder=json.dumps, decoder=json.loads, schema="pg_catalog"
    )
    await conn.set_type_codec("debversion", format="text", encoder=str, decoder=Version)


def create_pool(uri, *args, **kwargs) -> asyncpg.pool.Pool:
    return asyncpg.create_pool(uri, init=init_types)


def get_result_branch(result_branches, role):
    for entry in result_branches:
        if role == entry[0]:
            return entry[1:]
    raise KeyError


class Run:
    id: str
    command: str
    description: Optional[str]
    result_code: str
    main_branch_revision: Optional[bytes]
    revision: Optional[bytes]
    context: Optional[str]
    result: Optional[Any]
    suite: str
    instigated_context: Optional[str]
    vcs_type: str
    branch_url: str
    logfilenames: Optional[list[str]]
    worker_name: Optional[str]
    result_branches: Optional[list[tuple[str, str, bytes, bytes]]]
    result_tags: Optional[list[tuple[str, bytes]]]
    target_branch_url: Optional[str]
    change_set: str
    failure_transient: Optional[bool]
    failure_stage: Optional[str]
    codebase: str

    __slots__ = [
        "id",
        "start_time",
        "finish_time",
        "command",
        "description",
        "result_code",
        "value",
        "main_branch_revision",
        "revision",
        "context",
        "result",
        "suite",
        "instigated_context",
        "vcs_type",
        "branch_url",
        "logfilenames",
        "worker_name",
        "result_branches",
        "result_tags",
        "target_branch_url",
        "change_set",
        "failure_transient",
        "failure_stage",
        "codebase",
    ]

    def __init__(
        self,
        *,
        run_id,
        start_time,
        finish_time,
        command,
        description,
        result_code,
        value,
        main_branch_revision,
        revision,
        context,
        result,
        suite,
        instigated_context,
        vcs_type,
        branch_url,
        logfilenames,
        worker_name,
        result_branches,
        result_tags,
        target_branch_url,
        change_set,
        failure_transient,
        failure_stage,
        codebase,
    ) -> None:
        self.id = run_id
        self.start_time = start_time
        self.finish_time = finish_time
        self.command = command
        self.description = description
        self.result_code = result_code
        self.value = value
        self.main_branch_revision = main_branch_revision
        self.revision = revision
        self.context = context
        self.result = result
        self.suite = suite
        self.instigated_context = instigated_context
        self.vcs_type = vcs_type
        self.branch_url = branch_url
        self.logfilenames = logfilenames
        self.worker_name = worker_name
        if result_branches is None:
            self.result_branches = None
        else:
            self.result_branches = [
                (
                    role,
                    name,
                    br.encode("utf-8") if br else None,
                    r.encode("utf-8") if r else None,
                )  # type: ignore
                for (role, name, br, r) in result_branches
            ]
        if result_tags is None:
            self.result_tags = result_tags
        else:
            self.result_tags = [(name, r.encode("utf-8")) for (name, r) in result_tags]
        self.target_branch_url = target_branch_url
        self.change_set = change_set
        self.failure_transient = failure_transient
        self.failure_stage = failure_stage
        self.codebase = codebase

    @property
    def duration(self) -> datetime.timedelta:
        return self.finish_time - self.start_time

    def get_result_branch(self, role):
        return get_result_branch(self.result_branches, role)

    @property
    def campaign(self):
        return self.suite

    @classmethod
    def from_row(cls, row) -> "Run":
        return cls(
            run_id=row["id"],
            start_time=row["start_time"],
            finish_time=row["finish_time"],
            command=row["command"],
            description=row["description"],
            result_code=row["result_code"],
            main_branch_revision=(
                row["main_branch_revision"].encode("utf-8")
                if row["main_branch_revision"]
                else None
            ),
            revision=(row["revision"].encode("utf-8") if row["revision"] else None),
            context=row["context"],
            result=row["result"],
            value=row["value"],
            suite=row["suite"],
            instigated_context=row["instigated_context"],
            vcs_type=row["vcs_type"],
            branch_url=row["branch_url"],
            logfilenames=row["logfilenames"],
            worker_name=row["worker"],
            result_branches=row["result_branches"],
            result_tags=row["result_tags"],
            target_branch_url=row["target_branch_url"],
            change_set=row["change_set"],
            failure_transient=row["failure_transient"],
            failure_stage=row["failure_stage"],
            codebase=row["codebase"],
        )

    def __eq__(self, other) -> bool:
        if isinstance(other, Run):
            return self.id == other.id
        return False

    def __lt__(self, other) -> bool:
        if not isinstance(other, type(self)):
            raise TypeError(other)
        return self.id < other.id


async def iter_publishable_suites(conn: asyncpg.Connection, codebase: str) -> list[str]:
    query = """
SELECT DISTINCT suite
FROM publish_ready
WHERE codebase = $1
"""
    return [row[0] for row in await conn.fetch(query, codebase)]


async def has_cotenants(
    conn: asyncpg.Connection, codebase: str, url: str
) -> Optional[bool]:
    url = urlutils.split_segment_parameters(url)[0].rstrip("/")
    rows = await conn.fetch(
        "SELECT name FROM codebase where branch_url = $1 or url = $1",
        url,
    )
    if len(rows) > 1:
        return True
    elif len(rows) == 1 and rows[0][0] == codebase:
        return False
    else:
        # Uhm, we actually don't really know
        logging.warning("Unable to figure out if %s has cotenants on %s", codebase, url)
        return None


@middleware
async def asyncpg_error_middleware(request, handler):
    try:
        resp = await handler(request)
    except asyncpg.InsufficientResourcesError as e:
        insufficient_resources_counter.inc()
        traceback.print_exc()
        raise HTTPServiceUnavailable(text=str(e)) from e
    return resp
