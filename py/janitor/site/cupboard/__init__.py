#!/usr/bin/python
# Copyright (C) 2019-2022 Jelmer Vernooij <jelmer@jelmer.uk>
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

"""Serve the Janitor cupboard site."""

import re
from datetime import date, datetime
from typing import Any, Optional

import aiohttp_jinja2
import aiozipkin
import asyncpg
from aiohttp import ClientSession, web
from aiohttp.web_middlewares import normalize_path_middleware
from jinja2 import select_autoescape

from janitor.site import template_loader
from janitor.vcs import get_vcs_managers_from_config

from .. import check_logged_in, is_admin, is_qa_reviewer, worker_link_is_global
from ..common import html_template
from ..pkg import MergeProposalUserUrlResolver
from ..setup import setup_postgres

routes = web.RouteTableDef()


@routes.get("/cupboard/rejected", name="cupboard-rejected")
@html_template("cupboard/rejected.html")
async def handle_rejected(request):
    from .review import generate_rejected

    campaign = request.query.get("campaign")
    async with request.app["pool"].acquire() as conn:
        return await generate_rejected(conn, request.app["config"], campaign=campaign)


@routes.get("/cupboard/history", name="history")
@html_template("cupboard/history.html", headers={"Vary": "Cookie"})
async def handle_history(request):
    limit = int(request.query.get("limit", "100"))
    offset = int(request.query.get("offset", "0"))

    query = """\
SELECT finish_time, codebase, suite, worker_link,
worker as worker_name, finish_time - start_time AS duration,
result_code, id, description, failure_transient FROM run
ORDER BY finish_time DESC"""
    if offset:
        query += f" OFFSET {offset}"
    if limit:
        query += f" LIMIT {limit}"
    async with request.app["pool"].acquire() as conn:
        runs = await conn.fetch(query)
    return {"count": limit, "history": runs}


@routes.get("/cupboard/reprocess-logs", name="reprocess-logs")
@html_template("cupboard/reprocess-logs.html")
async def handle_reprocess_logs(request):
    async with request.app["pool"].acquire() as conn:
        count = await conn.fetchval(
            """
SELECT count(*) FROM run
WHERE
  (result_code = 'build-failed' OR
   result_code LIKE 'build-failed-stage-%' OR
   result_code LIKE 'autopkgtest-%' OR
   result_code LIKE 'build-%' OR
   result_code LIKE 'dist-%' OR
   result_code LIKE 'unpack-%' OR
   result_code LIKE 'create-session-%' OR
   result_code LIKE 'missing-%')
"""
        )
    return {"matching_run_count": count}


@routes.get("/cupboard/workers", name="workers")
@html_template("cupboard/workers.html", headers={"Vary": "Cookie"})
async def handle_workers(request):
    async with request.app["pool"].acquire() as conn:
        workers = []
        for worker in await conn.fetch(
            "select name, link, count(run.id) as run_count from worker "
            "left join run on run.worker = worker.name "
            "group by worker.name, worker.link"
        ):
            worker = dict(worker)
            if not worker_link_is_global(worker["link"]):
                worker["link"] = None
            workers.append(worker)
        return {"workers": workers}


@routes.get("/cupboard/queue", name="queue")
@html_template("cupboard/queue.html", headers={"Vary": "Cookie"})
async def handle_queue(request):
    limit = int(request.query.get("limit", "100"))
    from .queue import write_queue

    return await write_queue(
        request.app["pool"],
        queue_status=request.app["runner_status"],
        limit=limit,
    )


@routes.get("/cupboard/never-processed", name="never-processed")
@html_template("cupboard/never-processed.html", headers={"Vary": "Cookie"})
async def handle_never_processed(request):
    campaign = request.query.get("campaign")
    if campaign is not None and campaign.lower() == "_all":
        campaign = None
    campaigns = [campaign] if campaign else None
    async with request.app["pool"].acquire() as conn:
        query = """\
        select c.codebase, c.suite from candidate c
        where not exists (
            SELECT FROM run WHERE run.codebase = c.codebase AND c.suite = suite)
        """
        args = []
        if campaigns:
            query += " AND suite = ANY($1::text[])"
            args.append(campaigns)
        return {
            "never_processed": await conn.fetch(query, *args),
            "campaign": campaign,
        }


@routes.get("/cupboard/result-codes/", name="result-code-list")
@html_template("cupboard/result-code-index.html", headers={"Vary": "Cookie"})
async def handle_result_codes(request):
    campaign = request.query.get("campaign")
    exclude_never_processed = request.query.get("exclude_never_processed") == "on"
    include_transient = request.query.get("include_transient") == "on"
    include_historical = request.query.get("include_historical") == "on"
    if campaign is not None and campaign.lower() == "_all":
        campaign = None
    all_campaigns = [c.name for c in request.app["config"].campaign]
    args = [[campaign] if campaign else all_campaigns]
    async with request.app["pool"].acquire() as conn:
        if include_transient:
            query = """\
    select (
            case when result_code = 'nothing-new-to-do' then 'success'
            else result_code end), count(result_code) from last_runs AS run
    """
        else:
            query = """\
    select result_code, count(result_code) from last_effective_runs AS run
    """
        query += " where suite = ANY($1::text[])"
        if not include_historical:
            query += (
                " AND EXISTS (SELECT FROM candidate WHERE "
                "run.codebase = candidate.codebase AND "
                "run.suite = candidate.suite AND "
                "(run.change_set = candidate.change_set OR candidate.change_set is NULL))"
            )
        query += " group by 1"
        if not exclude_never_processed:
            query = f"""({query}) union
    select 'never-processed', count(*) from candidate c
        where not exists (
            SELECT FROM run WHERE run.codebase = c.codebase AND c.suite = suite)
        and suite = ANY($1::text[]) order by 2 desc
    """
        return {
            "exclude_never_processed": exclude_never_processed,
            "include_transient": include_transient,
            "include_historical": include_historical,
            "result_codes": await conn.fetch(query, *args),
            "campaign": campaign,
            "all_campaigns": all_campaigns,
        }


@routes.get("/cupboard/failure-stages/", name="failure-stage-list")
@html_template("cupboard/failure-stage-index.html", headers={"Vary": "Cookie"})
async def handle_failure_stages(request):
    campaign = request.query.get("campaign")
    include_transient = request.query.get("include_transient") == "on"
    include_historical = request.query.get("include_historical") == "on"
    if campaign is not None and campaign.lower() == "_all":
        campaign = None
    all_campaigns = [c.name for c in request.app["config"].campaign]
    args = [[campaign] if campaign else all_campaigns]
    async with request.app["pool"].acquire() as conn:
        if include_transient:
            query = """\
    select failure_stage, count(failure_stage) from last_runs AS run
    """
        else:
            query = """\
    select failure_stage, count(failure_stage) from last_effective_runs AS run
    """
        query += " where suite = ANY($1::text[])"
        if not include_historical:
            query += (
                " AND EXISTS (SELECT FROM candidate WHERE "
                "run.codebase = candidate.codebase AND "
                "run.suite = candidate.suite AND "
                "(run.change_set = candidate.change_set OR candidate.change_set is NULL))"
            )
        query += " group by 1"
        return {
            "include_transient": include_transient,
            "include_historical": include_historical,
            "failure_stages": await conn.fetch(query, *args),
            "campaign": campaign,
            "all_campaigns": all_campaigns,
        }


@routes.get("/cupboard/result-codes/{code}", name="result-code")
@html_template("cupboard/result-code.html", headers={"Vary": "Cookie"})
async def handle_result_code(request):
    campaign = request.query.get("campaign")
    include_transient = request.query.get("include_transient") == "on"
    include_historical = request.query.get("include_historical") == "on"
    if campaign is not None and campaign.lower() == "_all":
        campaign = None
    code = request.match_info["code"]
    if code == "success":
        table = "last_runs"
        codes = ["success", "nothing-new-to-do"]
    else:
        codes = [code]
        if include_transient:
            table = "last_runs"
        else:
            table = "last_effective_runs"
    query = (
        f"SELECT *, suite AS campaign FROM {table} AS run "
        "WHERE result_code = ANY($1::text[]) AND suite = ANY($2::text[])"
    )
    if not include_historical:
        query += (
            " AND EXISTS (SELECT FROM candidate WHERE "
            "run.codebase = candidate.codebase AND "
            "run.suite = candidate.suite AND "
            "(run.change_set = candidate.change_set OR candidate.change_set IS NULL))"
        )
    all_campaigns = [c.name for c in request.app["config"].campaign]
    async with request.app["pool"].acquire() as conn:
        return {
            "code": code,
            "runs": await conn.fetch(
                query, codes, [campaign] if campaign else all_campaigns
            ),
            "campaign": campaign,
            "include_historical": include_historical,
            "include_transient": include_transient,
            "all_campaigns": all_campaigns,
        }


@routes.get("/cupboard/publish/{id}", name="publish")
@html_template("cupboard/publish.html")
async def handle_publish(request):
    id = request.match_info["id"]
    from .publish import write_publish

    async with request.app["pool"].acquire() as conn:
        return await write_publish(conn, id)


@routes.get("/cupboard/publish/", name="publish-history")
@html_template("cupboard/publish-history.html", headers={"Vary": "Cookie"})
async def handle_publish_history(request):
    limit = int(request.query.get("limit", "100"))
    from .publish import write_history

    async with request.app["pool"].acquire() as conn:
        return await write_history(conn, limit=limit)


@routes.post("/cupboard/review", name="cupboard-review-post")
async def handle_review_post(request):
    from ...review import store_review
    from .review import generate_review

    check_logged_in(request)

    post = await request.post()
    publishable_only = post.get("publishable_only", "true") == "true"
    async with request.app["pool"].acquire() as conn:
        if "verdict" in post:
            verdict = {
                "approve": "approved",
                "reject": "rejected",
                "reschedule": "rescheduled",
                "abstain": "abstained",
            }[post["verdict"].lower()]
            review_comment = post.get("review_comment")
            try:
                user = request["user"]["email"]
            except KeyError:
                user = request["user"]["name"]
            await store_review(
                conn,
                request.app["http_client_session"],
                request.app["runner_url"],
                post["run_id"],
                verdict=verdict,
                comment=review_comment,
                reviewer=user,
                is_qa_reviewer=is_qa_reviewer(request),
            )
        text = await generate_review(
            conn,
            request,
            request.app["http_client_session"],
            request.app["differ_url"],
            request.app["vcs_managers"],
            campaigns=post.getall("suite", None),
            publishable_only=publishable_only,
        )
        return web.Response(
            content_type="text/html",
            text=text,
            headers={"Cache-Control": "no-cache"},
        )


@routes.get("/cupboard/review", name="cupboard-review")
async def handle_review(request):
    from .review import generate_review

    publishable_only = request.query.get("publishable_only", "true") == "true"
    if "required_only" in request.query:
        required_only = request.query["required_only"] == "true"
    else:
        required_only = True

    campaigns = request.query.getall("suite", None)
    async with request.app["pool"].acquire() as conn:
        text = await generate_review(
            conn,
            request,
            request.app["http_client_session"],
            request.app["differ_url"],
            request.app["vcs_managers"],
            campaigns=campaigns,
            publishable_only=publishable_only,
            required_only=required_only,
        )
    return web.Response(
        content_type="text/html", text=text, headers={"Cache-Control": "no-cache"}
    )


@routes.get("/cupboard/c/{codebase}/{run_id}/", name="cupboard-run")
@html_template("cupboard/run.html", headers={"Vary": "Cookie"})
async def handle_run(request):
    from ..common import get_run
    from ..pkg import generate_run_file

    span = aiozipkin.request_span(request)
    run_id = request.match_info["run_id"]
    codebase = request.match_info.get("codebase")
    async with request.app["pool"].acquire() as conn:
        with span.new_child("sql:run"):
            run = await get_run(conn, run_id)
            if run is None:
                raise web.HTTPNotFound(text=f"No run with id {run_id!r}")
    if codebase is not None and codebase != run["codebase"]:
        if run is None:
            raise web.HTTPNotFound(text=f"No run with id {run_id!r}")
    return await generate_run_file(
        request.app["pool"],
        request.app["http_client_session"],
        request.app["config"],
        request.app["differ_url"],
        request.app["publisher_url"],
        request.app["logfile_manager"],
        run,
        request.app["vcs_managers"],
        is_admin=is_admin(request),
        span=span,
    )


@routes.get("/cupboard/broken-merge-proposals", name="broken-mps")
@html_template("cupboard/broken-merge-proposals.html", headers={"Vary": "Cookie"})
async def handle_broken_mps(request):
    async with request.app["pool"].acquire() as conn:
        broken_mps = await conn.fetch(
            """\
SELECT
  current_run.url,
  last_run.suite,
  last_run.codebase,
  last_run.id,
  last_run.result_code,
  last_run.finish_time,
  last_run.description
FROM
  (SELECT DISTINCT ON (merge_proposal.url)
     merge_proposal.url,
     run.suite,
     run.codebase,
     run.finish_time,
     merge_proposal.revision AS current_revision
   FROM merge_proposal
   JOIN run ON merge_proposal.revision = run.revision
   WHERE merge_proposal.status = 'open'
  ) AS current_run
LEFT JOIN last_runs AS last_run
ON
  current_run.suite = last_run.suite
  AND current_run.codebase = last_run.codebase
WHERE
  last_run.result_code NOT IN ('success', 'nothing-to-do', 'nothing-new-to-do')
ORDER BY
  current_run.url,
  last_run.finish_time DESC;
"""
        )

    return {"broken_mps": broken_mps}


@routes.get("/cupboard/", name="cupboard-start")
@html_template("cupboard/start.html")
async def handle_cupboard_start(request):
    return {"extra_cupboard_links": _extra_cupboard_links}


@routes.get("/cupboard/cs/{id}/", name="cupboard-changeset")
@html_template("cupboard/changeset.html", headers={"Vary": "Cookie"})
async def handle_changeset(request):
    span = aiozipkin.request_span(request)
    async with request.app["pool"].acquire() as conn:
        with span.new_child("sql:changeset"):
            cs = await conn.fetchrow(
                "SELECT * FROM change_set WHERE id = $1", request.match_info["id"]
            )
        with span.new_child("sql:runs"):
            runs = await conn.fetch(
                "SELECT * FROM run WHERE change_set = $1 ORDER BY finish_time DESC",
                request.match_info["id"],
            )
        with span.new_child("sql:todo"):
            todo = await conn.fetch(
                "SELECT * FROM change_set_todo WHERE change_set = $1",
                request.match_info["id"],
            )
    return {"changeset": cs, "runs": runs, "todo": todo}


@routes.get("/cupboard/cs/", name="cupboard-changeset-list")
@html_template("cupboard/changeset-list.html", headers={"Vary": "Cookie"})
async def handle_changeset_list(request):
    span = aiozipkin.request_span(request)
    async with request.app["pool"].acquire() as conn:
        with span.new_child("sql:changesets"):
            cs = await conn.fetch(
                """\
select * from change_set where exists (
    select from candidate where change_set = change_set.id)
"""
            )
    return {"changesets": cs}


@routes.get("/cupboard/run/{run_id}/", name="cupboard-run-redirect")
async def handle_run_redirect(request):
    run_id = request.match_info["run_id"]

    async with request.app["pool"].acquire() as conn:
        codebase = await conn.fetchval("SELECT codebase FROM run WHERE id = $1", run_id)
        if codebase is None:
            raise web.HTTPNotFound(text=f"No such run: {run_id}")
        raise web.HTTPPermanentRedirect(
            location=request.app.router["cupboard-run"].url_for(
                codebase=codebase, run_id=run_id
            )
        )


@routes.get("/cupboard/c/{codebase}/", name="cupboard-codebase-redirect")
async def handle_codebase_redirect(request):
    codebase = request.match_info["codebase"]

    async with request.app["pool"].acquire() as conn:
        run_id = await conn.fetchval(
            "SELECT id FROM run WHERE codebase = $1 "
            "ORDER BY finish_time DESC NULLS LAST, start_time DESC LIMIT 1",
            codebase,
        )
        if run_id is None:
            raise web.HTTPNotFound(text=f"No runs yet for codebase: {codebase}")
        # A temporary redirect, not permanent - the most recent run for a
        # codebase changes every time a new one completes, so this target
        # must never be cached indefinitely by a client.
        raise web.HTTPFound(
            location=request.app.router["cupboard-run"].url_for(
                codebase=codebase, run_id=run_id
            )
        )


@routes.get("/cupboard/merge-proposals", name="cupboard-merge-proposals")
@html_template("cupboard/merge-proposals.html", headers={"Vary": "Cookie"})
async def handle_merge_proposals(request):
    from .merge_proposals import write_merge_proposals

    suite = request.match_info.get("suite")
    return await write_merge_proposals(request.app["pool"], suite)


@routes.get("/cupboard/merge-proposal", name="cupboard-merge-proposal")
@html_template("cupboard/merge-proposal.html", headers={"Vary": "Cookie"})
async def handle_merge_proposal(request):
    from .merge_proposals import write_merge_proposal

    try:
        url = request.query["url"]
    except KeyError as e:
        raise web.HTTPBadRequest(text="no url specified") from e
    return await write_merge_proposal(request.app["pool"], url)


@routes.get(
    "/cupboard/c/{codebase}/{run_id}/{filename:.+}", name="cupboard-result-file"
)
async def handle_result_file(request):
    codebase = request.match_info["codebase"]
    filename = request.match_info["filename"]
    run_id = request.match_info["run_id"]
    if not re.match("^[a-z0-9+-\\.]+$", codebase) or len(codebase) < 2:
        raise web.HTTPNotFound(text=f"Invalid codebase {codebase} for run {run_id}")
    if not re.match("^[a-z0-9-]+$", run_id) or len(run_id) < 5:
        raise web.HTTPNotFound(text=f"Invalid run run id {run_id}")
    if filename.endswith(".log") or re.match(r".*\.log\.[0-9]+", filename):
        if not re.match("^[+a-z0-9\\.]+$", filename) or len(filename) < 3:
            raise web.HTTPNotFound(text=f"No log file {filename} for run {run_id}")

        try:
            logfile = await request.app["logfile_manager"].get_log(
                codebase, run_id, filename
            )
        except FileNotFoundError as e:
            raise web.HTTPNotFound(
                text=f"No log file {filename} for run {run_id}"
            ) from e
        else:
            with logfile as f:
                text = f.read().decode("utf-8", "replace")
        return web.Response(
            content_type="text/plain",
            text=text,
        )
    else:
        try:
            f = await request.app["artifact_manager"].get_artifact(run_id, filename)
        except FileNotFoundError as e:
            raise web.HTTPNotFound(
                text=f"No artifact {filename} for run {run_id}"
            ) from e
        return web.Response(body=f.read())


@routes.get("/cupboard/ready", name="cupboard-ready")
@html_template("cupboard/ready-list.html", headers={"Vary": "Cookie"})
async def handle_ready_proposals(request):
    publish_status = request.query.get("publish_status")
    async with request.app["pool"].acquire() as conn:
        query = "SELECT codebase, suite, id, command, result FROM publish_ready"

        conditions = [
            "EXISTS (SELECT * FROM unnest(unpublished_branches) "
            "WHERE mode in "
            "('propose', 'attempt-push', 'push-derived', 'push'))"
        ]
        args = []
        if publish_status:
            args.append(publish_status)
            conditions.append(f"publish_status = ${len(args)}")

        query += " WHERE " + " AND ".join(conditions)

        query += " ORDER BY codebase ASC"

        runs = await conn.fetch(query, *args)
    return {"runs": runs}


@routes.get("/cupboard/done", name="cupboard-done")
@html_template("cupboard/done-list.html", headers={"Vary": "Cookie"})
async def handle_done_proposals(request):
    since_str = request.query.get("since")
    if since_str:
        try:
            since = datetime.fromisoformat(since_str)
        except ValueError as e:
            raise web.HTTPBadRequest(text="invalid since") from e
    else:
        # Default to beginning of the month
        since = datetime.fromisoformat(
            f"{date.today().year:04d}-{date.today().month:02d}-01"
        )

    async with request.app["pool"].acquire() as conn:
        oldest = await conn.fetchval("SELECT MIN(absorbed_at) FROM absorbed_runs")

        if since:
            orig_runs = await conn.fetch(
                "SELECT * FROM absorbed_runs WHERE absorbed_at >= $1 "
                "ORDER BY absorbed_at DESC NULLS LAST",
                since,
            )
        else:
            orig_runs = await conn.fetch(
                "SELECT * FROM absorbed_runs ORDER BY absorbed_at DESC NULLS LAST"
            )

    mp_user_url_resolver = MergeProposalUserUrlResolver()

    runs = []
    for orig_run in orig_runs:
        run = dict(orig_run)
        if not run["merged_by"]:
            run["merged_by_url"] = None
        else:
            run["merged_by_url"] = mp_user_url_resolver.resolve(
                run["merge_proposal_url"], run["merged_by"]
            )
        runs.append(run)

    return {"oldest": oldest, "runs": runs, "since": since}


_extra_cupboard_links: list[tuple[str, str]] = []


def register_cupboard_link(title: str, shortlink: str) -> None:
    _extra_cupboard_links.append((title, shortlink))


@routes.get("/cupboard/evaluate/{run_id}", name="cupboard-default-evaluate")
@html_template("cupboard/default-evaluate.html")
async def handle_cupboard_evaluate(request):
    run_id = request.match_info["run_id"]
    span = aiozipkin.request_span(request)

    from .review import generate_evaluate

    return await generate_evaluate(
        request.app["pool"],
        request.app["vcs_managers"],
        request.app["http_client_session"],
        request.app["differ_url"],
        run_id,
        span,
    )


def register_cupboard_endpoints(
    app,
    *,
    config,
    publisher_url,
    runner_url,
    trace_configs=None,
    db=None,
    evaluate_url=None,
):
    app.router.add_routes(routes)
    from .api import create_app

    app.add_subapp(
        "/cupboard/api",
        create_app(
            config=config,
            publisher_url=publisher_url,
            runner_url=runner_url,
            trace_configs=trace_configs,
            db=db,
        ),
    )
    if evaluate_url is None:
        evaluate_url = app.router["cupboard-default-evaluate"].url_for(run_id="RUN_ID")
    app["evaluate_url"] = evaluate_url
    app["runner_status"] = None


async def iter_needs_review(
    conn: asyncpg.Connection,
    campaigns: Optional[list[str]] = None,
    limit: Optional[int] = None,
    publishable_only: bool = False,
    required_only: Optional[bool] = None,
    reviewer: Optional[str] = None,
):
    args: list[Any] = []
    query = "SELECT id, codebase, suite FROM publish_ready"
    conditions = []
    if campaigns is not None:
        args.append(campaigns)
        conditions.append(f"suite = ANY(${len(args)}::text[])")

    publishable_condition = (
        "exists (select from unnest(unpublished_branches) where "
        "mode in ('propose', 'attempt-push', 'push-derived', 'push'))"
    )

    order_by = []

    order_by.append("(SELECT COUNT(*) FROM review WHERE run_id = id) ASC")

    if publishable_only:
        conditions.append(publishable_condition)
    order_by.append(
        "exists (select from unnest(unpublished_branches) where "
        "mode in ('propose', 'attempt-push', 'push')) DESC"
    )

    if required_only:
        conditions.append("publish_status = 'needs-manual-review'")

    if reviewer is not None:
        args.append(reviewer)
        conditions.append(
            f"not exists (select from review where reviewer = ${len(args)} and run_id = id)"
        )

    if conditions:
        query += " WHERE " + " AND ".join(conditions)

    order_by.extend(["value DESC NULLS LAST", "finish_time DESC"])

    if order_by:
        query += " ORDER BY " + ", ".join(order_by) + " "

    if limit is not None:
        query += f" LIMIT {limit}"
    return await conn.fetch(query, *args)


def create_app(
    *,
    config,
    publisher_url,
    runner_url,
    differ_url,
    evaluate_url=None,
    trace_configs=None,
    db=None,
):
    trailing_slash_redirect = normalize_path_middleware(append_slash=True)
    app = web.Application(middlewares=[trailing_slash_redirect])
    if db is None:
        setup_postgres(app)
    else:
        app["pool"] = db

    async def persistent_session(app):
        app["http_client_session"] = session = ClientSession(
            trace_configs=trace_configs
        )
        yield
        await session.close()

    app.cleanup_ctx.append(persistent_session)

    app["vcs_managers"] = get_vcs_managers_from_config(config)
    app["differ_url"] = differ_url
    app["runner_url"] = runner_url
    app["publisher_url"] = publisher_url

    app["config"] = config

    register_cupboard_endpoints(
        app,
        config=config,
        publisher_url=publisher_url,
        runner_url=runner_url,
        trace_configs=trace_configs,
        db=db,
        evaluate_url=evaluate_url,
    )

    aiohttp_jinja2.setup(
        app,
        loader=template_loader,
        enable_async=True,
        autoescape=select_autoescape(["html", "xml"]),
    )

    return app
