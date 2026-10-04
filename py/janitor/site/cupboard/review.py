#!/usr/bin/python3

import logging
from asyncio import TimeoutError

import aiozipkin
import asyncpg
from aiohttp import ClientConnectorError, ClientResponseError, web
from breezy.revision import NULL_REVISION

from janitor import state

from ..common import render_template_for_request
from . import iter_needs_review


async def generate_rejected(conn, config, campaign=None):
    if campaign is None:
        campaigns = [c.name for c in config.campaign]
    else:
        campaigns = [campaign]

    runs = await conn.fetch(
        "SELECT id, suite, codebase FROM last_unabsorbed_runs "
        "WHERE publish_status = 'rejected' AND suite = ANY($1::text[]) "
        "ORDER BY finish_time DESC",
        campaigns,
    )

    reviews: dict[str, list[asyncpg.Record]] = {}

    for row in await conn.fetch(
        "SELECT * FROM review WHERE run_id = ANY($1::text[])",
        [run["id"] for run in runs],
    ):
        reviews.setdefault(row["run_id"], []).append(row)

    return {"runs": runs, "suite": campaign, "reviews": reviews}


async def generate_review(
    conn,
    request,
    client,
    differ_url,
    vcs_managers,
    campaigns=None,
    publishable_only=True,
    required_only=None,
):
    limit = int(request.query.get("limit", "100"))

    span = aiozipkin.request_span(request)

    if request.get("user"):
        reviewer = request["user"].get("email")
    else:
        reviewer = None

    with span.new_child("sql:needs-review"):
        entries = await iter_needs_review(
            conn,
            limit=limit,
            campaigns=campaigns,
            publishable_only=publishable_only,
            required_only=required_only,
            reviewer=reviewer,
        )
    if not entries:
        return await render_template_for_request(
            "cupboard/review-done.html", request, {"publishable_only": publishable_only}
        )

    (
        run_id,
        codebase,
        campaign,
    ) = entries.pop(0)

    evaluate_url = str(request.url.join(request.app["evaluate_url"]))

    try:
        async with request.app["http_client_session"].get(
            evaluate_url.replace("RUN_ID", run_id), raise_for_status=True
        ) as resp:
            evaluate = await resp.text()
    except (ClientConnectorError, ClientResponseError) as e:
        evaluate = f"Unable to retrieve evaluation: {e}"

    kwargs = {
        "review_instructions_url": request.app.get("review_instructions_url"),
        "codebase": codebase,
        "run_id": run_id,
        "suite": campaign,
        "suites": campaigns,
        "campaign": campaign,
        "campaigns": campaigns,
        "evaluate": evaluate,
        "evaluate_url": str(request.app["evaluate_url"]),
        "publishable_only": publishable_only,
        "required_only": required_only,
        "todo": [
            {
                "codebase": entry["codebase"],
                "id": entry["id"],
            }
            for entry in entries
        ],
    }
    return await render_template_for_request("cupboard/review.html", request, kwargs)


async def generate_evaluate(
    db, vcs_managers, http_client_session, differ_url, run_id, span
):
    async with db.acquire() as conn:
        run = await conn.fetchrow(
            "SELECT codebase, array(SELECT row(role, remote_name, base_revision, revision) "
            "FROM new_result_branch WHERE run_id = id) AS result_branches, vcs_type, main_branch_revision, "
            "finish_time, value, command, suite AS campaign FROM run WHERE id = $1",
            run_id,
        )
    if run is None:
        raise web.HTTPNotFound(text=f"No run with id {run_id!r}")

    async def show_diff(role):
        try:
            (remote_name, base_revid, revid) = state.get_result_branch(
                run["result_branches"], role
            )
        except KeyError:
            return ""
        external_url = f"/api/run/{run_id}/diff?role={role}"
        if run["vcs_type"] is None:
            return "no vcs known"
        if revid is None:
            return "Branch deleted"
        try:
            with span.new_child("vcs-diff"):
                return (
                    await vcs_managers[run["vcs_type"]].get_diff(
                        run["codebase"],
                        base_revid.encode("utf-8") if base_revid else NULL_REVISION,
                        revid.encode("utf-8"),
                    )
                ).decode("utf-8", "replace")
        except ClientResponseError as e:
            return f"Unable to retrieve diff; error code {e.status}"
        except NotImplementedError as e:
            return str(e)
        except ClientConnectorError as e:
            return f"Unable to retrieve diff; error {e}"
        except TimeoutError:
            return f"Timeout while retrieving diff; see it at {external_url}"

    async def get_revision_info(role):
        try:
            (remote_name, base_revid, revid) = state.get_result_branch(
                run["result_branches"], role
            )
        except KeyError:
            return []

        if base_revid == revid:
            return []
        if run["vcs_type"] is None:
            logging.warning("No vcs known for run %s", run_id)
            return []
        if revid is None:
            return []
        old_revid = base_revid.encode("utf-8") if base_revid else NULL_REVISION
        new_revid = revid.encode("utf-8")
        try:
            return await vcs_managers[run["vcs_type"]].get_revision_info(
                run["codebase"], old_revid, new_revid
            )
        except ClientResponseError as e:
            logging.warning("Unable to retrieve commit info; error code %d", e.status)
            return []
        except ClientConnectorError as e:
            logging.warning("Unable to retrieve diff; error %s", e)
            return []
        except TimeoutError:
            logging.warning("Timeout while retrieving commit info")
            return []

    return {
        "run_id": run_id,
        "finish_time": run["finish_time"],
        "campaign": run["campaign"],
        "branches": run["result_branches"],
        "value": run["value"],
        "command": run["command"],
        "show_diff": show_diff,
        "get_revision_info": get_revision_info,
    }
