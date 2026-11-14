#!/usr/bin/python
# Copyright (C) 2019-2022 Jelmer Vernooij <jelmer@jelmer.uk>
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

import logging
import secrets
from datetime import timedelta

import aiozipkin
import asyncpg
from aiohttp import ClientConnectorError, ClientResponseError, ClientSession, web
from aiohttp_apispec import (
    docs,
    form_schema,
    querystring_schema,
    response_schema,
    setup_aiohttp_apispec,
)
from aiojobs.aiohttp import setup as setup_aiojobs
from aiojobs.aiohttp import spawn
from marshmallow import Schema, fields
from yarl import URL

from janitor import CAMPAIGN_REGEX, utcnow, version_string

from .. import check_admin, worker_link_is_global
from ..setup import setup_logfile_manager, setup_postgres

BUILD_LOG_FILENAME = "build.log"
DIST_LOG_FILENAME = "dist.log"


routes = web.RouteTableDef()


class MassRescheduleSchema(Schema):
    result_code = fields.Str(
        required=True,
        metadata={"description": "result code to match, or 'never-processed'"},
    )
    include_transient = fields.Str(metadata={"description": "'on' to include"})
    campaign = fields.Str()
    description_re = fields.Str(metadata={"description": "regex for description"})
    min_age = fields.Int(metadata={"description": "minimum age in days"})
    rejected = fields.Raw(metadata={"description": "if present, only rejected runs"})
    offset = fields.Int(metadata={"description": "offset from top of queue"})
    refresh = fields.Raw(metadata={"description": "if present, start from scratch"})


class CodebaseCampaignSchema(Schema):
    codebase = fields.Str()
    campaign = fields.Str()


@docs(summary="Reschedule all runs matching a set of criteria")
@form_schema(MassRescheduleSchema())
@response_schema(CodebaseCampaignSchema(many=True))
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
            params.append(utcnow() - timedelta(days=min_age))
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


class NeedsReviewQuerySchema(Schema):
    reviewer = fields.Str(metadata={"description": "defaults to the current user"})
    campaign = fields.List(
        fields.Str(), metadata={"description": "only these campaigns"}
    )
    publishable_only = fields.Str(load_default="true")
    required_only = fields.Str()
    limit = fields.Int(load_default=200)


class NeedsReviewSchema(Schema):
    id = fields.Str(metadata={"description": "run identifier"})
    codebase = fields.Str()
    campaign = fields.Str()


@docs(summary="List runs that need review")
@querystring_schema(NeedsReviewQuerySchema())
@response_schema(NeedsReviewSchema(many=True))
@routes.get("/needs-review", name="needs-review")
async def handle_needs_review(request):
    from . import iter_needs_review

    campaigns = request.query.getall("campaign", None)
    if "campaign" in request.match_info:
        if campaigns is not None:
            raise web.HTTPBadRequest(
                text="use either a campaign in the path or campaign arguments"
            )
        campaigns = [request.match_info["campaign"]]
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
                campaigns=campaigns,
                required_only=required_only,
                publishable_only=publishable_only,
                reviewer=reviewer,
                limit=limit,
            ):
                ret.append({"codebase": codebase, "id": run_id, "campaign": campaign})
    return web.json_response(ret, status=200)


# TODO(jelmer): Drop this separate handler once
# https://github.com/maximdanilchenko/aiohttp-apispec/issues/139 is fixed;
# a handler shared between routes gets the other route's path parameters.
@docs(summary="List runs of a campaign that need review")
@querystring_schema(NeedsReviewQuerySchema())
@response_schema(NeedsReviewSchema(many=True))
@routes.get(
    "/{campaign:" + CAMPAIGN_REGEX + "}/needs-review", name="needs-review-campaign"
)
async def handle_campaign_needs_review(request):
    return await handle_needs_review(request)


class ReprocessOptionsSchema(Schema):
    dry_run = fields.Raw(metadata={"description": "if present, do not store results"})
    reschedule = fields.Raw(metadata={"description": "if present, reschedule"})


class ReprocessResultSchema(Schema):
    changed = fields.Bool()
    result_code = fields.Str()
    description = fields.Str(allow_none=True)
    failure_details = fields.Raw(allow_none=True)


@docs(summary="Reprocess the logs of a run")
@form_schema(ReprocessOptionsSchema())
@response_schema(ReprocessResultSchema())
@routes.post("/run/{run_id}/reprocess-logs", name="admin-reprocess-logs-run")
async def handle_run_reprocess_logs(request):
    from ...reprocess_logs import (  # type: ignore
        process_dist_log,
        process_sbuild_log,
        reprocess_run_logs,
    )

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


async def select_runs_to_reprocess(
    conn, run_ids=None, campaign=None, description_re=None, min_age=0
):
    if not run_ids:
        args = []
        where = [
            "(result_code = 'build-failed' OR "
            "result_code LIKE 'build-failed-stage-%' OR "
            "result_code LIKE 'autopkgtest-%' OR "
            "result_code LIKE 'build-%' OR "
            "result_code LIKE 'dist-%' OR "
            "result_code LIKE 'unpack-%' OR "
            "result_code LIKE 'create-session-%' OR "
            "result_code LIKE 'missing-%')"
        ]
        if campaign:
            args.append(campaign)
            where.append(f"suite = ${len(args)}")
        if description_re:
            args.append(description_re)
            where.append(f"description ~ ${len(args)}")
        if min_age:
            args.append(utcnow() - timedelta(days=min_age))
            where.append(f"finish_time < ${len(args)}")

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
""" + " AND ".join(where)
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
    return await conn.fetch(query, *args)


class ReprocessLogsSchema(ReprocessOptionsSchema):
    run_id = fields.List(fields.Str())
    campaign = fields.Str()
    description_re = fields.Str(metadata={"description": "regex for description"})
    min_age = fields.Int(metadata={"description": "minimum age in days"})


class ReprocessingSchema(Schema):
    codebase = fields.Str()
    campaign = fields.Str()
    log_id = fields.Str()


@docs(summary="Reprocess the logs of a set of runs")
@form_schema(ReprocessLogsSchema())
@response_schema(ReprocessingSchema(many=True))
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

    async with request.app["pool"].acquire() as conn:
        try:
            rows = await select_runs_to_reprocess(
                conn,
                run_ids,
                campaign=post.get("campaign"),
                description_re=post.get("description_re"),
                min_age=0 if run_ids else int(post.get("min_age", "0")),
            )
        except asyncpg.InvalidRegularExpressionError as e:
            raise web.HTTPBadRequest(text=f"Invalid regex: {e.message}") from e

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


@docs(
    summary="Trigger an autopublish run in the publisher",
    responses={"200": {"description": "publisher response"}},
)
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


@docs(
    summary="Trigger a merge proposal scan in the publisher",
    responses={"200": {"description": "publisher response"}},
)
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


class WorkerSchema(Schema):
    name = fields.Str()
    link = fields.Url(
        allow_none=True, metadata={"description": "only set for global links"}
    )
    run_count = fields.Int()


@docs(summary="List workers")
@response_schema(WorkerSchema(many=True))
@routes.get("/workers", name="admin-workers-list")
async def handle_workers_list(request):
    check_admin(request)
    url = URL(request.app["runner_url"]) / "admin" / "workers"
    async with request.app["http_client_session"].get(
        url, raise_for_status=True
    ) as resp:
        data = await resp.json()
    workers = []
    for entry in sorted(data.get("workers", []), key=lambda e: e["name"]):
        link = entry.get("link")
        workers.append(
            {
                "name": entry["name"],
                "link": link if worker_link_is_global(link) else None,
                "run_count": entry["run_count"],
            }
        )
    return web.json_response(workers)


class WorkerAddSchema(Schema):
    name = fields.Str(required=True)
    link = fields.Url()


class WorkerCredentialsSchema(Schema):
    name = fields.Str()
    link = fields.Url(allow_none=True)
    password = fields.Str(metadata={"description": "generated password"})


@docs(summary="Add a worker")
@form_schema(WorkerAddSchema())
@response_schema(WorkerCredentialsSchema(), 201)
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


class WorkerNameSchema(Schema):
    name = fields.Str()


@docs(summary="Remove a worker")
@response_schema(WorkerNameSchema())
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
    setup_logfile_manager(app)
    setup_aiojobs(app)
    setup_aiohttp_apispec(
        app=app,
        title="Janitor Cupboard API Documentation",
        version=version_string,
        url="/swagger.json",
        swagger_path="/docs",
    )
    return app
