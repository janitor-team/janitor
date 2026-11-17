from datetime import datetime
from typing import Optional

from aiohttp import web
from yarl import URL


def _parse_timestamp(value):
    return datetime.fromisoformat(value) if value is not None else None


async def write_history(client, publisher_url: str, limit: Optional[int] = None):
    url = URL(publisher_url) / "publish"
    params = {}
    if limit is not None:
        params["limit"] = str(limit)
    async with client.get(url, params=params, raise_for_status=True) as resp:
        data = await resp.json()

    history = [
        (
            entry["id"],
            _parse_timestamp(entry.get("timestamp")),
            entry.get("branch_name"),
            entry["mode"],
            entry.get("merge_proposal_url"),
            entry["result_code"],
            entry.get("description"),
            entry.get("codebase_web_url"),
            entry.get("codebase"),
        )
        for entry in data["publishes"]
    ]
    return {"count": limit, "history": history}


async def write_publish(client, publisher_url: str, publish_id: str):
    url = URL(publisher_url) / "publish" / publish_id
    async with client.get(url) as resp:
        if resp.status == 404:
            raise web.HTTPNotFound(text=f"no such publish: {publish_id}")
        resp.raise_for_status()
        publish = await resp.json()
    publish["id"] = publish_id
    publish["timestamp"] = _parse_timestamp(publish.get("timestamp"))
    return {"publish": publish}
