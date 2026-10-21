#!/usr/bin/python3
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

from typing import Optional

from aiohttp_openmetrics import Gauge
from yarl import URL

from .schedule import do_schedule


async def store_review(
    conn,
    session,
    runner_url,
    run_id: str,
    verdict: str,
    comment: Optional[str],
    reviewer: Optional[str],
    is_qa_reviewer: bool,
):
    async with conn.transaction():
        if verdict == "reschedule":
            verdict = "rejected"

            run = await conn.fetchrow(
                "SELECT suite, codebase FROM run WHERE id = $1", run_id
            )
            await do_schedule(
                conn,
                campaign=run["suite"],
                refresh=True,
                requester=f"reviewer ({reviewer})",
                bucket="default",
                codebase=run["codebase"],
            )

        if verdict != "abstained" and is_qa_reviewer:
            async with session.post(
                URL(runner_url) / "runs" / run_id,
                json={"publish_status": verdict},
                raise_for_status=True,
            ):
                pass
        await conn.execute(
            "INSERT INTO review (run_id, comment, reviewer, verdict) VALUES "
            " ($1, $2, $3, $4) ON CONFLICT (run_id, reviewer) "
            "DO UPDATE SET verdict = EXCLUDED.verdict, comment = EXCLUDED.comment, "
            "reviewed_at = NOW()",
            run_id,
            comment,
            reviewer,
            verdict,
        )


stored_reviews = Gauge(
    "stored_reviews",
    "Reviews in the review table, by verdict and reviewer",
    labelnames=("verdict", "reviewer"),
)


async def update_stored_reviews(conn):
    rows = await conn.fetch(
        "SELECT verdict, reviewer, count(*) FROM review GROUP BY verdict, reviewer"
    )
    stored_reviews.clear()
    for row in rows:
        stored_reviews.labels(verdict=row[0], reviewer=row[1] or "").set(row[2])
