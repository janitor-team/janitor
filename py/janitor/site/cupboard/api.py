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

import logging
import secrets
from datetime import datetime, timedelta

import aiozipkin
import asyncpg
from aiohttp import ClientConnectorError, ClientResponseError, ClientSession, web
from aiohttp_apispec import docs
from aiojobs.aiohttp import setup as setup_aiojobs
from aiojobs.aiohttp import spawn
from yarl import URL

from janitor import CAMPAIGN_REGEX

from .. import check_admin, worker_link_is_global
from ..setup import setup_logfile_manager, setup_postgres

BUILD_LOG_FILENAME = "build.log"
DIST_LOG_FILENAME = "dist.log"


routes = web.RouteTableDef()


@docs()
@routes.post("/mass-reschedule", name="admin-reschedule")
async def handle_mass_reschedule(request):
    check_admin(request)
    post = await request.post()
    include_transient = post.get("include_transient", "off") == "on"
    try:
        result_code = post["result_code"]
    except KeyError as e:
        raise web.HTTPBadRequest(text="result_code not specified") from e
    campaign = post.get("campaign")
    description_re = post.get("description_re")
    min_age = int(post.get("min_age", "0"))
    rejected = "rejected" in post
    offset = int(post.get("offset", "0"))
    refresh = "refresh" in post
    config = request.app["config"]
    all_campaigns = [c.name for c in config.campaign]
    if result_code == "never-processed":
        query = (
            "select c.codebase AS codebase, c.suite AS campaign from candidate c WHERE "
        )
        params = []
        where = [
            "not exists (SELECT FROM run WHERE run.codebase = c.codebase AND c.suite = suite)"
        ]
        if campaign:
            params.append(campaign)
            where.append(f"c.suite = ${len(params)}")
        else:
            params.append(all_campaigns)
            where.append(f"c.suite = ANY(${len(params)}::text[])")
    else:
        if include_transient:
            table = "last_runs"
        else:
            table = "last_effective_runs"
        query = f"""
SELECT
codebase,
suite AS campaign,
finish_time - start_time as duration
FROM {table} AS run
WHERE
EXISTS (SELECT FROM candidate WHERE
run.codebase = candidate.codebase AND
run.suite = candidate.suite AND
(run.change_set = candidate.change_set OR candidate.change_set IS NULL))
AND """
        where = []
        params = []
        if result_code is not None:
            params.append(result_code)
            where.append(f"result_code = ${len(params)}")
        if campaign:
            params.append(campaign)
            where.append(f"suite = ${len(params)}")
        else:
            params.append(all_campaigns)
            where.append(f"suite = ANY(${len(params)}::text[])")
        if rejected:
            where.append("publish_status = 'rejected'")
        if description_re:
            params.append(description_re)
            where.append(f"description ~ ${len(params)}")
        if min_age:
            params.append(datetime.utcnow() - timedelta(days=min_age))
            where.append(f"finish_time < ${len(params)}")
    query += " AND ".join(where)

    async with request.app["pool"].acquire() as conn:
        try:
            runs = await conn.fetch(query, *params)
        except asyncpg.InvalidRegularExpressionError as e:
            raise web.HTTPBadRequest(text=f"Invalid regex: {e.message}") from e

    session = request.app["http_client_session"]

    async def do_reschedule():
        schedule_url = URL(request.app["runner_url"]) / "schedule"
        for run in runs:
            logging.info("Rescheduling %s, %s", run["codebase"], run["campaign"])
            try:
                async with session.post(
                    schedule_url,
                    json={
                        "codebase": run["codebase"],
                        "campaign": run["campaign"],
                        "requester": "reschedule",
                        "refresh": refresh,
                        "offset": offset,
                        "bucket": "reschedule",
                        "estimated_duration": (
                            run["duration"].total_seconds()
                            if run.get("duration")
                            else None
                        ),
                    },
                    raise_for_status=True,
                ):
                    pass
            except ClientResponseError as e:
                if e.status == 400:
                    logging.debug(
                        "Not rescheduling %s/%s: candidate unavailable",
                        run["codebase"],
                        run["campaign"],
                    )
                else:
                    logging.exception(
                        "Unable to reschedule %s/%s: %d: %s",
                        run["codebase"],
                        run["campaign"],
                        e.status,
                        e.message,
                    )

    await spawn(request, do_reschedule())
    return web.json_response(
        [{"codebase": run["codebase"], "campaign": run["campaign"]} for run in runs]
    )


@docs()
@routes.get("/needs-review", name="needs-review")
@routes.get(
    "/{campaign:" + CAMPAIGN_REGEX + "}/needs-review", name="needs-review-campaign"
)
async def handle_needs_review(request):
    from . import iter_needs_review

    requested_campaign = request.match_info.get("campaign")
    reviewer = request.query.get("reviewer")
    if reviewer is None and request.get("user"):
        reviewer = request["user"].get("email")
    span = aiozipkin.request_span(request)
    publishable_only = request.query.get("publishable_only", "true") == "true"
    if "required_only" in request.query:
        required_only = request.query["required_only"] == "true"
    else:
        required_only = None
    limit = request.query.get("limit", "200")
    if limit:
        limit = int(limit)
    else:
        limit = None
    ret = []
    async with request.app["pool"].acquire() as conn:
        with span.new_child("sql:needs-review"):
            for run_id, codebase, campaign in await iter_needs_review(
                conn,
                campaigns=([requested_campaign] if requested_campaign else None),
                required_only=required_only,
                publishable_only=publishable_only,
                reviewer=reviewer,
                limit=limit,
            ):
                ret.append({"codebase": codebase, "id": run_id, "campaign": campaign})
    return web.json_response(ret, status=200)


@docs()
@routes.post("/run/{run_id}/reprocess-logs", name="admin-reprocess-logs-run")
async def handle_run_reprocess_logs(request):
    from ...reprocess_logs import (  # type: ignore
        process_dist_log,
        process_sbuild_log,
        reprocess_run_logs,
    )

    check_admin(request)
    post = await request.post()
    run_id = request.match_info["run_id"]
    dry_run = "dry_run" in post
    reschedule = "reschedule" in post
    async with request.app["pool"].acquire() as conn:
        run = await conn.fetchrow(
            "SELECT codebase, suite AS campaign, command, "
            "finish_time - start_time as duration, codebase, "
            "result_code, description, failure_details, change_set FROM run WHERE id = $1",
            run_id,
        )

    result = await reprocess_run_logs(
        db=request.app["pool"],
        logfile_manager=request.app["logfile_manager"],
        codebase=run["codebase"],
        campaign=run["campaign"],
        log_id=run_id,
        command=run["command"],
        change_set=run["change_set"],
        duration=run["duration"],
        result_code=run["result_code"],
        description=run["description"],
        failure_details=run["failure_details"],
        process_fns=[
            ("dist-", DIST_LOG_FILENAME, process_dist_log),
            ("build-", BUILD_LOG_FILENAME, process_sbuild_log),
        ],
        dry_run=dry_run,
        reschedule=reschedule,
    )

    if result:
        (new_code, new_description, new_failure_details) = result
        return web.json_response(
            {
                "changed": True,
                "result_code": new_code,
                "description": new_description,
                "failure_details": new_failure_details,
            }
        )
    else:
        return web.json_response(
            {
                "changed": False,
                "result_code": run["result_code"],
                "description": run["description"],
                "failure_details": run["failure_details"],
            }
        )


@docs()
@routes.post("/reprocess-logs", name="admin-reprocess-logs")
async def handle_reprocess_logs(request):
    from ...reprocess_logs import (
        process_dist_log,
        process_sbuild_log,
        reprocess_run_logs,
    )

    check_admin(request)
    post = await request.post()
    dry_run = "dry_run" in post
    reschedule = "reschedule" in post
    try:
        run_ids = post.getall("run_id")
    except KeyError:
        run_ids = None

    if not run_ids:
        args = []
        query = """
SELECT
  codebase,
  suite AS campaign,
  id,
  command,
  finish_time - start_time as duration,
  result_code,
  description,
  failure_details,
  change_set
FROM run
WHERE
  (result_code = 'build-failed' OR
   result_code LIKE 'build-failed-stage-%' OR
   result_code LIKE 'autopkgtest-%' OR
   result_code LIKE 'build-%' OR
   result_code LIKE 'dist-%' OR
   result_code LIKE 'unpack-%s' OR
   result_code LIKE 'create-session-%' OR
   result_code LIKE 'missing-%')
"""
    else:
        args = [run_ids]
        query = """
SELECT
  codebase,
  suite AS campaign,
  id,
  command,
  finish_time - start_time as duration,
  result_code,
  description,
  failure_details,
  change_set,
  codebase
FROM run
WHERE
  id = ANY($1::text[])
"""
    async with request.app["pool"].acquire() as conn:
        rows = await conn.fetch(query, *args)

    for row in rows:
        await spawn(
            request,
            reprocess_run_logs(
                db=request.app["pool"],
                logfile_manager=request.app["logfile_manager"],
                codebase=row["codebase"],
                campaign=row["campaign"],
                log_id=row["id"],
                command=row["command"],
                change_set=row["change_set"],
                duration=row["duration"],
                result_code=row["result_code"],
                description=row["description"],
                failure_details=row["failure_details"],
                process_fns=[
                    ("dist-", DIST_LOG_FILENAME, process_dist_log),
                    ("build-", BUILD_LOG_FILENAME, process_sbuild_log),
                ],
                dry_run=dry_run,
                reschedule=reschedule,
            ),
        )

    return web.json_response(
        [
            {
                "codebase": row["codebase"],
                "campaign": row["campaign"],
                "log_id": row["id"],
            }
            for row in rows
        ]
    )


@docs()
@routes.post("/publish/autopublish", name="publish-autopublish")
async def handle_publish_autopublish(request):
    check_admin(request)
    publisher_url = request.app["publisher_url"]
    url = URL(publisher_url) / "autopublish"
    try:
        async with request.app["http_client_session"].post(url) as resp:
            return web.Response(body=await resp.read(), status=resp.status)
    except ClientConnectorError:
        return web.Response(text="unable to contact publisher", status=400)


@docs()
@routes.post("/publish/scan", name="publish-scan")
async def handle_publish_scan(request):
    check_admin(request)
    publisher_url = request.app["publisher_url"]
    url = URL(publisher_url) / "scan"
    try:
        async with request.app["http_client_session"].post(url) as resp:
            return web.Response(body=await resp.read(), status=resp.status)
    except ClientConnectorError:
        return web.Response(text="unable to contact publisher", status=400)


@docs()
@routes.get("/workers", name="admin-workers-list")
async def handle_workers_list(request):
    check_admin(request)
    async with request.app["pool"].acquire() as conn:
        rows = await conn.fetch(
            "SELECT worker.name, worker.link, "
            "count(run.id) AS run_count "
            "FROM worker "
            "LEFT JOIN run ON run.worker = worker.name "
            "GROUP BY worker.name, worker.link "
            "ORDER BY worker.name"
        )
    workers = []
    for row in rows:
        link = row["link"] if worker_link_is_global(row["link"]) else None
        workers.append(
            {"name": row["name"], "link": link, "run_count": row["run_count"]}
        )
    return web.json_response(workers)


@docs()
@routes.post("/workers", name="admin-workers-add")
async def handle_workers_add(request):
    check_admin(request)
    post = await request.post()
    try:
        name = post["name"]
    except KeyError as e:
        raise web.HTTPBadRequest(text="name not specified") from e
    link = post.get("link") or None
    password = secrets.token_urlsafe(24)
    async with request.app["pool"].acquire() as conn:
        try:
            await conn.execute(
                "INSERT INTO worker (name, password, link) "
                "VALUES ($1, crypt($2, gen_salt('bf')), $3)",
                name,
                password,
                link,
            )
        except asyncpg.UniqueViolationError as e:
            raise web.HTTPConflict(text=f"worker {name!r} already exists") from e
    return web.json_response(
        {"name": name, "link": link, "password": password}, status=201
    )


@docs()
@routes.delete("/workers/{name}", name="admin-workers-delete")
async def handle_workers_delete(request):
    check_admin(request)
    name = request.match_info["name"]
    async with request.app["pool"].acquire() as conn:
        result = await conn.execute("DELETE FROM worker WHERE name = $1", name)
    # asyncpg returns e.g. "DELETE 1" or "DELETE 0"
    if result.endswith(" 0"):
        raise web.HTTPNotFound(text=f"no such worker {name!r}")
    return web.json_response({"name": name})


def create_app(*, config, publisher_url, runner_url, trace_configs=None, db=None):
    app = web.Application()
    app["config"] = config
    app.router.add_routes(routes)

    async def persistent_session(app):
        app["http_client_session"] = session = ClientSession(
            trace_configs=trace_configs
        )
        yield
        await session.close()

    app.cleanup_ctx.append(persistent_session)

    app["publisher_url"] = publisher_url
    app["runner_url"] = runner_url
    if db is None:
        setup_postgres(app)
    else:
        app["pool"] = db
    setup_logfile_manager(app, trace_configs=trace_configs)
    setup_aiojobs(app)
    return app
