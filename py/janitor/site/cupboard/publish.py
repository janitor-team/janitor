from typing import Optional

from aiohttp import web


async def iter_publish_history(conn, limit: Optional[int] = None):
    query = """
SELECT
    publish.id, publish.timestamp, publish.branch_name,
    publish.mode, publish.merge_proposal_url, publish.result_code,
    publish.description, codebase.web_url, publish.codebase AS codebase
FROM
    publish
LEFT JOIN codebase ON codebase.name = publish.codebase
ORDER BY timestamp DESC
LIMIT $1
"""
    return await conn.fetch(query, limit)


async def write_history(conn, limit: Optional[int] = None):
    return {
        "count": limit,
        "history": await iter_publish_history(conn, limit=limit),
    }


async def write_publish(conn, publish_id):
    query = """
SELECT
    publish.id AS id,
    publish.timestamp AS timestamp,
    publish.branch_name AS branch_name,
    publish.mode AS mode,
    publish.merge_proposal_url AS merge_proposal_url,
    publish.result_code AS result_code,
    publish.description AS description,
    codebase.web_url AS vcs_browse,
    codebase.name AS codebase
FROM
    publish
LEFT JOIN codebase ON codebase.name = publish.codebase
WHERE id = $1
"""
    publish = await conn.fetchrow(query, publish_id)
    if publish is None:
        raise web.HTTPNotFound(text=f"no such publish: {publish_id}")
    return {"publish": publish}
