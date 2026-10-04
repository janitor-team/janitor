#!/usr/bin/python
# Copyright (C) 2018 Jelmer Vernooij <jelmer@jelmer.uk>
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

from datetime import timedelta
from typing import Any, Optional

import asyncpg


class QueueItem:
    __slots__ = [
        "id",
        "context",
        "command",
        "estimated_duration",
        "campaign",
        "refresh",
        "requester",
        "change_set",
        "codebase",
    ]

    def __init__(
        self,
        *,
        id,
        context,
        command,
        estimated_duration,
        campaign,
        refresh,
        requester,
        change_set,
        codebase,
    ) -> None:
        self.id = id
        self.context = context
        self.command = command
        self.estimated_duration = estimated_duration
        self.campaign = campaign
        self.refresh = refresh
        self.requester = requester
        self.change_set = change_set
        self.codebase = codebase

    @classmethod
    def from_row(cls, row) -> "QueueItem":
        return cls(
            id=row["id"],
            context=row["context"],
            command=row["command"],
            estimated_duration=row["estimated_duration"],
            campaign=row["campaign"],
            refresh=row["refresh"],
            requester=row["requester"],
            change_set=row["change_set"],
            codebase=row["codebase"],
        )

    def __eq__(self, other):
        if isinstance(other, QueueItem):
            return self.id == other.id
        return False

    def __lt__(self, other):
        return self.id < other.id

    def __hash__(self):
        return hash((type(self), self.id))


class Queue:
    def __init__(self, conn: asyncpg.Connection) -> None:
        self.conn = conn

    async def get_position(
        self, campaign: str, codebase: str
    ) -> tuple[Optional[int], Optional[timedelta]]:
        row = await self.conn.fetchrow(
            "SELECT position, wait_time FROM queue_positions "
            "WHERE codebase = $1 AND suite = $2",
            codebase,
            campaign,
        )
        if not row:
            return (None, None)
        return row

    async def get_item(self, queue_id: int):
        query = """
SELECT
    queue.command AS command,
    queue.context AS context,
    queue.id AS id,
    queue.estimated_duration AS estimated_duration,
    queue.suite AS campaign,
    queue.refresh AS refresh,
    queue.requester AS requester,
    queue.change_set AS change_set,
    queue.codebase AS codebase
FROM
    queue
WHERE queue.id = $1
"""
        row = await self.conn.fetchrow(query, queue_id)
        if row:
            return QueueItem.from_row(row)
        return None

    async def next_item(
        self,
        codebase: Optional[str] = None,
        campaign: Optional[str] = None,
        exclude_hosts: Optional[set[str]] = None,
        assigned_queue_items: Optional[set[int]] = None,
    ) -> tuple[Optional[QueueItem], dict[str, str]]:
        query = """
SELECT
    queue.command AS command,
    queue.context AS context,
    queue.id AS id,
    queue.estimated_duration AS estimated_duration,
    queue.suite AS campaign,
    queue.refresh AS refresh,
    queue.requester AS requester,
    queue.change_set AS change_set,
    codebase.vcs_type AS vcs_type,
    codebase.branch_url AS branch_url,
    codebase.subpath AS subpath,
    queue.codebase AS codebase
FROM
    queue
LEFT JOIN codebase ON codebase.name = queue.codebase
"""
        conditions = []
        args: list[Any] = []
        if assigned_queue_items:
            args.append(assigned_queue_items)
            conditions.append(f"NOT (queue.id = ANY(${len(args)}::int[]))")
        if codebase:
            args.append(codebase)
            conditions.append(f"queue.codebase = ${len(args)}")
        if campaign:
            args.append(campaign)
            conditions.append(f"queue.suite = ${len(args)}")
        if exclude_hosts:
            args.append(exclude_hosts)
            # TODO(jelmer): Use codebase.hostname when kali upgrades to postgres 12+
            conditions.append(
                f"NOT (codebase.branch_url IS NOT NULL AND SUBSTRING(codebase.branch_url from '.*://(?:[^/@]*@)?([^/]*)') = ANY(${len(args)}::text[]))"
            )

        if conditions:
            query += " WHERE " + " AND ".join(conditions)

        query += """
ORDER BY
queue.bucket ASC,
queue.priority ASC,
queue.id ASC
LIMIT 1
"""
        row = await self.conn.fetchrow(query, *args)
        if row is None:
            return None, {}
        vcs_info = {}
        if row["branch_url"]:
            vcs_info["branch_url"] = row["branch_url"]
        # subpath is a required (non-Option) PathBuf worker-side
        vcs_info["subpath"] = row["subpath"] or ""
        if row["vcs_type"]:
            vcs_info["vcs_type"] = row["vcs_type"]
        return QueueItem.from_row(row), vcs_info

    async def iter_queue(
        self, limit: Optional[int] = None, campaign: Optional[str] = None
    ):
        query = """
SELECT
    queue.command AS command,
    queue.context AS context,
    queue.id AS id,
    queue.estimated_duration AS estimated_duration,
    queue.suite AS campaign,
    queue.refresh AS refresh,
    queue.requester AS requester,
    queue.change_set AS change_set,
    queue.codebase AS codebase
FROM
    queue
"""
        conditions = []
        args: list[Any] = []
        if campaign:
            args.append(campaign)
            conditions.append(f"queue.suite = ${len(args)}")

        if conditions:
            query += " WHERE " + " AND ".join(conditions)

        query += """
ORDER BY
queue.bucket ASC,
queue.priority ASC,
queue.id ASC
"""
        args.append(limit)
        query += f" LIMIT ${len(args)}"
        for row in await self.conn.fetch(query, *args):
            yield QueueItem.from_row(row)

    async def add(
        self,
        *,
        codebase: str,
        command: str,
        campaign: str,
        change_set: Optional[str] = None,
        offset: float = 0.0,
        bucket: str = "default",
        context: Optional[str] = None,
        estimated_duration: Optional[timedelta] = None,
        refresh: bool = False,
        requester: Optional[str] = None,
    ) -> tuple[int, str]:
        row = await self.conn.fetchrow(
            "INSERT INTO queue "
            "(command, priority, bucket, context, "
            "estimated_duration, suite, refresh, requester, change_set, "
            "codebase) VALUES ($1, "
            "(SELECT COALESCE(MIN(priority), 0) FROM queue)"
            + " + $2, $3, $4, $5, $6, $7, $8, $9, $10) "
            "ON CONFLICT (codebase, suite, coalesce(change_set, ''::text)) "
            "DO UPDATE SET "
            "context = EXCLUDED.context, priority = EXCLUDED.priority, "
            "bucket = EXCLUDED.bucket, "
            "estimated_duration = EXCLUDED.estimated_duration, "
            "refresh = EXCLUDED.refresh, requester = EXCLUDED.requester, "
            "command = EXCLUDED.command, codebase = EXCLUDED.codebase "
            "WHERE queue.bucket >= EXCLUDED.bucket OR "
            "(queue.bucket = EXCLUDED.bucket AND "
            "queue.priority >= EXCLUDED.priority) RETURNING id, bucket",
            command,
            offset,
            bucket,
            context,
            estimated_duration,
            campaign,
            refresh,
            requester,
            change_set,
            codebase,
        )
        if row is None:
            # Nothing has changed? TODO(jelmer): Avoid a second query in
            # this case.
            row = await self.conn.fetchrow(
                "SELECT id, bucket FROM queue "
                "WHERE codebase = $1 AND suite = $2 "
                "AND coalesce(change_set, '') = $3",
                codebase,
                campaign,
                change_set or "",
            )
            assert row, (
                f"Unable to add or retrieve queue entry for {campaign}/{codebase}/{change_set}"
            )
            return row
        return row

    async def get_buckets(self):
        return await self.conn.fetch(
            "SELECT bucket, count(*) FROM queue GROUP BY bucket ORDER BY bucket ASC"
        )
