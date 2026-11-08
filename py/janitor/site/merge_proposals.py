#!/usr/bin/python3

from datetime import datetime
from typing import Optional

from yarl import URL


async def write_merge_proposals(client, publisher_url: str, suite: Optional[str]):
    if suite:
        url = URL(publisher_url) / suite / "merge-proposals"
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
        "campaign": suite,
        "open_proposals": proposals_by_status.get("open", []),
        "merged_proposals": merged,
        "closed_proposals": proposals_by_status.get("closed", []),
        "rejected_proposals": proposals_by_status.get("rejected", []),
        "abandoned_proposals": proposals_by_status.get("abandoned", []),
    }


async def _fetch_proposal_detail(client, publisher_url: str, url: str):
    api_url = URL(publisher_url) / "merge-proposal"
    async with client.get(api_url, params={"url": url}) as resp:
        if resp.status == 404:
            return None
        resp.raise_for_status()
        proposal = await resp.json()
    for key in ("merged_at", "last_scanned"):
        value = proposal.get(key)
        if value is not None:
            proposal[key] = datetime.fromisoformat(value)
    return proposal


async def _fetch_publishes(client, publisher_url: str, url: str):
    api_url = URL(publisher_url) / "publish"
    async with client.get(
        api_url, params={"merge_proposal_url": url}, raise_for_status=True
    ) as resp:
        data = await resp.json()
    publishes = []
    for entry in reversed(data["publishes"]):
        entry = dict(entry)
        ts = entry.get("timestamp")
        if ts is not None:
            entry["timestamp"] = datetime.fromisoformat(ts)
        publishes.append(entry)
    return publishes


async def write_merge_proposal(client, publisher_url: str, url: str):
    proposal = await _fetch_proposal_detail(client, publisher_url, url)
    publishes = await _fetch_publishes(client, publisher_url, url)

    return {
        "proposal": proposal,
        "publishes": publishes,
    }
