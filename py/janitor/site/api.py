#!/usr/bin/python3
# Copyright (C) 2019-2021 Jelmer Vernooij <jelmer@jelmer.uk>
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

import asyncio
import logging
from typing import Optional

import aiozipkin
import mimeparse
from aiohttp import (
    ClientConnectorError,
    ClientOSError,
    ClientResponseError,
    ClientSession,
    ContentTypeError,
    ServerDisconnectedError,
    web,
)
from aiohttp.web_middlewares import normalize_path_middleware
from aiohttp_apispec import (
    docs,
    form_schema,
    querystring_schema,
    response_schema,
    setup_aiohttp_apispec,
)
from breezy.revision import NULL_REVISION
from marshmallow import Schema, fields, validate
from yarl import URL

from janitor import version_string
from janitor.config import Config

from ..vcs import VcsManager
from . import (
    BuildDiffUnavailable,
    DebdiffRetrievalError,
    RunWithoutArtifacts,
    check_admin,
    check_logged_in,
    get_archive_diff,
    hide_private_worker_links,
    highlight_diff,
    is_qa_reviewer,
    serve_run_log,
)
from .common import render_template_for_request
from .setup import setup_postgres

routes = web.RouteTableDef()


class ReasonSchema(Schema):
    reason = fields.Str()


class PublishRequestSchema(Schema):
    mode = fields.Str(
        validate=validate.OneOf(["push-derived", "push", "propose", "attempt-push"]),
        metadata={"description": "publish mode; overrides the policy (admin only)"},
    )


class PublishResultSchema(Schema):
    run_id = fields.Str()
    mode = fields.Str()
    publish_ids = fields.Dict(
        keys=fields.Str(), values=fields.Str(), metadata={"description": "by role"}
    )
    code = fields.Str()
    description = fields.Str()


@docs(summary="Publish the last run for a codebase")
@form_schema(PublishRequestSchema())
@response_schema(PublishResultSchema(), 202, description="publish started")
@response_schema(PublishResultSchema(), 200, description="nothing to publish")
@routes.post("/{campaign}/c/{codebase}/publish", name="codebase-publish")
async def handle_publish(request):
    check_logged_in(request)
    publisher_url = request.app["publisher_url"]
    codebase = request.match_info["codebase"]
    campaign = request.match_info["campaign"]
    post = await request.post()
    mode = post.get("mode")
    if mode is not None:
        # A chosen mode overrides the publish policy, so it is for admins only.
        check_admin(request)
    if mode not in (None, "push-derived", "push", "propose", "attempt-push"):
        return web.json_response({"error": "Invalid mode", "mode": mode}, status=400)
    url = URL(publisher_url) / campaign / codebase / "publish"
    if request["user"]:
        try:
            requester = request["user"]["email"]
        except KeyError:
            requester = request["user"]["name"]
    else:
        requester = "user from web UI"
    data = {"requester": requester}
    if mode:
        data["mode"] = mode
    try:
        async with request.app["http_client_session"].post(url, data=data) as resp:
            if resp.status in (200, 202):
                return web.json_response(await resp.json(), status=resp.status)
            else:
                return web.json_response(await resp.json(), status=400)
    except ContentTypeError as e:
        return web.json_response(
            {"reason": f"publisher returned error {e}"}, status=400
        )
    except ClientConnectorError:
        return web.json_response({"reason": "unable to contact publisher"}, status=400)


class ScheduleRequestSchema(Schema):
    offset = fields.Int(metadata={"description": "offset from top of queue"})
    refresh = fields.Int(metadata={"description": "1 to start from scratch"})


class ScheduleResultSchema(Schema):
    codebase = fields.Str(metadata={"description": "codebase name"})
    campaign = fields.Str(metadata={"description": "campaign"})
    bucket = fields.Str(metadata={"description": "queue bucket"})
    queue_id = fields.Int(metadata={"description": "queue item identifier"})
    offset = fields.Int(metadata={"description": "offset from top of queue"})
    estimated_duration_seconds = fields.Float(
        metadata={"description": "estimated duration in seconds"}
    )
    queue_position = fields.Int(metadata={"description": "new position in the queue"})
    queue_wait_time = fields.Float(
        metadata={"description": "new delay until run, in seconds"}
    )


@docs(summary="Reschedule a run")
@form_schema(ScheduleRequestSchema())
@response_schema(ScheduleResultSchema())
@routes.post("/run/{run_id}/reschedule", name="run-reschedule")
async def handle_run_reschedule(request):
    check_logged_in(request)
    run_id = request.match_info["run_id"]
    post = await request.post()
    offset = post.get("offset")
    try:
        refresh = bool(int(post.get("refresh", "0")))
    except ValueError:
        return web.json_response({"error": "invalid boolean for refresh"}, status=400)
    if request["user"]:
        try:
            requester = request["user"]["email"]
        except KeyError:
            requester = request["user"]["name"]
    else:
        requester = "user from web UI"
    json = {
        "run_id": run_id,
        "refresh": refresh,
        "requester": requester,
        "bucket": "manual",
        "offset": offset,
    }
    url = URL(request.app["runner_url"]) / "schedule"
    try:
        async with request.app["http_client_session"].post(url, json=json) as resp:
            ret = await resp.json()
            if resp.status >= 400:
                return web.json_response(ret, status=resp.status)
    except ContentTypeError as e:
        return web.json_response(
            {"error": f"runner returned error {e.code}"}, status=400
        )
    except ClientConnectorError:
        return web.json_response({"error": "unable to contact runner"}, status=502)
    return web.json_response(ret)


@docs(summary="Schedule a control run for a run")
@form_schema(ScheduleRequestSchema())
@response_schema(ScheduleResultSchema())
@routes.post("/run/{run_id}/schedule-control", name="run-schedule-control")
async def handle_schedule_control(request):
    check_logged_in(request)
    run_id = request.match_info["run_id"]
    post = await request.post()
    offset = post.get("offset")
    try:
        refresh = bool(int(post.get("refresh", "0")))
    except ValueError:
        return web.json_response({"error": "invalid boolean for refresh"}, status=400)
    if request["user"]:
        try:
            requester = request["user"]["email"]
        except KeyError:
            requester = request["user"]["name"]
    else:
        requester = "user from web UI"

    json = {
        "run_id": run_id,
        "offset": offset,
        "refresh": refresh,
        "requester": requester,
    }

    schedule_url = URL(request.app["runner_url"]) / "schedule-control"
    queue_position_url = URL(request.app["runner_url"]) / "queue" / "position"
    try:
        async with request.app["http_client_session"].post(
            schedule_url, json=json, raise_for_status=True
        ) as resp:
            ret = await resp.json()
        async with request.app["http_client_session"].get(
            queue_position_url,
            params={"campaign": ret["campaign"], "codebase": ret["codebase"]},
            raise_for_status=True,
        ) as resp:
            queue_position = await resp.json()
    except ContentTypeError as e:
        return web.json_response(
            {"error": f"runner returned error {e.code}"}, status=400
        )
    except ClientConnectorError:
        return web.json_response({"error": "unable to contact runner"}, status=502)
    ret["queue_position"] = queue_position["position"]
    ret["queue_wait_time"] = queue_position["wait_time"]
    return web.json_response(ret)


class MergeProposalSchema(Schema):
    url = fields.Url(metadata={"description": "merge proposal URL"})
    status = fields.Str(metadata={"description": "status"})


class MergeProposalChangeSchema(Schema):
    url = fields.Url(required=True, metadata={"description": "merge proposal URL"})
    status = fields.Str(required=True, metadata={"description": "new status"})


@docs(summary="List merge proposals for a campaign")
@response_schema(MergeProposalSchema(many=True))
@routes.get("/{campaign}/merge-proposals", name="campaign-merge-proposals")
async def handle_campaign_merge_proposal_list(request):
    campaign = request.match_info["campaign"]

    url = URL(request.app["publisher_url"]) / campaign / "merge-proposals"
    async with request.app["http_client_session"].get(
        url, raise_for_status=True
    ) as resp:
        return web.json_response(await resp.json())


@docs(summary="List merge proposals for a codebase")
@response_schema(MergeProposalSchema(many=True))
@routes.get("/c/{codebase}/merge-proposals", name="codebase-merge-proposals")
async def handle_codebase_merge_proposal_list(request):
    codebase = request.match_info["codebase"]
    url = URL(request.app["publisher_url"]) / "c" / codebase / "merge-proposals"
    async with request.app["http_client_session"].get(
        url, raise_for_status=True
    ) as resp:
        return web.json_response(await resp.json())


@docs(summary="List merge proposals")
@response_schema(MergeProposalSchema(many=True))
@routes.get("/merge-proposals", name="merge-proposals")
async def handle_merge_proposal_list(request):
    url = URL(request.app["publisher_url"]) / "merge-proposals"
    async with request.app["http_client_session"].get(
        url, raise_for_status=True
    ) as resp:
        return web.json_response(await resp.json())


@docs(
    summary="Change the status of a merge proposal",
    responses={"200": {"description": "status changed"}},
)
@form_schema(MergeProposalChangeSchema())
@routes.post("/merge-proposal", name="merge-proposal")
async def handle_merge_proposal_change(request):
    check_admin(request)
    post = await request.post()

    url = URL(request.app["publisher_url"]) / "merge-proposal"
    async with request.app["http_client_session"].post(
        url, data={"url": post["url"], "status": post["status"]}, raise_for_status=True
    ):
        return web.json_response({})


class RefreshProposalStatusSchema(Schema):
    url = fields.Url(required=True, metadata={"description": "merge proposal URL"})


@docs(
    summary="Refresh the status of a merge proposal",
    produces=["text/plain"],
    responses={"202": {"description": "refresh scheduled"}},
)
@form_schema(RefreshProposalStatusSchema())
@routes.post("/refresh-proposal-status", name="refresh-proposal-status")
async def handle_refresh_proposal_status(request):
    check_logged_in(request)
    post = await request.post()
    try:
        mp_url = post["url"]
    except KeyError as e:
        raise web.HTTPBadRequest(text="No URL specified") from e

    data = {"url": mp_url}
    url = URL(request.app["publisher_url"]) / "refresh-status"
    async with request.app["http_client_session"].post(url, data=data) as resp:
        if resp.status in (200, 202):
            return web.Response(text="Success", status=resp.status)
        return web.Response(text=(await resp.text()), status=resp.status)


class QueueItemSchema(Schema):
    queue_id = fields.Int(metadata={"description": "Queue identifier"})
    codebase = fields.Str(metadata={"description": "Codebase name"})
    campaign = fields.Str(metadata={"description": "Campaign"})
    context = fields.Raw(metadata={"description": "Run context"})  # type: ignore
    command = fields.Str(metadata={"description": "Command"})


class QueueQuerySchema(Schema):
    limit = fields.Int(metadata={"description": "maximum number of items"})


@docs(summary="List the queue")
@querystring_schema(QueueQuerySchema())
@response_schema(QueueItemSchema(many=True))
@routes.get("/queue", name="queue")
async def handle_queue(request):
    limit = request.query.get("limit")
    params = {}
    if limit is not None:
        try:
            params["limit"] = str(int(limit))
        except ValueError as e:
            raise web.HTTPBadRequest(text="limit must be an integer") from e
    url = URL(request.app["runner_url"]) / "queue"
    span = aiozipkin.request_span(request)
    with span.new_child("runner:queue"):
        async with request.app["http_client_session"].get(url, params=params) as resp:
            return web.json_response(await resp.json(), status=resp.status)


@docs(
    summary="List the names of active codebases",
    responses={"200": {"description": "array of codebase names"}},
)
@routes.get("/codebase-names", name="codebase-names")
async def handle_codebase_names(request):
    # TODO(jelmer): Cache the list of active codebase names in the site, rather
    # than retrieving it from the runner on every request.
    url = URL(request.app["runner_url"]) / "codebases" / "names"
    try:
        async with request.app["http_client_session"].get(url) as resp:
            if resp.status != 200:
                logging.warning(
                    "Unable to retrieve codebase names: runner returned %d: %s",
                    resp.status,
                    await resp.text(),
                )
                raise web.HTTPBadGateway(text="Unable to retrieve codebase names")
            names = await resp.json()
    except ClientConnectorError as e:
        logging.warning("Unable to contact runner for codebase names: %s", e)
        raise web.HTTPBadGateway(text="unable to contact runner") from e
    return web.json_response(names, headers={"Cache-Control": "max-age=600"})


class RunDiffQuerySchema(Schema):
    role = fields.Str(load_default="main", metadata={"description": "branch role"})


@docs(
    summary="Retrieve the VCS diff for a run",
    produces=["text/x-diff", "text/plain", "text/html"],
    responses={"200": {"description": "the diff"}},
)
@querystring_schema(RunDiffQuerySchema())
@routes.get("/run/{run_id}/diff", name="run-diff")
async def handle_diff(request):
    return await serve_run_diff(request, request.match_info["run_id"])


async def serve_run_diff(request, run_id):
    role = request.query.get("role", "main")
    span = aiozipkin.request_span(request)
    url = URL(request.app["runner_url"]) / "runs" / run_id
    with span.new_child("runner:get-run"):
        async with request.app["http_client_session"].get(url) as resp:
            if resp.status == 404:
                raise web.HTTPNotFound(text=f"no run {run_id}")
            resp.raise_for_status()
            run = await resp.json()

    branch = next(
        (b for b in run["result_branches"] if b["role"] == role),
        None,
    )
    if branch is None:
        raise web.HTTPNotFound(text=f"no run {run_id}")

    if run["vcs_type"] is None:
        return web.Response(status=404, text="Not in a VCS")

    if branch["revision"] is None:
        return web.Response(text="Branch deleted")
    try:
        try:
            with span.new_child("vcs-diff"):
                diff = await request.app["vcs_managers"][run["vcs_type"]].get_diff(
                    run["codebase"],
                    branch["base_revision"].encode("utf-8")
                    if branch["base_revision"]
                    else NULL_REVISION,
                    branch["revision"].encode("utf-8"),
                )
        except ClientResponseError as e:
            return web.Response(status=e.status, text="Unable to retrieve diff")
        except NotImplementedError as e:
            raise web.HTTPBadRequest(
                text="unsupported vcs {}".format(run["vcs_type"])
            ) from e

        best_match = mimeparse.best_match(
            ["text/x-diff", "text/plain", "text/html"],
            request.headers.get("Accept", "*/*"),
        )
        if best_match in ("text/x-diff", "text/plain"):
            return web.Response(
                body=diff, content_type="text/x-diff", headers={"Vary": "Accept"}
            )
        elif best_match == "text/html":
            return web.Response(
                text=highlight_diff(diff.decode("utf-8", "replace")),
                content_type="text/html",
                headers={"Vary": "Accept"},
            )
        raise web.HTTPNotAcceptable(
            text="Acceptable content types: text/html, text/x-diff"
        )
    except ContentTypeError as e:
        return web.Response(text=f"publisher returned error {e.code}", status=400)
    except ClientConnectorError:
        return web.Response(text="unable to contact publisher", status=502)
    except ClientOSError:
        return web.Response(text="unable to contact publisher - oserror", status=502)


class ArchiveDiffQuerySchema(Schema):
    filter_boring = fields.Raw(
        metadata={"description": "if present, filter out uninteresting changes"}
    )


class ArchiveDiffUnavailableSchema(Schema):
    reason = fields.Str()
    run_id = fields.List(fields.Str())
    unavailable_run_id = fields.Str(allow_none=True)
    campaign = fields.Str()


@docs(
    summary="Retrieve the debdiff or diffoscope output for a run",
    produces=["text/plain", "text/html", "application/json"],
)
@querystring_schema(ArchiveDiffQuerySchema())
@response_schema(ArchiveDiffUnavailableSchema(), 404, description="diff unavailable")
@routes.get("/run/{run_id}/{kind:debdiff|diffoscope}", name="run-archive-diff")
async def handle_archive_diff(request):
    run_id = request.match_info["run_id"]
    kind = request.match_info["kind"]
    span = aiozipkin.request_span(request)
    filter_boring = "filter_boring" in request.query

    try:
        with span.new_child("archive-diff"):
            debdiff, content_type = await get_archive_diff(
                request.app["http_client_session"],
                request.app["differ_url"],
                run_id,
                None,
                kind=kind,
                filter_boring=filter_boring,
                accept=request.headers.get("ACCEPT", "*/*"),
            )
    except BuildDiffUnavailable as e:
        return web.json_response(
            {
                "reason": e.reason,
                "message": e.message,
                "unavailable_run_id": e.unavailable_run_id,
            },
            status=404,
        )
    except RunWithoutArtifacts as e:
        raise web.HTTPNotFound(text=f"Build {run_id} has no artifacts") from e
    except DebdiffRetrievalError as e:
        return web.json_response(
            {
                "reason": f"unable to contact differ for binary diff: {e!r}",
                "inner_reason": e.args[0],
            },
            status=503,
        )

    return web.Response(
        body=debdiff,
        content_type=content_type,
        headers={"Vary": "Accept"},
    )


@docs(
    summary="List the log files of a run",
    responses={"200": {"description": "array of log file names"}},
)
@routes.get("/run/{run_id}/logs", name="run-logs")
async def handle_run_logs(request):
    run_id = request.match_info["run_id"]
    url = URL(request.app["runner_url"]) / "log" / run_id
    try:
        async with request.app["http_client_session"].get(url) as resp:
            if resp.status == 404:
                raise web.HTTPNotFound(text=f"No such run: {run_id}")
            if resp.status != 200:
                logging.warning(
                    "Unable to list logs for run %s: runner returned %d: %s",
                    run_id,
                    resp.status,
                    await resp.text(),
                )
                raise web.HTTPBadGateway(text=f"Unable to list logs for run {run_id}")
            return web.json_response(await resp.json())
    except ClientConnectorError as e:
        logging.warning("Unable to contact runner for logs of %s: %s", run_id, e)
        raise web.HTTPBadGateway(text="unable to contact runner") from e


@docs(
    summary="Retrieve a log file of a run",
    produces=["text/plain"],
    responses={"200": {"description": "the log file"}},
)
@routes.get("/run/{run_id}/logs/{filename}", name="run-log-file")
async def handle_run_log_file(request):
    return await serve_run_log(
        request, request.match_info["run_id"], request.match_info["filename"]
    )


class ReviewSchema(Schema):
    verdict = fields.Str(
        validate=validate.OneOf(["approved", "rejected", "abstained", "reschedule"])
    )
    review_comment = fields.Str(data_key="review-comment")


@docs(summary="Review a run")
@form_schema(ReviewSchema())
@response_schema(ReviewSchema())
@routes.post("/run/{run_id}", name="run-update")
async def handle_run_post(request):
    from ..review import store_review

    async with request.app["pool"].acquire() as conn:
        run_id = request.match_info["run_id"]

        check_logged_in(request)
        span = aiozipkin.request_span(request)
        post = await request.post()
        verdict = post.get("verdict")
        review_comment = post.get("review-comment")
        if verdict:
            verdict = verdict.lower()
            with span.new_child("sql:update-run"):
                try:
                    user = request["user"]["email"]
                except KeyError:
                    user = request["user"]["name"]
                await store_review(
                    conn,
                    request.app["http_client_session"],
                    request.app["runner_url"],
                    run_id,
                    verdict=verdict,
                    comment=review_comment,
                    reviewer=user,
                    is_qa_reviewer=is_qa_reviewer(request),
                )
        return web.json_response({"verdict": verdict, "review-comment": review_comment})


# TODO(jelmer): Drop this separate handler once
# https://github.com/maximdanilchenko/aiohttp-apispec/issues/139 is fixed;
# a handler shared between routes gets the other route's path parameters.
@docs(summary="Review a run")
@form_schema(ReviewSchema())
@response_schema(ReviewSchema())
@routes.post("/c/{codebase}/run/{run_id}", name="codebase-run-update")
async def handle_codebase_run_post(request):
    return await handle_run_post(request)


class ActiveRunSchema(Schema):
    id = fields.Str(metadata={"description": "Run identifier"})
    queue_id = fields.Int()
    codebase = fields.Str()
    campaign = fields.Str()
    change_set = fields.Str(allow_none=True)
    command = fields.Str()
    estimated_duration = fields.Float(
        allow_none=True, metadata={"description": "in seconds"}
    )
    current_duration = fields.Float(metadata={"description": "in seconds"})
    start_time = fields.DateTime()
    worker = fields.Str()
    worker_link = fields.Url(
        allow_none=True, metadata={"description": "only set for global links"}
    )
    vcs = fields.Dict()
    backchannel = fields.Dict()
    instigated_context = fields.Raw(allow_none=True)
    resume_from = fields.Str(allow_none=True)


class RunnerStatusSchema(Schema):
    processing = fields.List(fields.Nested(ActiveRunSchema))
    avoid_hosts = fields.List(fields.Str())
    rate_limit_hosts = fields.Dict(
        keys=fields.Str(),
        values=fields.DateTime(),
        metadata={"description": "rate limited hosts and when the limit expires"},
    )


@docs(summary="Retrieve the runner status")
@response_schema(RunnerStatusSchema())
@routes.get("/runner/status", name="runner-status")
async def handle_runner_status(request):
    url = URL(request.app["runner_url"]) / "status"
    span = aiozipkin.request_span(request)
    with span.new_child("runner:status"):
        try:
            async with request.app["http_client_session"].get(url) as resp:
                status = await resp.json()
                if resp.status == 200:
                    status["processing"] = [
                        hide_private_worker_links(e) for e in status["processing"]
                    ]
                return web.json_response(status, status=resp.status)
        except ContentTypeError as e:
            return web.json_response(
                {"reason": f"runner returned error {e}"}, status=400
            )
        except ClientConnectorError as e:
            return web.json_response(
                {"reason": "unable to contact runner", "details": repr(e)}, status=502
            )


@docs(
    summary="List the log files of an active run",
    produces=["application/json", "text/plain", "text/html"],
    responses={"200": {"description": "array of log file names"}},
)
@routes.get("/active-runs/{run_id}/log/", name="run-log-list")
async def handle_runner_log_index(request):
    run_id = request.match_info["run_id"]
    url = URL(request.app["runner_url"]) / "log" / run_id
    span = aiozipkin.request_span(request)
    with span.new_child("runner:log-list"):
        try:
            async with request.app["http_client_session"].get(url) as resp:
                ret = await resp.json()
        except ContentTypeError as e:
            return web.json_response(
                {"reason": f"runner returned error {e}"}, status=400
            )
        except ClientConnectorError as e:
            return web.json_response(
                {"reason": "unable to contact runner", "details": repr(e)}, status=502
            )
        except asyncio.TimeoutError:
            return web.json_response(
                {"reason": "timeout contacting runner"}, status=502
            )

    best_match = mimeparse.best_match(
        ["text/html", "application/json", "text/plain"],
        request.headers.get("Accept", "*/*"),
    )
    if best_match == "application/json":
        return web.json_response(ret)
    elif best_match == "text/plain":
        return web.Response(
            text="".join([line + "\n" for line in ret]), content_type="text/plain"
        )
    elif best_match == "text/html":
        text = await render_template_for_request(
            "log-index.html", request, {"contents": ret}
        )
        return web.Response(text=text, content_type="text/html")

    raise web.HTTPNotAcceptable()


@docs(summary="Kill an active run")
@response_schema(ActiveRunSchema(), 200, description="the killed run")
@response_schema(ReasonSchema(), 410, description="worker has no active run")
@response_schema(ReasonSchema(), 501, description="kill not supported")
@routes.post("/active-runs/{run_id}/kill", name="run-kill")
async def handle_runner_kill(request):
    span = aiozipkin.request_span(request)
    with span.new_child("check-admin"):
        check_admin(request)
    run_id = request.match_info["run_id"]
    with span.new_child("runner:kill"):
        url = URL(request.app["runner_url"]) / "kill" / run_id
        try:
            async with request.app["http_client_session"].post(url) as resp:
                if resp.status == 501:
                    return web.json_response(
                        {
                            "reason": await resp.text()
                            or "kill not supported for this type of run"
                        },
                        status=501,
                    )
                if resp.status == 410:
                    return web.json_response(
                        {"reason": await resp.text() or "worker has no active run"},
                        status=410,
                    )
                resp.raise_for_status()
                return web.json_response(await resp.json(), status=resp.status)
        except ContentTypeError as e:
            return web.json_response(
                {"reason": f"runner returned error {e}"}, status=400
            )
        except ClientConnectorError:
            return web.json_response({"reason": "unable to contact runner"}, status=502)
        except asyncio.TimeoutError:
            return web.Response(text="timeout contacting runner", status=502)
        except ClientResponseError as e:
            return web.json_response({"reason": str(e)}, status=502)


@docs(
    summary="Retrieve a log file of an active run",
    produces=["text/plain"],
    responses={"200": {"description": "the log file"}},
)
@routes.get("/active-runs/{run_id}/log/{filename}", name="run-log")
async def handle_runner_log(request):
    run_id = request.match_info["run_id"]
    filename = request.match_info["filename"]
    span = aiozipkin.request_span(request)
    with span.new_child("runner:log"):
        url = URL(request.app["runner_url"]) / "log" / run_id / filename
        try:
            async with request.app["http_client_session"].get(url) as resp:
                body = await resp.read()
                return web.Response(
                    body=body, status=resp.status, content_type="text/plain"
                )
        except ContentTypeError as e:
            return web.Response(text=f"runner returned error {e}", status=400)
        except ClientConnectorError:
            return web.Response(text="unable to contact runner", status=502)
        except asyncio.TimeoutError:
            return web.Response(text="timeout contacting runner", status=502)


class PublishDetailsSchema(Schema):
    codebase = fields.Str(allow_none=True)
    target_branch_url = fields.Str(allow_none=True)
    branch_name = fields.Str(allow_none=True)
    main_branch_revision = fields.Str(allow_none=True)
    revision = fields.Str(allow_none=True)
    mode = fields.Str()
    merge_proposal_url = fields.Str(allow_none=True)
    result_code = fields.Str()
    description = fields.Str(allow_none=True)


@docs(summary="Retrieve details of a publish attempt")
@response_schema(PublishDetailsSchema())
@routes.get("/publish/{publish_id}", name="publish-details")
async def handle_publish_id(request):
    publish_id = request.match_info["publish_id"]
    span = aiozipkin.request_span(request)
    with span.new_child("publisher:publish"):
        url = URL(request.app["publisher_url"]) / "publish" / publish_id
        try:
            async with request.app["http_client_session"].get(url) as resp:
                return web.json_response(await resp.json())
        except ContentTypeError as e:
            return web.Response(text=f"runner returned error {e}", status=400)
        except ClientConnectorError:
            return web.Response(text="unable to contact runner", status=502)
        except asyncio.TimeoutError:
            return web.Response(text="timeout contacting runner", status=502)


class QueueAssignmentItemSchema(Schema):
    id = fields.Int()
    context = fields.Raw(allow_none=True)  # type: ignore
    command = fields.Str()
    estimated_duration = fields.Float(allow_none=True)
    campaign = fields.Str()
    refresh = fields.Bool()
    requester = fields.Str(allow_none=True)
    change_set = fields.Str(allow_none=True)
    codebase = fields.Str()


class PeekSchema(Schema):
    queue_item = fields.Nested(QueueAssignmentItemSchema)
    vcs_info = fields.Dict()
    build_config = fields.Dict(keys=fields.Str(), values=fields.Str())
    estimated_duration = fields.Int(
        allow_none=True, metadata={"description": "in seconds"}
    )


@docs(summary="Show the item that would be assigned next")
@response_schema(PeekSchema(), 201)
@routes.get("/active-runs/+peek")
async def handle_run_peek(request):
    span = aiozipkin.request_span(request)
    url = URL(request.app["runner_url"]) / "active-runs/+peek"
    with span.new_child("forward-runner"):
        try:
            async with request.app["http_client_session"].get(url) as resp:
                if resp.status != 201:
                    try:
                        internal_error = await resp.json()
                    except ContentTypeError:
                        internal_error = await resp.text()
                    ret = {
                        "internal-status": resp.status,
                        "internal-result": internal_error,
                    }
                    if "reason" in internal_error:
                        ret["reason"] = internal_error["reason"]
                    return web.json_response(ret, status=400)
                assignment = await resp.json()
                return web.json_response(assignment, status=201)
        except (ClientConnectorError, ServerDisconnectedError) as e:
            return web.json_response(
                {"reason": f"unable to contact runner: {e}"}, status=502
            )
        except asyncio.TimeoutError as e:
            return web.json_response(
                {"reason": f"timeout contacting runner: {e}"}, status=502
            )


@docs(summary="List active runs")
@response_schema(ActiveRunSchema(many=True))
@routes.get("/active-runs", name="active-runs-list")
async def handle_list_active_runs(request):
    span = aiozipkin.request_span(request)
    with span.new_child("runner:active-runs-list"):
        url = URL(request.app["runner_url"]) / "status"
        async with request.app["http_client_session"].get(url) as resp:
            if resp.status != 200:
                return web.json_response(await resp.json(), status=resp.status)
            status = await resp.json()
            return web.json_response(
                [hide_private_worker_links(e) for e in status["processing"]],
                status=200,
            )


@docs(summary="Retrieve an active run")
@response_schema(ActiveRunSchema())
@routes.get("/active-runs/{run_id}", name="active-run-get")
async def handle_get_active_run(request):
    run_id = request.match_info["run_id"]
    span = aiozipkin.request_span(request)
    with span.new_child("runner:get-active-run"):
        url = URL(request.app["runner_url"]) / "status"
        async with request.app["http_client_session"].get(url) as resp:
            if resp.status != 200:
                return web.json_response(await resp.json(), status=resp.status)
            processing = (await resp.json())["processing"]
            for entry in processing:
                if entry["id"] == run_id:
                    return web.json_response(
                        hide_private_worker_links(entry), status=200
                    )
            return web.json_response({}, status=404)


def create_app(
    *,
    publisher_url: Optional[str],
    runner_url: str,
    vcs_managers: Optional[dict[str, VcsManager]],
    differ_url: Optional[str],
    config: Config,
    external_url: Optional[URL] = None,
    trace_configs=None,
    db=None,
) -> web.Application:
    trailing_slash_redirect = normalize_path_middleware(append_slash=True)
    app = web.Application(middlewares=[trailing_slash_redirect])
    app.router.add_routes(routes)

    async def persistent_session(app):
        app["http_client_session"] = session = ClientSession(
            trace_configs=trace_configs
        )
        yield
        await session.close()

    app.cleanup_ctx.append(persistent_session)
    app["config"] = config

    app["external_url"] = external_url
    app["publisher_url"] = publisher_url
    app["vcs_managers"] = vcs_managers
    app["runner_url"] = runner_url
    app["differ_url"] = differ_url

    if db is None:
        setup_postgres(app)
    else:
        app["pool"] = db

    # app.middlewares.append(apispec_validation_middleware)
    setup_aiohttp_apispec(
        app=app,
        title="Janitor API Documentation",
        version=version_string,
        url="/swagger.json",
        swagger_path="/docs",
    )
    return app
