#!/usr/bin/python3

from typing import Optional

import asyncpg
from yarl import URL

from .. import backend_url


async def write_merge_proposals(client, publisher_url: str, suite: Optional[str]):
    if suite:
        url = backend_url(publisher_url, suite, "merge-proposals")
    else:
        url = URL(publisher_url) / "merge-proposals"
    async with client.get(url, raise_for_status=True) as resp:
        proposals = await resp.json()

    proposals_by_status: dict[str, list[dict]] = {}
    for proposal in proposals:
        proposals_by_status.setdefault(proposal["status"], []).append(proposal)

    merged = proposals_by_status.get("merged", []) + proposals_by_status.get(
        "applied", []
    )
    return {
        "suite": suite,
        "open_proposals": proposals_by_status.get("open", []),
        "merged_proposals": merged,
        "closed_proposals": proposals_by_status.get("closed", []),
        "rejected_proposals": proposals_by_status.get("rejected", []),
        "abandoned_proposals": proposals_by_status.get("abandoned", []),
    }


async def get_proposal_with_run(conn: asyncpg.Connection, url: str):
    query = """
SELECT
    run.codebase AS codebase,
    run.suite AS suite,
    merge_proposal.url AS url,
    merge_proposal.status AS status,
    merge_proposal.merged_at AS merged_at,
    merge_proposal.merged_by AS merged_by,
    merge_proposal.last_scanned AS last_scanned,
    merge_proposal.can_be_merged AS can_be_merged
FROM
    merge_proposal
LEFT JOIN new_result_branch ON new_result_branch.revision = merge_proposal.revision
LEFT JOIN run ON run.id = new_result_branch.run_id
WHERE url = $1
"""
    return await conn.fetchrow(query, url)


async def get_publishes(conn, url):
    return await conn.fetch(
        """
SELECT * FROM publish WHERE merge_proposal_url = $1
ORDER BY timestamp DESC
""",
        url,
    )


async def write_merge_proposal(db, url):
    async with db.acquire() as conn:
        proposal = await get_proposal_with_run(conn, url)

        publishes = await get_publishes(conn, url)

    return {
        "proposal": proposal,
        "publishes": publishes,
    }
