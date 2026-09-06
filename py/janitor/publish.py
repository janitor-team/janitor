#!/usr/bin/python3
# Copyright (C) 2019 Jelmer Vernooij <jelmer@jelmer.uk>
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

"""Publishing VCS changes."""

__all__ = [
    "calculate_next_try_time",
]

import asyncio
import json
import logging
import os
import sys
import time
import uuid
import warnings
from collections.abc import AsyncIterable, Iterator
from contextlib import AsyncExitStack
from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Any, Optional, cast

import aioredlock
import aiozipkin
import asyncpg
import asyncpg.pool
import breezy.plugin
import gpg
from aiohttp import ClientSession, web
from aiohttp.web_middlewares import normalize_path_middleware
from aiohttp_apispec import setup_aiohttp_apispec
from aiohttp_openmetrics import (
    REGISTRY,
    Counter,
    Gauge,
    Histogram,
    push_to_gateway,
    setup_metrics,
)
from aiojobs.aiohttp import setup as setup_aiojobs
from aiojobs.aiohttp import spawn
from breezy import urlutils
from breezy.errors import PermissionDenied, RedirectRequested, UnexpectedHttpStatus
from breezy.forge import (
    Forge,
    ForgeLoginRequired,
    MergeProposal,
    UnsupportedForge,
    forges,
    get_proposal_by_url,
    iter_forge_instances,
)
from breezy.transport import Transport
from redis.asyncio import Redis
from silver_platter import (
    BranchMissing,
    BranchRateLimited,
    BranchUnavailable,
)
from silver_platter import (
    _open_branch as open_branch,
)

from . import set_user_agent, state
from ._launchpad import override_launchpad_consumer_name
from ._publish import (
    BucketRateLimited,
    FixedRateLimiter,
    NonRateLimiter,
    RateLimited,
    RateLimiter,
    SlowStartRateLimiter,
    branches_match,
    calculate_next_try_time,
    get_merged_by_user_url,
    role_branch_url,
)
from .config import Campaign, Config, get_campaign_config, read_config
from .schedule import CandidateUnavailable, do_schedule, do_schedule_control
from .vcs import VcsManager, get_vcs_managers_from_config

override_launchpad_consumer_name()
breezy.plugin.load_plugins()


EXISTING_RUN_RETRY_INTERVAL = 30

MODE_SKIP = "skip"
MODE_BUILD_ONLY = "build-only"
MODE_PUSH = "push"
MODE_PUSH_DERIVED = "push-derived"
MODE_PROPOSE = "propose"
MODE_ATTEMPT_PUSH = "attempt-push"
MODE_BTS = "bts"
SUPPORTED_MODES = [
    MODE_PUSH,
    MODE_SKIP,
    MODE_BUILD_ONLY,
    MODE_PUSH_DERIVED,
    MODE_PROPOSE,
    MODE_ATTEMPT_PUSH,
    MODE_BTS,
]


proposal_rate_limited_count = Counter(
    "proposal_rate_limited",
    "Number of attempts to create a proposal that was rate-limited",
    ["codebase", "campaign"],
)
open_proposal_count = Gauge("open_proposal_count", "Number of open proposals.")
bucket_proposal_count = Gauge(
    "bucket_proposal_count", "Number of proposals per bucket.", labelnames=("bucket",)
)
merge_proposal_count = Gauge(
    "merge_proposal_count",
    "Number of merge proposals by status.",
    labelnames=("status",),
)
new_merge_proposal_count = Counter(
    "new_merge_proposal_count", "Number of new merge proposals opened."
)
last_publish_pending_success = Gauge(
    "last_publish_pending_success",
    "Last time pending changes were successfully published",
)
last_scan_existing_success = Gauge(
    "last_scan_existing_success",
    "Last time existing merge proposals were successfully scanned",
)
publish_latency = Histogram(
    "publish_latency", "Delay between build finish and publish."
)

exponential_backoff_count = Counter(
    "exponential_backoff_count",
    "Number of times publishing has been skipped due to exponential backoff",
)

push_limit_count = Counter(
    "push_limit_count", "Number of times pushes haven't happened due to the limit"
)

missing_branch_url_count = Counter(
    "missing_branch_url_count",
    "Number of runs that weren't published because they had a missing branch URL",
)

rejected_last_mp_count = Counter("rejected_last_mp", "Last merge proposal was rejected")

missing_publish_mode_count = Counter(
    "missing_publish_mode_count",
    "Number of runs not published due to missing publish mode",
    labelnames=("role",),
)

unpublished_aux_branches_count = Counter(
    "unpublished_aux_branches_count",
    "Number of branches not published because auxiliary branches "
    "were not yet published",
    labelnames=("role",),
)

command_changed_count = Counter(
    "command_changed_count",
    "Number of runs not published because the codemod command changed",
)


no_result_branches_count = Counter(
    "no_result_branches_count", "Runs not published since there were no result branches"
)


missing_main_result_branch_count = Counter(
    "missing_main_result_branch_count",
    "Runs not published because of missing main result branch",
)

forge_rate_limited_count = Counter(
    "forge_rate_limited_count",
    "Runs were not published because the relevant forge was rate-limiting",
    labelnames=("forge",),
)

unexpected_http_response_count = Counter(
    "unexpected_http_response_count",
    "Number of unexpected HTTP responses during checks of existing proposals",
)


CLOSED_STATUSES = ["closed", "abandoned", "rejected", "applied"]


logger = logging.getLogger("janitor.publish")


routes = web.RouteTableDef()


class PublishFailure(Exception):
    def __init__(self, mode: str, code: str, description: str) -> None:
        self.mode = mode
        self.code = code
        self.description = description


async def derived_branch_name(conn, campaign_config, run, role):
    if len(run.result_branches) == 1:
        name = campaign_config.branch_name
    else:
        name = f"{campaign_config.branch_name}/{role}"

    if await state.has_cotenants(conn, run.codebase, run.branch_url):
        return name + "/" + run.codebase
    else:
        return name


@dataclass
class PublishResult:
    description: str
    target_branch_url: Optional[str] = None
    is_new: bool = False
    proposal_url: Optional[str] = None
    proposal_web_url: Optional[str] = None
    target_branch_web_url: Optional[str] = None
    branch_name: Optional[str] = None


class BranchBusy(Exception):
    """The branch is already busy."""

    def __init__(self, branch_url) -> None:
        self.branch_url = branch_url


class WorkerInvalidResponse(Exception):
    """Invalid response from worker."""

    def __init__(self, output) -> None:
        self.output = output


async def run_worker_process(args, request, *, encoding="utf-8"):
    p = await asyncio.create_subprocess_exec(
        *args,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        stdin=asyncio.subprocess.PIPE,
    )

    (stdout, stderr) = await p.communicate(json.dumps(request).encode(encoding))

    if p.returncode == 1:
        try:
            response = json.loads(stdout.decode(encoding))
        except json.JSONDecodeError as e:
            raise WorkerInvalidResponse(stderr.decode(encoding)) from e
        sys.stderr.write(stderr.decode(encoding))
        return 1, response

    if p.returncode == 0:
        return p.returncode, json.loads(stdout.decode(encoding))

    raise WorkerInvalidResponse(stderr.decode(encoding))


class PublishWorker:
    def __init__(
        self,
        *,
        lock_manager=None,
        redis=None,
        template_env_path: Optional[str] = None,
        external_url: Optional[str] = None,
        differ_url: Optional[str] = None,
    ) -> None:
        self.template_env_path = template_env_path
        self.external_url = external_url
        self.differ_url = differ_url
        self.lock_manager = lock_manager
        self.redis = redis

    async def publish_one(
        self,
        *,
        campaign: str,
        codebase: str,
        command,
        target_branch_url: str,
        mode: str,
        role: str,
        revision: bytes,
        log_id: str,
        unchanged_id: str | None,
        derived_branch_name: str,
        rate_limit_bucket: Optional[str],
        vcs_manager: VcsManager,
        bucket_rate_limiter: Optional[RateLimiter] = None,
        require_binary_diff: bool = False,
        allow_create_proposal: bool = False,
        reviewers: Optional[list[str]] = None,
        result_tags: Optional[list[tuple[str, bytes]]] = None,
        commit_message_template: Optional[str] = None,
        title_template: Optional[str] = None,
        codemod_result=None,
        existing_mp_url: Optional[str] = None,
        extra_context: Optional[dict[str, Any]] = None,
    ) -> PublishResult:
        """Publish a single run in some form.

        Args:
          campaign: The campaign name
          command: Command that was run
        """
        assert mode in SUPPORTED_MODES, f"mode is {mode!r}"
        local_branch_url = vcs_manager.get_branch_url(codebase, f"{campaign}/{role}")
        target_branch_url = target_branch_url.rstrip("/")

        request = {
            "campaign": campaign,
            "command": command,
            "codemod_result": codemod_result,
            "target_branch_url": target_branch_url,
            "source_branch_url": local_branch_url,
            "existing_mp_url": existing_mp_url,
            "derived_branch_name": derived_branch_name,
            "mode": mode,
            "role": role,
            "log_id": log_id,
            "unchanged_id": unchanged_id,
            "require-binary-diff": require_binary_diff,
            "allow_create_proposal": allow_create_proposal,
            "external_url": self.external_url,
            "differ_url": self.differ_url,
            "revision": revision.decode("utf-8"),
            "reviewers": reviewers,
            "commit_message_template": commit_message_template,
            "title_template": title_template,
            "extra_context": extra_context,
        }

        if result_tags:
            request["tags"] = {n: r.decode("utf-8") for (n, r) in result_tags}
        else:
            request["tags"] = {}

        args = [sys.executable, "-m", "janitor.publish_one"]

        if self.template_env_path:
            args.append(f"--template-env-path={self.template_env_path}")

        try:
            async with AsyncExitStack() as es:
                if self.lock_manager:
                    await es.enter_async_context(
                        await self.lock_manager.lock(f"publish:{target_branch_url}")
                    )
                try:
                    returncode, response = await run_worker_process(args, request)
                except WorkerInvalidResponse as e:
                    raise PublishFailure(
                        mode, "publisher-invalid-response", e.output
                    ) from e
        except aioredlock.LockError as e:
            raise BranchBusy(target_branch_url) from e

        if returncode == 1:
            raise PublishFailure(mode, response["code"], response["description"])

        if returncode == 0:
            proposal_url = response.get("proposal_url")
            proposal_web_url = response.get("proposal_web_url")
            branch_name = response.get("branch_name")
            is_new = response.get("is_new")
            description = response.get("description")
            target_branch_url = response.get("target_branch_url")
            target_branch_web_url = response.get("target_branch_web_url")

            if proposal_url and is_new:
                if self.redis:
                    await self.redis.publish(
                        "merge-proposal",
                        json.dumps(
                            {
                                "url": proposal_url,
                                "web_url": proposal_web_url,
                                "status": "open",
                                "codebase": codebase,
                                "campaign": campaign,
                                "target_branch_url": target_branch_url,
                                "target_branch_web_url": target_branch_web_url,
                            }
                        ),
                    )

                new_merge_proposal_count.inc()
                merge_proposal_count.labels(status="open").inc()
                open_proposal_count.inc()
                if rate_limit_bucket:
                    if bucket_rate_limiter:
                        bucket_rate_limiter.inc(rate_limit_bucket)
                    bucket_proposal_count.labels(bucket=rate_limit_bucket).inc()

            return PublishResult(
                proposal_url=proposal_url,
                proposal_web_url=proposal_web_url,
                branch_name=branch_name,
                is_new=is_new,
                target_branch_url=target_branch_url,
                target_branch_web_url=target_branch_web_url,
                description=description,
            )

        raise AssertionError


async def consider_publish_run(
    conn: asyncpg.Connection,
    redis,
    *,
    config: Config,
    publish_worker: PublishWorker,
    vcs_managers,
    bucket_rate_limiter,
    run: state.Run,
    rate_limit_bucket,
    unpublished_branches,
    command: str,
    push_limit: Optional[int] = None,
    require_binary_diff: bool = False,
) -> dict[str, Optional[str]]:
    if run.revision is None:
        logger.warning(
            "Run %s is publish ready, but does not have revision set.",
            run.id,
            extra={"run_id": run.id},
        )
        return {}
    campaign_config = get_campaign_config(config, run.campaign)
    attempt_count = await get_publish_attempt_count(
        conn, run.revision, {"differ-unreachable"}
    )
    next_try_time = calculate_next_try_time(run.finish_time, attempt_count)
    if datetime.utcnow() < next_try_time:
        logger.info(
            "Not attempting to push %s / %s (%s) due to "
            "exponential backoff. Next try in %s.",
            run.codebase,
            run.campaign,
            run.id,
            next_try_time - datetime.utcnow(),
            extra={"run_id": run.id},
        )
        exponential_backoff_count.inc()
        return {}

    ms = [b[4] for b in unpublished_branches]
    if push_limit is not None and (MODE_PUSH in ms or MODE_ATTEMPT_PUSH in ms):
        if push_limit == 0:
            logger.info(
                "Not pushing %s / %s: push limit reached",
                run.codebase,
                run.campaign,
                extra={"run_id": run.id},
            )
            push_limit_count.inc()
            return {}
    if run.branch_url is None:
        logger.warning(
            "%s: considering publishing for branch without branch url",
            run.id,
            extra={"run_id": run.id},
        )
        missing_branch_url_count.inc()
        # TODO(jelmer): Support target_branch_url ?
        return {}

    last_mps: list[tuple[str, str]] = await get_previous_mp_status(
        conn, run.codebase, run.campaign
    )
    if any(last_mp[1] in ("rejected", "closed") for last_mp in last_mps):
        logger.warning(
            "%s: last merge proposal was rejected by maintainer: %r",
            run.id,
            last_mps,
            extra={"run_id": run.id},
        )
        rejected_last_mp_count.inc()
        return {}

    actual_modes: dict[str, Optional[str]] = {}
    for (
        role,
        _remote_name,
        _base_revision,
        _revision,
        publish_mode,
        max_frequency_days,
    ) in unpublished_branches:
        if publish_mode is None:
            logger.warning(
                "%s: No publish mode for branch with role %s",
                run.id,
                role,
                extra={"run_id": run.id, "role": role},
            )
            missing_publish_mode_count.labels(role=role).inc()
            continue
        if role == "main" and None in actual_modes.values():
            logger.warning(
                "%s: Skipping branch with role %s, as not all "
                "auxiliary branches were published.",
                run.id,
                role,
                extra={"run_id": run.id, "role": role},
            )
            unpublished_aux_branches_count.labels(role=role).inc()
            continue
        actual_modes[role] = await publish_from_policy(
            conn=conn,
            campaign_config=campaign_config,
            publish_worker=publish_worker,
            bucket_rate_limiter=bucket_rate_limiter,
            vcs_managers=vcs_managers,
            run=run,
            role=role,
            rate_limit_bucket=rate_limit_bucket,
            target_branch_url=run.target_branch_url or run.branch_url,
            mode=publish_mode,
            max_frequency_days=max_frequency_days,
            command=command,
            redis=redis,
            require_binary_diff=require_binary_diff,
            force=False,
            requester="publisher (publish pending)",
        )

    return actual_modes


async def iter_publish_ready(
    conn: asyncpg.Connection,
    *,
    run_id: Optional[str] = None,
) -> AsyncIterable[
    tuple[
        state.Run,
        str,
        str,
        list[
            tuple[
                str,
                Optional[str],
                str,
                bytes,
                bytes,
                Optional[str],
                Optional[int],
                Optional[str],
            ]
        ],
    ]
]:
    args: list[Any] = []
    query = """
SELECT * FROM publish_ready
"""
    conditions = []
    if run_id is not None:
        args.append(run_id)
        conditions.append(f"id = ${len(args)}")
    conditions.append("publish_status = 'approved'")
    conditions.append("change_set_state IN ('ready', 'publishing')")

    any_publishable_branches = (
        "exists (select from unnest(unpublished_branches) where "
        "mode in ('propose', 'attempt-push', 'push-derived', 'push'))"
    )

    conditions.append(any_publishable_branches)

    if conditions:
        query += " WHERE " + " AND ".join(conditions)

    order_by = ["change_set_state = 'publishing' DESC"]

    order_by.extend(["value DESC NULLS LAST", "finish_time DESC"])

    if order_by:
        query += " ORDER BY " + ", ".join(order_by) + " "

    for record in await conn.fetch(query, *args):
        yield tuple(  # type: ignore
            [
                state.Run.from_row(record),
                record["rate_limit_bucket"],
                record["policy_command"],
                record["unpublished_branches"],
            ]
        )


async def publish_pending_ready(
    *,
    db,
    redis,
    config,
    publish_worker,
    bucket_rate_limiter,
    vcs_managers,
    push_limit: Optional[int] = None,
    require_binary_diff: bool = False,
):
    start = time.time()
    actions: dict[Optional[str], int] = {}

    async with db.acquire() as conn1, db.acquire() as conn:
        async for (
            run,
            rate_limit_bucket,
            command,
            unpublished_branches,
        ) in iter_publish_ready(conn1):
            actual_modes = await consider_publish_run(
                conn,
                redis=redis,
                config=config,
                publish_worker=publish_worker,
                vcs_managers=vcs_managers,
                bucket_rate_limiter=bucket_rate_limiter,
                run=run,
                command=command,
                rate_limit_bucket=rate_limit_bucket,
                unpublished_branches=unpublished_branches,
                push_limit=push_limit,
                require_binary_diff=require_binary_diff,
            )
            for actual_mode in actual_modes.values():
                if actual_mode is None:
                    continue
                actions.setdefault(actual_mode, 0)
                actions[actual_mode] += 1
            if MODE_PUSH in actual_modes.values() and push_limit is not None:
                push_limit -= 1

    logger.info("Actions performed: %r", actions)
    logger.info(
        "Done publishing pending changes; duration: %.2fs" % (time.time() - start)
    )

    last_publish_pending_success.set_to_current_time()


async def handle_publish_failure(e, conn, run, bucket: str) -> tuple[str, str]:
    unchanged_run = await conn.fetchrow(
        "SELECT result_code, revision FROM last_runs "
        "WHERE revision = $2 AND codebase = $1 and result_code = 'success'",
        run.codebase,
        run.main_branch_revision.decode("utf-8"),
    )

    code = e.code
    description = e.description
    if e.code == "merge-conflict":
        logger.info("Merge proposal would cause conflict; restarting.")
        await do_schedule(
            conn,
            campaign=run.campaign,
            change_set=run.change_set,
            codebase=run.codebase,
            requester="publisher (pre-creation merge conflict)",
            bucket=bucket,
        )
    elif e.code == "diverged-branches":
        logger.info("Branches have diverged; restarting.")
        await do_schedule(
            conn,
            campaign=run.campaign,
            change_set=run.change_set,
            requester="publisher (diverged branches)",
            bucket=bucket,
            codebase=run.codebase,
        )
    elif e.code == "missing-build-diff-self":
        if run.result_code != "success":
            description = "Missing build diff; run was not actually successful?"
        else:
            description = "Missing build artifacts, rescheduling"
            await do_schedule(
                conn,
                campaign=run.campaign,
                change_set=run.change_set,
                refresh=True,
                requester="publisher (missing build artifacts - self)",
                bucket=bucket,
                codebase=run.codebase,
            )
    elif e.code == "missing-build-diff-control":
        if unchanged_run and unchanged_run["result_code"] != "success":
            description = "Missing build diff; last control run failed ({}).".format(
                unchanged_run["result_code"]
            )
        elif unchanged_run and unchanged_run["result_code"] == "success":
            description = (
                "Missing build diff due to control run, but successful "
                "control run exists. Rescheduling."
            )
            await do_schedule_control(
                conn,
                main_branch_revision=unchanged_run["revision"].encode("utf-8"),
                refresh=True,
                requester="publisher (missing build artifacts - control)",
                bucket=bucket,
                codebase=run.codebase,
            )
        else:
            description = "Missing binary diff; requesting control run."
            if run.main_branch_revision is not None:
                await do_schedule_control(
                    conn,
                    main_branch_revision=run.main_branch_revision,
                    requester="publisher (missing control run for diff)",
                    bucket=bucket,
                    codebase=run.codebase,
                )
            else:
                logger.warning(
                    "Successful run (%s) does not have main branch revision set",
                    run.id,
                )
    return (code, description)


async def already_published(
    conn: asyncpg.Connection,
    target_branch_url: str,
    branch_name: str,
    revision: bytes,
    modes: list[str],
) -> bool:
    row = await conn.fetchrow(
        """\
SELECT * FROM publish
WHERE mode = ANY($1::publish_mode[]) AND revision = $2 AND target_branch_url = $3 AND branch_name = $4
""",
        modes,
        revision.decode("utf-8"),
        target_branch_url,
        branch_name,
    )
    if row:
        return True
    return False


async def get_open_merge_proposal(
    conn: asyncpg.Connection, codebase: str, branch_name: str
):
    query = """\
SELECT
    merge_proposal.revision,
    merge_proposal.url
FROM
    merge_proposal
INNER JOIN publish ON merge_proposal.url = publish.merge_proposal_url
WHERE
    merge_proposal.status = 'open' AND
    merge_proposal.codebase = $1 AND
    publish.branch_name = $2
ORDER BY timestamp DESC
"""
    return await conn.fetchrow(query, codebase, branch_name)


async def check_last_published(
    conn: asyncpg.Connection, campaign: str, codebase: str
) -> Optional[datetime]:
    return await conn.fetchval(
        """
SELECT timestamp from publish left join run on run.revision = publish.revision
WHERE run.suite = $1 and run.codebase = $2 AND publish.result_code = 'success'
order by timestamp desc limit 1
""",
        campaign,
        codebase,
    )


async def store_publish(
    conn: asyncpg.Connection,
    *,
    change_set: str,
    codebase: str,
    branch_name: Optional[str],
    target_branch_url: Optional[str],
    target_branch_web_url: Optional[str],
    main_branch_revision: Optional[bytes],
    revision: Optional[bytes],
    role: str,
    mode: str,
    result_code: str,
    description: str,
    merge_proposal_url: Optional[str] = None,
    publish_id: Optional[str] = None,
    requester: Optional[str] = None,
    run_id: Optional[str] = None,
) -> None:
    if isinstance(revision, bytes):
        revision = revision.decode("utf-8")  # type: ignore
    if isinstance(main_branch_revision, bytes):
        main_branch_revision = main_branch_revision.decode("utf-8")  # type: ignore
    async with conn.transaction():
        if result_code == "success":
            if merge_proposal_url:
                assert mode == "propose"
                await conn.execute(
                    "INSERT INTO merge_proposal "
                    "(url, status, revision, last_scanned, "
                    " target_branch_url, codebase) "
                    "VALUES ($1, 'open', $2, NOW(), $3, $4) ON CONFLICT (url) "
                    "DO UPDATE SET "
                    "revision = EXCLUDED.revision, "
                    "last_scanned = EXCLUDED.last_scanned, "
                    "target_branch_url = EXCLUDED.target_branch_url, "
                    "codebase = EXCLUDED.codebase",
                    merge_proposal_url,
                    revision,
                    target_branch_url,
                    codebase,
                )
            else:
                if revision is None:
                    raise AssertionError
                assert mode in ("push", "push-derived")
                assert run_id is not None
                if mode == "push":
                    await conn.execute(
                        "UPDATE new_result_branch "
                        "SET absorbed = true WHERE run_id = $1 AND role = $2",
                        run_id,
                        role,
                    )
        await conn.execute(
            "INSERT INTO publish (branch_name, "
            "main_branch_revision, revision, role, mode, result_code, "
            "description, merge_proposal_url, id, requester, change_set, run_id, "
            "target_branch_url, target_branch_web_url, codebase) "
            "values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, "
            "$13, $14, $15) ",
            branch_name,
            main_branch_revision,
            revision,
            role,
            mode,
            result_code,
            description,
            merge_proposal_url,
            publish_id,
            requester,
            change_set,
            run_id,
            target_branch_url,
            target_branch_web_url,
            codebase,
        )
        if result_code == "success":
            await conn.execute(
                "UPDATE change_set SET state = 'publishing' WHERE state = 'ready' AND id = $1",
                change_set,
            )
            # TODO(jelmer): if there is nothing left to publish, then mark this
            # change_set as done


async def publish_from_policy(
    *,
    conn: asyncpg.Connection,
    redis,
    campaign_config: Campaign,
    publish_worker: PublishWorker,
    bucket_rate_limiter: RateLimiter,
    vcs_managers: dict[str, VcsManager],
    run: state.Run,
    role: str,
    rate_limit_bucket: Optional[str],
    target_branch_url: str,
    mode: str,
    max_frequency_days: Optional[int],
    command: str,
    require_binary_diff: bool = False,
    force: bool = False,
    requester: Optional[str] = None,
) -> Optional[str]:
    if not command:
        logger.warning("no command set for %s", run.id)
        return None
    if command != run.command:
        command_changed_count.inc()
        logger.warning(
            "Not publishing %s/%s: command has changed. "
            "Build used %r, now: %r. Rescheduling.",
            run.codebase,
            run.campaign,
            run.command,
            command,
            extra={"run_id": run.id, "role": role},
        )
        await do_schedule(
            conn,
            campaign=run.campaign,
            change_set=run.change_set,
            command=command,
            bucket="update-new-mp",
            refresh=True,
            requester=f"publisher (changed policy: {run.command!r} ⇒ {command!r})",
            codebase=run.codebase,
        )
        return None

    publish_id = str(uuid.uuid4())
    if mode in (None, MODE_BUILD_ONLY, MODE_SKIP):
        return None
    if run.result_branches is None:
        logger.warning(
            "no result branches for %s", run.id, extra={"run_id": run.id, "role": role}
        )
        no_result_branches_count.inc()
        return None
    try:
        (remote_branch_name, base_revision, revision) = run.get_result_branch(role)
    except KeyError:
        missing_main_result_branch_count.inc()
        logger.warning(
            "unable to find branch with role %s: %s",
            role,
            run.id,
            extra={"run_id": run.id, "role": role},
        )
        return None

    target_branch_url = role_branch_url(target_branch_url, remote_branch_name)

    if not force and await already_published(
        conn,
        run.branch_url,
        campaign_config.branch_name,
        revision,
        [MODE_PROPOSE, MODE_PUSH] if mode == MODE_ATTEMPT_PUSH else [mode],
    ):
        return None
    if mode in (MODE_PROPOSE, MODE_ATTEMPT_PUSH):
        open_mp = await get_open_merge_proposal(
            conn, run.codebase, campaign_config.branch_name
        )
        if not open_mp:
            try:
                if rate_limit_bucket:
                    bucket_rate_limiter.check_allowed(rate_limit_bucket)
            except RateLimited as e:
                proposal_rate_limited_count.labels(
                    codebase=run.codebase, campaign=run.campaign
                ).inc()
                logger.debug(
                    "Not creating proposal for %s/%s: %s",
                    run.codebase,
                    run.campaign,
                    e,
                    extra={"run_id": run.id},
                )
                mode = MODE_BUILD_ONLY
            if max_frequency_days is not None:
                last_published = await check_last_published(
                    conn, run.campaign, run.codebase
                )
                if (
                    last_published is not None
                    and (datetime.utcnow() - last_published).days < max_frequency_days
                ):
                    logger.debug(
                        "Not creating proposal for %s/%s: "
                        "was published already in last %d days (at %s)",
                        run.codebase,
                        run.campaign,
                        max_frequency_days,
                        last_published,
                        extra={"run_id": run.id},
                    )
                    mode = MODE_BUILD_ONLY
    if mode in (MODE_BUILD_ONLY, MODE_SKIP):
        return None

    if base_revision is None:
        unchanged_run = None
    else:
        unchanged_run = await conn.fetchrow(
            "SELECT id, result_code FROM last_runs "
            "WHERE codebase = $1 AND revision = $2 AND result_code = 'success'",
            run.codebase,
            base_revision.decode("utf-8"),
        )

    # TODO(jelmer): Make this more generic
    if (
        unchanged_run
        and unchanged_run["result_code"] in ("debian-upstream-metadata-invalid",)
        and run.campaign == "lintian-fixes"
    ):
        require_binary_diff = False

    logger.info(
        "Publishing %s / %r / %s (mode: %s)", run.codebase, run.command, role, mode
    )
    try:
        publish_result = await publish_worker.publish_one(
            campaign=run.campaign,
            codebase=run.codebase,
            extra_context={},
            command=run.command,
            codemod_result=run.result,
            target_branch_url=target_branch_url,
            mode=mode,
            role=role,
            revision=revision,
            log_id=run.id,
            unchanged_id=(cast(str, unchanged_run["id"]) if unchanged_run else None),
            derived_branch_name=await derived_branch_name(
                conn, campaign_config, run, role
            ),
            rate_limit_bucket=rate_limit_bucket,
            vcs_manager=vcs_managers[run.vcs_type],
            require_binary_diff=require_binary_diff,
            bucket_rate_limiter=bucket_rate_limiter,
            result_tags=run.result_tags,
            allow_create_proposal=run_sufficient_for_proposal(
                campaign_config, run.value
            ),
            commit_message_template=(
                campaign_config.merge_proposal.commit_message
                if campaign_config.merge_proposal
                else None
            ),
            title_template=(
                campaign_config.merge_proposal.title
                if campaign_config.merge_proposal
                else None
            ),
        )
    except BranchBusy as e:
        logger.info("Branch %r was busy", e.branch_url)
        return None
    except PublishFailure as e:
        code, description = await handle_publish_failure(
            e, conn, run, bucket="update-new-mp"
        )
        publish_result = PublishResult(description="Nothing to do")
        if e.code == "nothing-to-do":
            logger.info("Nothing to do.")
        else:
            logger.info("Failed(%s): %s", code, description)
    else:
        code = "success"
        description = "Success"

    if mode == MODE_ATTEMPT_PUSH:
        if publish_result.proposal_url:
            mode = MODE_PROPOSE
        else:
            mode = MODE_PUSH

    await store_publish(
        conn,
        change_set=run.change_set,
        codebase=run.codebase,
        branch_name=publish_result.branch_name,
        main_branch_revision=base_revision,
        revision=revision,
        role=role,
        mode=mode,
        result_code=code,
        description=description,
        merge_proposal_url=(
            publish_result.proposal_url if publish_result.proposal_url else None
        ),
        publish_id=publish_id,
        target_branch_url=publish_result.target_branch_url,
        target_branch_web_url=publish_result.target_branch_web_url,
        requester=requester,
        run_id=run.id,
    )

    if code == "success" and mode == MODE_PUSH:
        # TODO(jelmer): Call state.update_branch_status() for the
        # main branch URL
        pass

    if code == "success":
        publish_delay = datetime.utcnow() - run.finish_time
        publish_latency.observe(publish_delay.total_seconds())
    else:
        publish_delay = None

    topic_entry: dict[str, Any] = {
        "id": publish_id,
        "codebase": run.codebase,
        "campaign": run.campaign,
        "proposal_url": publish_result.proposal_url or None,
        "mode": mode,
        "main_branch_url": publish_result.target_branch_url,
        "main_branch_browse_url": publish_result.target_branch_web_url,
        "branch_name": publish_result.branch_name,
        "result_code": code,
        "result": run.result,
        "run_id": run.id,
        "publish_delay": (publish_delay.total_seconds() if publish_delay else None),
    }

    await pubsub_publish(redis, topic_entry)

    if code == "success":
        return mode

    return None


async def pubsub_publish(redis, topic_entry):
    await redis.publish("publish", json.dumps(topic_entry))


def run_sufficient_for_proposal(
    campaign_config: Campaign, run_value: Optional[int]
) -> bool:
    if (
        run_value is not None
        and campaign_config.merge_proposal is not None
        and campaign_config.merge_proposal.value_threshold
    ):
        return run_value >= campaign_config.merge_proposal.value_threshold
    else:
        # Assume yes, if the run doesn't have an associated value or if there
        # is no threshold configured.
        return True


async def publish_and_store(
    *,
    db: asyncpg.Connection,
    redis,
    campaign_config: Campaign,
    publish_worker: PublishWorker,
    publish_id: str,
    run: state.Run,
    mode: str,
    role: str,
    rate_limit_bucket: Optional[str],
    vcs_managers: dict[str, VcsManager],
    bucket_rate_limiter: RateLimiter,
    allow_create_proposal: bool = True,
    require_binary_diff: bool = False,
    requester: Optional[str] = None,
):
    remote_branch_name, base_revision, revision = run.get_result_branch(role)

    target_branch_url = role_branch_url(
        run.target_branch_url or run.branch_url, remote_branch_name
    )

    if allow_create_proposal is None:
        allow_create_proposal = run_sufficient_for_proposal(campaign_config, run.value)

    async with db.acquire() as conn:
        if run.main_branch_revision:
            unchanged_run_id = await conn.fetchval(
                "SELECT id FROM run "
                "WHERE revision = $2 AND codebase = $1 and result_code = 'success' "
                "ORDER BY finish_time DESC LIMIT 1",
                run.codebase,
                run.main_branch_revision.decode("utf-8"),
            )
        else:
            unchanged_run_id = None

        try:
            publish_result = await publish_worker.publish_one(
                campaign=run.campaign,
                codebase=run.codebase,
                extra_context={},
                command=run.command,
                codemod_result=run.result,
                target_branch_url=target_branch_url,
                mode=mode,
                role=role,
                revision=revision,
                log_id=run.id,
                unchanged_id=unchanged_run_id,
                derived_branch_name=await derived_branch_name(
                    conn, campaign_config, run, role
                ),
                rate_limit_bucket=rate_limit_bucket,
                vcs_manager=vcs_managers[run.vcs_type],
                require_binary_diff=require_binary_diff,
                allow_create_proposal=allow_create_proposal,
                bucket_rate_limiter=bucket_rate_limiter,
                result_tags=run.result_tags,
                commit_message_template=(
                    campaign_config.merge_proposal.commit_message
                    if campaign_config.merge_proposal
                    else None
                ),
                title_template=(
                    campaign_config.merge_proposal.title
                    if campaign_config.merge_proposal
                    else None
                ),
            )
        except BranchBusy as e:
            logger.debug("Branch %r was busy while publishing", e.branch_url)
            return
        except PublishFailure as e:
            await store_publish(
                conn,
                change_set=run.change_set,
                codebase=run.codebase,
                target_branch_url=target_branch_url,
                target_branch_web_url=None,
                branch_name=campaign_config.branch_name,
                main_branch_revision=run.main_branch_revision,
                revision=run.revision,
                role=role,
                mode=e.mode,
                result_code=e.code,
                description=e.description,
                publish_id=publish_id,
                requester=requester,
                run_id=run.id,
            )
            publish_entry = {
                "id": publish_id,
                "mode": e.mode,
                "result_code": e.code,
                "description": e.description,
                "campaign": run.campaign,
                "main_branch_url": target_branch_url,
                "result": run.result,
                "codebase": run.codebase,
            }

            await pubsub_publish(redis, publish_entry)
            return

        if mode == MODE_ATTEMPT_PUSH:
            if publish_result.proposal_url:
                mode = MODE_PROPOSE
            else:
                mode = MODE_PUSH

        await store_publish(
            conn,
            change_set=run.change_set,
            codebase=run.codebase,
            branch_name=publish_result.branch_name,
            main_branch_revision=run.main_branch_revision,
            revision=run.revision,
            role=role,
            mode=mode,
            result_code="success",
            description="Success",
            merge_proposal_url=(
                publish_result.proposal_url if publish_result.proposal_url else None
            ),
            target_branch_url=publish_result.target_branch_url,
            target_branch_web_url=publish_result.target_branch_web_url,
            publish_id=publish_id,
            requester=requester,
            run_id=run.id,
        )

        publish_delay = datetime.utcnow() - run.finish_time
        publish_latency.observe(publish_delay.total_seconds())

        publish_entry = {
            "id": publish_id,
            "campaign": run.campaign,
            "proposal_url": publish_result.proposal_url or None,
            "mode": mode,
            "main_branch_url": publish_result.target_branch_url,
            "main_branch_browse_url": publish_result.target_branch_web_url,
            "branch_name": publish_result.branch_name,
            "result_code": "success",
            "result": run.result,
            "role": role,
            "publish_delay": publish_delay.total_seconds(),
            "run_id": run.id,
            "codebase": run.codebase,
        }

        await pubsub_publish(redis, publish_entry)


async def get_publish_attempt_count(
    conn: asyncpg.Connection, revision: bytes, transient_result_codes: set[str]
) -> int:
    return await conn.fetchval(
        "select count(*) from publish where revision = $1 "
        "and result_code != ALL($2::text[])",
        revision.decode("utf-8"),
        transient_result_codes,
    )


@routes.get("/{campaign}/merge-proposals", name="campaign-merge-proposals")
@routes.get("/c/{codebase}/merge-proposals", name="codebase-merge-proposals")
@routes.get("/merge-proposals", name="merge-proposals")
async def handle_merge_proposal_list(request):
    response_obj = []
    codebase = request.match_info.get("codebase")
    campaign = request.match_info.get("campaign")
    async with request.app["db"].acquire() as conn:
        args = []
        query = """
    SELECT
        DISTINCT ON (merge_proposal.url)
        merge_proposal.url AS url, merge_proposal.status AS status,
        run.suite
    FROM
        merge_proposal
    LEFT JOIN run
    ON merge_proposal.revision = run.revision AND run.result_code = 'success'
    """
        cond = []
        if codebase is not None:
            args.append(codebase)
            cond.append(f"run.codebase = ${len(args)}")
        if campaign:
            args.append(campaign)
            cond.append(f"run.suite = ${len(args)}")
        if cond:
            query += "WHERE " + " AND ".join(cond)
        query += " ORDER BY merge_proposal.url, run.finish_time DESC"
        for row in await conn.fetch(query, *args):
            response_obj.append({"url": row["url"], "status": row["status"]})
    return web.json_response(response_obj)


@routes.get("/absorbed")
async def handle_absorbed(request):
    try:
        since = datetime.fromisoformat(request.query["since"])
    except KeyError:
        extra = ""
        args = []
    else:
        args = [since]
        extra = f" AND absorbed_at >= ${len(args)}"

    ret = []
    async with request.app["db"].acquire() as conn:
        query = """\
SELECT
   change_set,
   delay,
   campaign,
   result,
   id,
   absorbed_at,
   merged_by,
   merge_proposal_url
FROM absorbed_runs
"""
        for row in await conn.fetch(query + extra, *args):
            ret.append(
                {
                    "mode": row["mode"],
                    "change_set": row["change_set"],
                    "delay": row["delay"].total_seconds,
                    "campaign": row["campaign"],
                    "merged-by": row["merged_by"],
                    "merged-by-url": await asyncio.to_thread(
                        get_merged_by_user_url,
                        row["merge_proposal_url"],
                        row["merged_by"],
                    ),
                    "absorbed-at": row["absorbed-at"],
                    "id": row["id"],
                    "result": row["result"],
                }
            )
    return web.json_response(ret)


@routes.get("/policy/{name}", name="get-policy")
async def handle_policy_get(request):
    name = request.match_info["name"]
    async with request.app["db"].acquire() as conn:
        row = await conn.fetchrow(
            "SELECT * FROM named_publish_policy WHERE name = $1", name
        )
    if not row:
        return web.json_response({"reason": "Publish policy not found"}, status=404)
    return web.json_response(
        {
            "rate_limit_bucket": row["rate_limit_bucket"],
            "per_branch": {
                p["role"]: {
                    "mode": p["mode"],
                    "max_frequency_days": p["frequency_days"],
                }
                for p in row["publish"]
            },
        }
    )


@routes.get("/policy", name="get-full-policy")
async def handle_full_policy_get(request):
    async with request.app["db"].acquire() as conn:
        rows = await conn.fetch("SELECT * FROM named_publish_policy")
    return web.json_response(
        {
            row["name"]: {
                "rate_limit_bucket": row["rate_limit_bucket"],
                "per_branch": {
                    p["role"]: {
                        "mode": p["mode"],
                        "max_frequency_days": p["frequency_days"],
                    }
                    for p in row["per_branch_policy"]
                },
            }
            for row in rows
        }
    )


@routes.put("/policy/{name}", name="put-policy")
async def handle_policy_put(request):
    name = request.match_info["name"]
    policy = await request.json()
    async with request.app["db"].acquire() as conn:
        await conn.execute(
            "INSERT INTO named_publish_policy "
            "(name, per_branch_policy, rate_limit_bucket) "
            "VALUES ($1, $2, $3) ON CONFLICT (name) "
            "DO UPDATE SET "
            "per_branch_policy = EXCLUDED.per_branch_policy, "
            "rate_limit_bucket = EXCLUDED.rate_limit_bucket",
            name,
            [
                (r, v["mode"], v.get("max_frequency_days"))
                for (r, v) in policy["per_branch"].items()
            ],
            policy.get("rate_limit_bucket"),
        )
    # TODO(jelmer): Call consider_publish_run
    return web.json_response({})


@routes.put("/policy", name="put-full-policy")
async def handle_full_policy_put(request):
    policy = await request.json()
    async with request.app["db"].acquire() as conn, conn.transaction():
        entries = [
            (
                name,
                [
                    (r, b["mode"], b.get("max_frequency_days"))
                    for (r, b) in v["per_branch"].items()
                ],
                v.get("rate_limit_bucket"),
            )
            for (name, v) in policy.items()
        ]
        await conn.executemany(
            "INSERT INTO named_publish_policy "
            "(name, per_branch_policy, rate_limit_bucket) "
            "VALUES ($1, $2, $3) ON CONFLICT (name) "
            "DO UPDATE SET "
            "per_branch_policy = EXCLUDED.per_branch_policy, "
            "rate_limit_bucket = EXCLUDED.rate_limit_bucket",
            entries,
        )
        await conn.execute(
            "DELETE FROM named_publish_policy WHERE NOT (name = ANY($1::text[]))",
            policy.keys(),
        )
    # TODO(jelmer): Call consider_publish_run
    return web.json_response({})


@routes.delete("/policy/{name}", name="delete-policy")
async def handle_policy_del(request):
    name = request.match_info["name"]
    async with request.app["db"].acquire() as conn:
        try:
            await conn.execute("DELETE FROM named_publish_policy WHERE name = $1", name)
        except asyncpg.ForeignKeyViolationError:
            # There's a candidate that still references this
            # publish policy
            return web.json_response({}, status=412)
    return web.json_response({})


@routes.post("/merge-proposal", name="merge-proposal")
async def update_merge_proposal_request(request):
    post = await request.post()
    async with request.app["db"].acquire() as conn:
        async with conn.transaction():
            row = await conn.fetchrow(
                "SELECT status FROM merge_proposal WHERE url = $1", post["url"]
            )
            if row["status"] in CLOSED_STATUSES and post["status"] in CLOSED_STATUSES:
                pass
            elif row["status"] == "open" and post["status"] in CLOSED_STATUSES:
                mp = await asyncio.to_thread(get_proposal_by_url, post["url"])
                if post.get("comment"):
                    logger.info(
                        "%s: %s", mp.url, post["comment"], extra={"mp_url": mp.url}
                    )
                    try:
                        await asyncio.to_thread(mp.post_comment, post["comment"])
                    except PermissionDenied as e:
                        logger.warning(
                            "Permission denied posting comment to %s: %s",
                            mp.url,
                            e,
                            extra={"mp_url": mp.url},
                        )

                try:
                    await asyncio.to_thread(mp.close)
                except PermissionDenied as e:
                    logger.warning(
                        "Permission denied closing merge request %s: %s",
                        mp.url,
                        e,
                        extra={"mp_url": mp.url},
                    )
                    raise
            else:
                raise web.HTTPBadRequest(
                    text=f"no transition from {row['url']} to {post['url']}"
                )

            await conn.execute(
                "UPDATE merge_proposal SET status = $1 WHERE url = $2",
                post["status"],
                post["url"],
            )

    return web.Response(text="updated")


@routes.post("/consider/{run_id}", name="consider")
async def consider_request(request):
    run_id = request.match_info["run_id"]

    async def run():
        async with request.app["db"].acquire() as conn:
            async for (
                run,
                rate_limit_bucket,
                command,
                unpublished_branches,
            ) in iter_publish_ready(conn, run_id=run_id):
                break
            else:
                return
            await consider_publish_run(
                conn,
                redis=request.app["redis"],
                config=request.app["config"],
                publish_worker=request.app["publish_worker"],
                vcs_managers=request.app["vcs_managers"],
                bucket_rate_limiter=request.app["bucket_rate_limiter"],
                run=run,
                command=command,
                rate_limit_bucket=rate_limit_bucket,
                unpublished_branches=unpublished_branches,
                require_binary_diff=request.app["require_binary_diff"],
            )

    await spawn(request, run())
    return web.json_response({}, status=200)


async def get_publish_policy(conn: asyncpg.Connection, codebase: str, campaign: str):
    row = await conn.fetchrow(
        "SELECT per_branch_policy, command, rate_limit_bucket "
        "FROM candidate "
        "LEFT JOIN named_publish_policy "
        "ON named_publish_policy.name = candidate.publish_policy "
        "WHERE codebase = $1 AND suite = $2",
        codebase,
        campaign,
    )
    if row:
        return (
            {
                v["role"]: (v["mode"], v["frequency_days"])
                for v in row["per_branch_policy"]
            },
            row["command"],
            row["rate_limit_bucket"],
        )
    return None, None, None


@routes.get("/publish/{publish_id}", name="publish-details")
async def handle_publish_id(request):
    publish_id = request.match_info["publish_id"]
    async with request.app["db"].acquire() as conn:
        row = await conn.fetchrow(
            publish_id,
        )
        if row:
            raise web.HTTPNotFound(text=f"no such publish: {publish_id}")
    return web.json_response({})


@routes.post("/{campaign}/{codebase}/publish", name="publish")
async def publish_request(request):
    vcs_managers = request.app["vcs_managers"]
    bucket_rate_limiter = request.app["bucket_rate_limiter"]
    codebase = request.match_info["codebase"]
    campaign = request.match_info["campaign"]
    role = request.query.get("role")
    post = await request.post()
    mode = post.get("mode")
    async with request.app["db"].acquire() as conn:
        run = await get_last_effective_run(conn, codebase, campaign)
        if run is None:
            return web.json_response({}, status=400)

        publish_policy, _, rate_limit_bucket = await get_publish_policy(
            conn, codebase, campaign
        )

        logger.info("Handling request to publish %s/%s", codebase, campaign)

    if role is not None:
        roles = [role]
    else:
        roles = [e[0] for e in run.result_branches]

    if mode:
        branches = [(r, mode) for r in roles]
    else:
        branches = [(r, publish_policy.get(r, (MODE_SKIP, None))[0]) for r in roles]

    publish_ids = {}
    for role, mode in branches:
        publish_id = str(uuid.uuid4())
        publish_ids[role] = publish_id

        logger.info(".. publishing for role %s: %s", role, mode)

        if mode in (MODE_SKIP, MODE_BUILD_ONLY):
            continue

        await spawn(
            request,
            publish_and_store(
                db=request.app["db"],
                redis=request.app["redis"],
                campaign_config=get_campaign_config(
                    request.app["config"], run.campaign
                ),
                publish_worker=request.app["publish_worker"],
                publish_id=publish_id,
                run=run,
                mode=mode,
                role=role,
                rate_limit_bucket=rate_limit_bucket,
                vcs_managers=vcs_managers,
                bucket_rate_limiter=bucket_rate_limiter,
                allow_create_proposal=True,
                require_binary_diff=False,
                requester=post.get("requester"),
            ),
        )

    if not publish_ids:
        return web.json_response(
            {"run_id": run.id, "code": "done", "description": "Nothing to do"}
        )

    return web.json_response(
        {"run_id": run.id, "mode": mode, "publish_ids": publish_ids}, status=202
    )


@routes.get("/credentials", name="credentials")
async def credentials_request(request):
    ssh_keys = []
    for entry in os.scandir(os.path.expanduser("~/.ssh")):
        if entry.name.endswith(".pub"):
            with open(entry.path) as f:
                ssh_keys.extend([line.strip() for line in f.readlines()])
    pgp_keys = []
    for gpg_entry in list(request.app["gpg"].keylist(secret=True)):
        pgp_keys.append(request.app["gpg"].key_export_minimal(gpg_entry.fpr).decode())
    hosting = []
    for name, forge_cls in forges.items():
        for instance in forge_cls.iter_instances():
            try:
                current_user = instance.get_current_user()
            except ForgeLoginRequired:
                continue
            except UnsupportedForge:
                # WTF? Well, whatever.
                continue
            except RedirectRequested:
                # This should never happen; forge implementation is meant
                # to either ignore or handle this redirect.
                continue
            if current_user:
                current_user_url = instance.get_user_url(current_user)
            else:
                current_user_url = None
            forge = {
                "kind": name,
                "name": instance.name,
                "url": instance.base_url,
                "user": current_user,
                "user_url": current_user_url,
            }
            hosting.append(forge)

    return web.json_response(
        {
            "ssh_keys": ssh_keys,
            "pgp_keys": pgp_keys,
            "hosting": hosting,
        }
    )


async def create_app(
    *,
    vcs_managers: dict[str, VcsManager],
    db: asyncpg.pool.Pool,
    redis,
    config,
    publish_worker: Optional[PublishWorker] = None,
    forge_rate_limiter: Optional[dict[str, datetime]] = None,
    bucket_rate_limiter: Optional[RateLimiter] = None,
    require_binary_diff: bool = False,
    push_limit: Optional[int] = None,
    modify_mp_limit: Optional[int] = None,
):
    trailing_slash_redirect = normalize_path_middleware(append_slash=True)
    app = web.Application(
        middlewares=[trailing_slash_redirect, state.asyncpg_error_middleware]
    )
    app.router.add_routes(routes)
    app["gpg"] = gpg.Context(armor=True)
    app["publish_worker"] = publish_worker
    app["vcs_managers"] = vcs_managers
    app["db"] = db
    app["redis"] = redis
    app["config"] = config
    if bucket_rate_limiter is None:
        bucket_rate_limiter = NonRateLimiter()
    app["bucket_rate_limiter"] = bucket_rate_limiter
    if forge_rate_limiter is None:
        forge_rate_limiter = {}
    app["forge_rate_limiter"] = forge_rate_limiter
    app["modify_mp_limit"] = modify_mp_limit
    app["push_limit"] = push_limit
    app["require_binary_diff"] = require_binary_diff
    setup_metrics(app)
    setup_aiohttp_apispec(
        app=app,
        title="Publish Documentation",
        version=None,
        url="/swagger.json",
        swagger_path="/docs",
    )
    setup_aiojobs(app)
    return app


async def run_web_server(listen_addr, port, **kwargs):
    app = await create_app(**kwargs)
    config = kwargs["config"]
    endpoint = aiozipkin.create_endpoint("janitor.publish", ipv4=listen_addr, port=port)
    if config.zipkin_address:
        tracer = await aiozipkin.create(
            config.zipkin_address, endpoint, sample_rate=0.1
        )
    else:
        tracer = await aiozipkin.create_custom(endpoint)

    aiozipkin.setup(app, tracer)

    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, listen_addr, port)
    logger.info("Listening on %s:%s", listen_addr, port)
    await site.start()


@routes.get("/health", name="health")
async def handle_health(request):
    return web.Response(text="ok")


@routes.get("/ready", name="ready")
async def handle_ready(request):
    return web.Response(text="ok")


async def get_mp_status(mp):
    if await asyncio.to_thread(mp.is_merged):
        return "merged"
    elif await asyncio.to_thread(mp.is_closed):
        return "closed"
    else:
        return "open"


@routes.post("/scan", name="scan")
async def scan_request(request):
    async def scan():
        async with request.app["db"].acquire() as conn:
            await check_existing(
                conn=conn,
                redis=request.app["redis"],
                config=request.app["config"],
                publish_worker=request.app["publish_worker"],
                bucket_rate_limiter=request.app["bucket_rate_limiter"],
                forge_rate_limiter=request.app["forge_rate_limiter"],
                vcs_managers=request.app["vcs_managers"],
                modify_limit=request.app["modify_mp_limit"],
            )

    await spawn(request, scan())
    return web.Response(status=202, text="Scan started.")


@routes.post("/check-stragglers", name="check-stragglers")
async def refresh_stragglers(request):
    async def scan(db, redis, urls):
        async with db.acquire() as conn:
            proposal_info_manager = ProposalInfoManager(conn, redis)
            for url in urls:
                await check_straggler(proposal_info_manager, url)

    ndays = int(request.query.get("ndays", 5))
    async with request.app["db"].acquire() as conn:
        proposal_info_manager = ProposalInfoManager(conn, request.app["redis"])
        urls = await proposal_info_manager.iter_outdated_proposal_info_urls(ndays)
    await spawn(request, scan(request.app["db"], request.app["redis"], urls))
    return web.json_response(urls)


@routes.post("/refresh-status", name="refresh-status")
async def refresh_proposal_status_request(request):
    post = await request.post()
    try:
        url = post["url"]
    except KeyError as e:
        raise web.HTTPBadRequest(body="missing url parameter") from e
    logger.info("Request to refresh proposal status for %s", url)

    async def scan():
        mp = await asyncio.to_thread(get_proposal_by_url, url)
        async with request.app["db"].acquire() as conn:
            status = await get_mp_status(mp)
            try:
                await check_existing_mp(
                    conn=conn,
                    redis=request.app["redis"],
                    config=request.app["config"],
                    publish_worker=request.app["publish_worker"],
                    mp=mp,
                    status=status,
                    vcs_managers=request.app["vcs_managers"],
                    bucket_rate_limiter=request.app["bucket_rate_limiter"],
                )
            except NoRunForMergeProposal as e:
                logger.warning(
                    "Unable to find stored metadata for %s, skipping.", e.mp.url
                )
            except BranchRateLimited:
                logger.warning("Rate-limited accessing %s. ", mp.url)

    await spawn(request, scan())
    return web.Response(status=202, text="Refresh of proposal started.")


@routes.post("/autopublish", name="autopublish")
async def autopublish_request(request):
    async def autopublish():
        await publish_pending_ready(
            db=request.app["db"],
            redis=request.app["redis"],
            config=request.app["config"],
            publish_worker=request.app["publish_worker"],
            bucket_rate_limiter=request.app["bucket_rate_limiter"],
            vcs_managers=request.app["vcs_managers"],
            push_limit=request.app["push_limit"],
            require_binary_diff=request.app["require_binary_diff"],
        )

    await spawn(request, autopublish())
    return web.Response(status=202, text="Autopublish started.")


@routes.get("/rate-limits/{bucket}", name="bucket-rate-limits")
async def bucket_rate_limits_request(request):
    bucket_rate_limiter = request.app["bucket_rate_limiter"]

    stats = bucket_rate_limiter.get_stats()

    (current_open, max_open) = stats.get(request.match_info["bucket"], (None, None))

    ret = {
        "open": current_open,
        "max_open": max_open,
        "remaining": None
        if (current_open is None or max_open is None)
        else max_open - current_open,
    }

    return web.json_response(ret)


async def get_previous_mp_status(conn, codebase: str, campaign: str):
    rows = await conn.fetch(
        """\
WITH per_run_mps AS (
    SELECT run.id AS run_id, run.finish_time,
    merge_proposal.url AS mp_url, merge_proposal.status AS mp_status
    FROM run
    LEFT JOIN merge_proposal ON run.revision = merge_proposal.revision
    WHERE run.codebase = $1
    AND run.suite = $2
    AND run.result_code = 'success'
    AND merge_proposal.status NOT IN ('open', 'abandoned')
    GROUP BY run.id, merge_proposal.url
)
SELECT mp_url, mp_status FROM per_run_mps
WHERE run_id = (
    SELECT run_id FROM per_run_mps ORDER BY finish_time DESC LIMIT 1)
""",
        codebase,
        campaign,
    )
    return rows


@routes.get("/rate-limits", name="rate-limits")
async def rate_limits_request(request):
    bucket_rate_limiter = request.app["bucket_rate_limiter"]

    per_bucket = {}
    for bucket, (current_open, max_open) in bucket_rate_limiter.get_stats().items():
        per_bucket[bucket] = {
            "open": current_open,
            "max_open": max_open,
            "remaining": (
                None
                if (current_open is None or max_open is None)
                else max_open - current_open
            ),
        }

    return web.json_response(
        {
            "proposals_per_bucket": per_bucket,
            "per_forge": {
                str(f): dt.isoformat()
                for f, dt in request.app["forge_rate_limiter"].items()
            },
            "push_limit": request.app["push_limit"],
        }
    )


@routes.get("/blockers/{run_id}", name="blockers")
async def blockers_request(request):
    span = aiozipkin.request_span(request)
    async with request.app["db"].acquire() as conn:
        with span.new_child("sql:publish-status"):
            run = await conn.fetchrow(
                """\
SELECT
  run.id AS id,
  run.codebase AS codebase,
  run.suite AS campaign,
  run.finish_time AS finish_time,
  run.command AS run_command,
  run.publish_status AS publish_status,
  named_publish_policy.rate_limit_bucket AS rate_limit_bucket,
  run.revision AS revision,
  candidate.command AS policy_command,
  run.result_code AS result_code,
  change_set.state AS change_set_state,
  change_set.id AS change_set,
  codebase.inactive AS inactive
FROM run
INNER JOIN codebase ON codebase.name = run.codebase
INNER JOIN candidate ON candidate.codebase = run.codebase AND candidate.suite = run.suite
INNER JOIN named_publish_policy ON candidate.publish_policy = named_publish_policy.name
INNER JOIN change_set ON change_set.id = run.change_set
WHERE run.id = $1
""",
                request.match_info["run_id"],
            )

        if run is None:
            return web.json_response(
                {
                    "reason": "No such publish-ready run",
                    "run_id": request.match_info["run_id"],
                },
                status=404,
            )

        with span.new_child("sql:reviews"):
            reviews = await conn.fetch(
                "SELECT * FROM review WHERE run_id = $1", run["id"]
            )

        if run["revision"] is not None:
            with span.new_child("sql:publish-attempt-count"):
                attempt_count = await get_publish_attempt_count(
                    conn, run["revision"].encode("utf-8"), {"differ-unreachable"}
                )
        else:
            attempt_count = 0

        with span.new_child("sql:last-mp"):
            last_mps: list[tuple[str, str]] = await get_previous_mp_status(
                conn, run["codebase"], run["campaign"]
            )
    ret = {}
    ret["success"] = {
        "result": (run["result_code"] == "success"),
        "details": {"result_code": run["result_code"]},
    }
    ret["inactive"] = {
        "result": not run["inactive"],
        "details": {"inactive": run["inactive"]},
    }
    ret["command"] = {
        "result": run["run_command"] == run["policy_command"],
        "details": {"correct": run["policy_command"], "actual": run["run_command"]},
    }
    ret["publish_status"] = {
        "result": (run["publish_status"] == "approved"),
        "details": {
            "status": run["publish_status"],
            "reviews": {
                review["reviewer"]: {
                    "timestamp": review["reviewed_at"].isoformat(),
                    "comment": review["comment"],
                    "verdict": review["verdict"],
                }
                for review in reviews
            },
        },
    }

    next_try_time = calculate_next_try_time(run["finish_time"], attempt_count)
    ret["backoff"] = {
        "result": datetime.utcnow() >= next_try_time,
        "details": {
            "attempt_count": attempt_count,
            "next_try_time": next_try_time.isoformat(),
        },
    }

    # TODO(jelmer): include forge rate limits?

    ret["propose_rate_limit"] = {"details": {"bucket": run["rate_limit_bucket"]}}
    try:
        request.app["bucket_rate_limiter"].check_allowed(run["rate_limit_bucket"])
    except BucketRateLimited as e:
        ret["propose_rate_limit"]["result"] = False
        ret["propose_rate_limit"]["details"] = {
            "open": e.open_mps,
            "max_open": e.max_open_mps,
        }
    except RateLimited:
        ret["propose_rate_limit"]["result"] = False
    else:
        ret["propose_rate_limit"]["result"] = True

    ret["change_set"] = {
        "result": (run["change_set_state"] in ("publishing", "ready")),
        "details": {
            "change_set_id": run["change_set"],
            "change_set_state": run["change_set_state"],
        },
    }

    ret["previous_mp"] = {
        "result": all(last_mp[1] not in ("rejected", "closed") for last_mp in last_mps),
        "details": [{"url": last_mp[0], "status": last_mp[1]} for last_mp in last_mps],
    }

    return web.json_response(ret)


async def process_queue_loop(
    *,
    db,
    redis,
    config,
    publish_worker,
    bucket_rate_limiter,
    forge_rate_limiter,
    vcs_managers,
    interval,
    auto_publish: bool = True,
    push_limit: Optional[int] = None,
    modify_mp_limit: Optional[int] = None,
    require_binary_diff: bool = False,
):
    while True:
        cycle_start = datetime.utcnow()
        async with db.acquire() as conn:
            await check_existing(
                conn=conn,
                redis=redis,
                config=config,
                publish_worker=publish_worker,
                bucket_rate_limiter=bucket_rate_limiter,
                forge_rate_limiter=forge_rate_limiter,
                vcs_managers=vcs_managers,
                modify_limit=modify_mp_limit,
            )
            await check_stragglers(conn, redis)
        if auto_publish:
            await publish_pending_ready(
                db=db,
                redis=redis,
                config=config,
                publish_worker=publish_worker,
                bucket_rate_limiter=bucket_rate_limiter,
                vcs_managers=vcs_managers,
                push_limit=push_limit,
                require_binary_diff=require_binary_diff,
            )
        cycle_duration = datetime.utcnow() - cycle_start
        to_wait = max(0, interval - cycle_duration.total_seconds())
        logger.info("Waiting %d seconds for next cycle.", to_wait)
        if to_wait > 0:
            await asyncio.sleep(to_wait)


class NoRunForMergeProposal(Exception):
    """No run matching merge proposal."""

    def __init__(self, mp, revision) -> None:
        self.mp = mp
        self.revision = revision


async def get_last_effective_run(conn, codebase, campaign):
    query = """
SELECT
    id, command, start_time, finish_time, description,
    result_code,
    value, main_branch_revision, revision, context, result, suite,
    instigated_context, vcs_type, branch_url, logfilenames,
    worker,
    array(SELECT row(role, remote_name, base_revision,
     revision) FROM new_result_branch WHERE run_id = id) AS result_branches,
    result_tags, target_branch_url, change_set AS change_set,
    failure_transient, failure_stage, codebase
FROM
    last_effective_runs
WHERE codebase = $1 AND suite = $2
LIMIT 1
"""
    row = await conn.fetchrow(query, codebase, campaign)
    if row is None:
        return None
    return state.Run.from_row(row)


async def get_merge_proposal_run(
    conn: asyncpg.Connection, mp_url: str
) -> asyncpg.Record:
    query = """
SELECT
    run.id AS id,
    run.suite AS campaign,
    run.branch_url AS branch_url,
    run.command AS command,
    run.value AS value,
    rb.role AS role,
    rb.remote_name AS remote_branch_name,
    rb.revision AS revision,
    run.codebase AS codebase,
    run.change_set AS change_set
FROM new_result_branch rb
RIGHT JOIN run ON rb.run_id = run.id
WHERE rb.revision IN (
    SELECT revision from merge_proposal WHERE merge_proposal.url = $1)
ORDER BY run.finish_time DESC
LIMIT 1
"""
    return await conn.fetchrow(query, mp_url)


@dataclass
class ProposalInfo:
    can_be_merged: Optional[bool]
    status: str
    revision: Optional[bytes]
    target_branch_url: Optional[str]
    rate_limit_bucket: Optional[str] = None
    codebase: Optional[str] = None


async def guess_proposal_info_from_revision(
    conn: asyncpg.Connection, revision: bytes
) -> tuple[Optional[str], Optional[str]]:
    query = """\
SELECT DISTINCT run.codebase, named_publish_policy.rate_limit_bucket AS rate_limit_bucket
FROM run
LEFT JOIN new_result_branch rb ON rb.run_id = run.id
INNER JOIN candidate ON run.codebase = candidate.codebase AND run.suite = candidate.suite
INNER JOIN named_publish_policy ON named_publish_policy.name = candidate.publish_policy
WHERE rb.revision = $1 AND run.codebase is not null
"""
    rows = await conn.fetch(query, revision.decode("utf-8"))
    if len(rows) == 1:
        return rows[0][0], rows[0][1]
    return None, None


async def guess_rate_limit_bucket(
    conn: asyncpg.Connection, codebase: str, source_branch_name: str
):
    # For now, just assume that source_branch_name is campaign
    campaign = source_branch_name.split("/")[0]
    query = """\
SELECT named_publish_policy.rate_limit_bucket FROM candidate
INNER JOIN named_publish_policy ON named_publish_policy.name = candidate.publish_policy
WHERE candidate.suite = $1 AND candidate.codebase = $2
"""
    return await conn.fetchval(query, campaign, codebase)


async def guess_codebase_from_branch_url(
    conn: asyncpg.Connection,
    url: str,
    possible_transports: Optional[list[Transport]] = None,
):
    # TODO(jelmer): use codebase table
    query = """
SELECT
  name, branch_url
FROM
  codebase
WHERE
  TRIM(trailing '/' from branch_url) = ANY($1::text[])
ORDER BY length(branch_url) DESC
"""
    repo_url, params = urlutils.split_segment_parameters(url.rstrip("/"))
    try:
        branch = urlutils.unescape(params["branch"])
    except KeyError:
        branch = None
    options = [
        url.rstrip("/"),
        repo_url.rstrip("/"),
    ]
    result = await conn.fetchrow(query, options)
    if result is None:
        return None

    if url.rstrip("/") == result["branch_url"].rstrip("/"):
        return result["codebase"]

    source_branch = await asyncio.to_thread(
        open_branch,
        result["branch_url"].rstrip("/"),
        possible_transports=possible_transports,
    )
    if (
        source_branch.controldir.user_url.rstrip("/") != url.rstrip("/")
        and source_branch.name != branch
    ):
        logger.info(
            "Did not resolve branch URL to codebase: %r (%r) != %r (%r)",
            source_branch.user_url,
            source_branch.name,
            url,
            branch,
        )
        return None
    return result["codebase"]


def find_campaign_by_branch_name(config, branch_name):
    for campaign in config.campaign:
        if campaign.branch_name == branch_name:
            return campaign.name, "main"
    return None, None


class ProposalInfoManager:
    def __init__(self, conn: asyncpg.Connection, redis) -> None:
        self.conn = conn
        self.redis = redis

    async def iter_outdated_proposal_info_urls(self, days):
        return [
            row["url"]
            for row in await self.conn.fetch(
                f"SELECT url FROM merge_proposal WHERE last_scanned is NULL OR now() - last_scanned > interval '{days} days'"
            )
        ]

    async def get_proposal_info(self, url) -> Optional[ProposalInfo]:
        row = await self.conn.fetchrow(
            """\
    SELECT
        merge_proposal.rate_limit_bucket AS rate_limit_bucket,
        merge_proposal.revision,
        merge_proposal.status,
        merge_proposal.target_branch_url,
        merge_proposal.codebase,
        can_be_merged
    FROM
        merge_proposal
    WHERE
        merge_proposal.url = $1
    """,
            url,
        )
        if not row:
            return None
        return ProposalInfo(
            rate_limit_bucket=row["rate_limit_bucket"],
            revision=cast(bytes, row["revision"].encode("utf-8")) if row[1] else None,
            status=row["status"],
            target_branch_url=row["target_branch_url"],
            can_be_merged=row["can_be_merged"],
            codebase=row["codebase"],
        )

    async def delete_proposal_info(self, url):
        await self.conn.execute("DELETE FROM merge_proposal WHERE url = $1", url)

    async def update_canonical_url(self, old_url: str, canonical_url: str):
        async with self.conn.transaction():
            old_url = await self.conn.fetchval(
                "UPDATE merge_proposal canonical SET codebase = COALESCE(canonical.codebase, old.codebase), "
                "rate_limit_bucket = COALESCE(canonical.rate_limit_bucket, old.rate_limit_bucket) "
                "FROM merge_proposal old WHERE old.url = $1 AND canonical.url = $2 RETURNING old.url",
                str(old_url),
                str(canonical_url),
            )
            await self.conn.execute(
                "UPDATE publish SET merge_proposal_url = $1 WHERE merge_proposal_url = $2",
                str(canonical_url),
                str(old_url),
            )
            if old_url:
                await self.conn.execute(
                    "DELETE FROM merge_proposal WHERE url = $1", str(old_url)
                )
            else:
                await self.conn.execute(
                    "UPDATE merge_proposal SET url = $1 WHERE url = $2",
                    str(canonical_url),
                    str(old_url),
                )

    async def update_proposal_info(
        self,
        mp,
        *,
        status,
        revision,
        codebase,
        target_branch_url,
        campaign,
        can_be_merged: Optional[bool],
        rate_limit_bucket: Optional[str],
    ):
        if status == "closed":
            # TODO(jelmer): Check if changes were applied manually and mark
            # as applied rather than closed?
            pass
        if status == "merged":
            merged_by = await asyncio.to_thread(mp.get_merged_by)
            merged_by_url = await asyncio.to_thread(
                get_merged_by_user_url, mp.url, merged_by
            )
            merged_at = await asyncio.to_thread(mp.get_merged_at)
            if merged_at is not None:
                merged_at = merged_at.replace(tzinfo=None)
        else:
            merged_by = None
            merged_by_url = None
            merged_at = None
        async with self.conn.transaction():
            await self.conn.execute(
                """INSERT INTO merge_proposal (
                    url, status, revision, merged_by, merged_at,
                    target_branch_url, last_scanned, can_be_merged, rate_limit_bucket,
                    codebase)
                VALUES ($1, $2, $3, $4, $5, $6, NOW(), $7, $8, $9)
                ON CONFLICT (url)
                DO UPDATE SET
                  status = EXCLUDED.status,
                  revision = EXCLUDED.revision,
                  merged_by = EXCLUDED.merged_by,
                  merged_at = EXCLUDED.merged_at,
                  target_branch_url = EXCLUDED.target_branch_url,
                  last_scanned = EXCLUDED.last_scanned,
                  can_be_merged = EXCLUDED.can_be_merged,
                  rate_limit_bucket = EXCLUDED.rate_limit_bucket,
                  codebase = EXCLUDED.codebase
                """,
                mp.url,
                status,
                revision.decode("utf-8") if revision is not None else None,
                merged_by,
                merged_at,
                target_branch_url,
                can_be_merged,
                rate_limit_bucket,
                codebase,
            )
            if revision:
                await self.conn.execute(
                    """
                UPDATE new_result_branch SET absorbed = $1 WHERE revision = $2
                """,
                    (status == "merged"),
                    revision.decode("utf-8"),
                )

        # TODO(jelmer): Check if the change_set should be marked as published

        await self.redis.publish(
            "merge-proposal",
            json.dumps(
                {
                    "url": mp.url,
                    "target_branch_url": target_branch_url,
                    "rate_limit_bucket": rate_limit_bucket,
                    "status": status,
                    "codebase": codebase,
                    "merged_by": merged_by,
                    "merged_by_url": merged_by_url,
                    "merged_at": str(merged_at),
                    "campaign": campaign,
                }
            ),
        )


async def abandon_mp(
    proposal_info_manager: ProposalInfoManager,
    mp: MergeProposal,
    revision: bytes,
    codebase: Optional[str],
    target_branch_url: str,
    campaign: Optional[str],
    can_be_merged: Optional[bool],
    rate_limit_bucket: Optional[str],
    comment: Optional[str],
):
    if comment:
        logger.info("%s: %s", mp.url, comment)
    await proposal_info_manager.update_proposal_info(
        mp,
        status="abandoned",
        revision=revision,
        target_branch_url=target_branch_url,
        campaign=campaign,
        codebase=codebase,
        rate_limit_bucket=rate_limit_bucket,
        can_be_merged=can_be_merged,
    )
    if comment:
        try:
            await asyncio.to_thread(mp.post_comment, comment)
        except PermissionDenied as e:
            logger.warning("Permission denied posting comment to %s: %s", mp.url, e)

    try:
        await asyncio.to_thread(mp.close)
    except PermissionDenied as e:
        logger.warning("Permission denied closing merge request %s: %s", mp.url, e)
        raise


async def close_applied_mp(
    proposal_info_manager,
    mp: MergeProposal,
    revision: bytes,
    codebase: Optional[str],
    target_branch_url: str,
    campaign: Optional[str],
    can_be_merged: Optional[bool],
    rate_limit_bucket: Optional[str],
    comment: Optional[str],
):
    await proposal_info_manager.update_proposal_info(
        mp,
        status="applied",
        revision=revision,
        codebase=codebase,
        target_branch_url=target_branch_url,
        campaign=campaign,
        can_be_merged=can_be_merged,
        rate_limit_bucket=rate_limit_bucket,
    )
    try:
        await asyncio.to_thread(mp.post_comment, comment)
    except PermissionDenied as e:
        logger.warning("Permission denied posting comment to %s: %s", mp.url, e)

    try:
        await asyncio.to_thread(mp.close)
    except PermissionDenied as e:
        logger.warning("Permission denied closing merge request %s: %s", mp.url, e)
        raise


async def check_stragglers(conn, redis):
    proposal_info_manager = ProposalInfoManager(conn, redis)
    stragglers = await proposal_info_manager.iter_outdated_proposal_info_urls(5)
    for url in stragglers:
        await check_straggler(proposal_info_manager, url)


async def check_straggler(proposal_info_manager, url):
    # Find the canonical URL
    async with ClientSession() as session:
        async with session.get(url) as resp:
            if resp.status == 200 and resp.url != url:
                await proposal_info_manager.update_canonical_url(url, resp.url)
            if resp.status == 404:
                # TODO(jelmer): Keep it but leave a tumbestone around?
                await proposal_info_manager.delete_proposal_info(url)
            else:
                logger.warning("Got status %d loading straggler %r", resp.status, url)


async def check_existing_mp(
    conn,
    redis,
    config,
    publish_worker,
    mp,
    status,
    vcs_managers,
    bucket_rate_limiter,
    mps_per_bucket=None,
    possible_transports: Optional[list[Transport]] = None,
    check_only: bool = False,
    close_below_threshold: bool = True,
) -> bool:
    proposal_info_manager = ProposalInfoManager(conn, redis)
    old_proposal_info = await proposal_info_manager.get_proposal_info(mp.url)
    if old_proposal_info:
        codebase = old_proposal_info.codebase
        rate_limit_bucket = old_proposal_info.rate_limit_bucket
    else:
        codebase = None
        rate_limit_bucket = None
    revision = await asyncio.to_thread(mp.get_source_revision)
    source_branch_url = await asyncio.to_thread(mp.get_source_branch_url)
    try:
        can_be_merged = await asyncio.to_thread(mp.can_be_merged)
    except NotImplementedError:
        # TODO(jelmer): Download and attempt to merge locally?
        can_be_merged = None

    if revision is None:
        if source_branch_url is None:
            logger.warning("No source branch for %r", mp, extra={"mp_url": mp.url})
            revision = None
            source_branch_name = None
        else:
            try:
                source_branch = await asyncio.to_thread(
                    open_branch,
                    source_branch_url,
                    possible_transports=possible_transports,
                )
            except (BranchMissing, BranchUnavailable):
                revision = None
                source_branch_name = None
            else:
                revision = await asyncio.to_thread(source_branch.last_revision)
                source_branch_name = source_branch.name
    else:
        source_branch_name = None
    if source_branch_name is None and source_branch_url is not None:
        segment_params = urlutils.split_segment_parameters(source_branch_url)[1]
        source_branch_name = segment_params.get("branch")
        if source_branch_name is not None:
            source_branch_name = urlutils.unescape(source_branch_name)
    if revision is None and old_proposal_info:
        revision = old_proposal_info.revision
    target_branch_url = await asyncio.to_thread(mp.get_target_branch_url)
    if rate_limit_bucket is None:
        codebase = await guess_codebase_from_branch_url(
            conn, target_branch_url, possible_transports=possible_transports
        )
        if codebase is None:
            if revision is not None:
                (
                    codebase,
                    rate_limit_bucket,
                ) = await guess_proposal_info_from_revision(conn, revision)
            if codebase is None:
                logger.warning(
                    "No codebase known for %s (%s)",
                    mp.url,
                    target_branch_url,
                    extra={"mp_url": mp.url},
                )
            else:
                logger.info(
                    "Guessed codebase name (%s) for %s based on revision.",
                    codebase,
                    mp.url,
                    extra={"mp_url": mp.url},
                )
        else:
            if source_branch_name is not None:
                rate_limit_bucket = await guess_rate_limit_bucket(
                    conn, codebase, source_branch_name
                )
    if (
        old_proposal_info
        and old_proposal_info.status in ("abandoned", "applied", "rejected")
        and status == "closed"
    ):
        status = old_proposal_info.status

    if old_proposal_info is None or (
        old_proposal_info.status != status
        or revision != old_proposal_info.revision
        or target_branch_url != old_proposal_info.target_branch_url
        or rate_limit_bucket != old_proposal_info.rate_limit_bucket
        or can_be_merged != old_proposal_info.can_be_merged
    ):
        mp_run = await get_merge_proposal_run(conn, mp.url)
        await proposal_info_manager.update_proposal_info(
            mp,
            status=status,
            revision=revision,
            codebase=codebase,
            target_branch_url=target_branch_url,
            campaign=mp_run["campaign"] if mp_run else None,
            can_be_merged=can_be_merged,
            rate_limit_bucket=rate_limit_bucket,
        )
    else:
        await conn.execute(
            "UPDATE merge_proposal SET last_scanned = NOW() WHERE url = $1", mp.url
        )
        mp_run = None
    if rate_limit_bucket is not None and mps_per_bucket is not None:
        mps_per_bucket[status].setdefault(rate_limit_bucket, 0)
        mps_per_bucket[status][rate_limit_bucket] += 1
    if status != "open":
        return False
    if check_only:
        return False

    if mp_run is None:
        mp_run = await get_merge_proposal_run(conn, mp.url)

    if mp_run is None:
        # If we don't have any information about this merge proposal, then
        # it might be one that we lost the data for. Let's reschedule.
        if codebase and source_branch_name:
            campaign, role = find_campaign_by_branch_name(config, source_branch_name)
            if campaign:
                logger.warning(
                    "Recovered orphaned merge proposal %s",
                    mp.url,
                    extra={"mp_url": mp.url},
                )
                last_run = await get_last_effective_run(conn, codebase, campaign)
                if last_run is None:
                    try:
                        await do_schedule(
                            conn,
                            campaign=campaign,
                            change_set=None,
                            bucket="update-existing-mp",
                            refresh=True,
                            requester="publisher (orphaned merge proposal)",
                            codebase=codebase,
                        )
                    except CandidateUnavailable as e:
                        logger.warning(
                            "Candidate unavailable while attempting to reschedule "
                            "orphaned %s: %s/%s",
                            mp.url,
                            codebase,
                            campaign,
                            extra={"mp_url": mp.url},
                        )
                        raise NoRunForMergeProposal(mp, revision) from e
                    else:
                        logger.warning("Rescheduled", extra={"mp_url": mp.url})
                        return False
                else:
                    mp_run = {
                        "remote_branch_name": None,
                        "campaign": campaign,
                        "change_set": None,
                        "codebase": codebase,
                        "role": role,
                        "id": None,
                        "branch_url": target_branch_url,
                        "revision": revision.decode("utf-8"),
                        "value": None,
                    }
                    logger.warning(
                        "Going ahead with dummy old run", extra={"mp_url": mp.url}
                    )
            else:
                raise NoRunForMergeProposal(mp, revision)
        else:
            raise NoRunForMergeProposal(mp, revision)

    mp_remote_branch_name = mp_run["remote_branch_name"]

    if mp_remote_branch_name is None:
        if target_branch_url is None:
            logger.warning("No target branch for %r", mp, extra={"mp_url": mp.url})
        else:
            try:
                mp_remote_branch_name = (
                    await asyncio.to_thread(
                        open_branch,
                        target_branch_url,
                        possible_transports=possible_transports,
                    )
                ).name
            except (BranchMissing, BranchUnavailable):
                pass

    last_run = await get_last_effective_run(
        conn, mp_run["codebase"], mp_run["campaign"]
    )
    if last_run is None:
        logger.warning(
            "%s: Unable to find any relevant runs.", mp.url, extra={"mp_url": mp.url}
        )
        return False

    if last_run.result_code == "nothing-to-do":
        # A new run happened since the last, but there was nothing to
        # do.
        logger.info(
            "%s: Last run did not produce any changes, closing proposal.",
            mp.url,
            extra={"mp_url": mp.url},
        )

        try:
            await close_applied_mp(
                proposal_info_manager,
                mp,
                revision,
                codebase,
                target_branch_url,
                mp_run["campaign"],
                can_be_merged=can_be_merged,
                rate_limit_bucket=rate_limit_bucket,
                comment="""
This merge proposal will be closed, since all remaining changes have been \
applied independently.
""",
            )
        except PermissionDenied:
            return False
        else:
            return True

    if last_run.result_code != "success":
        last_run_age = datetime.utcnow() - last_run.finish_time
        if last_run.failure_transient:
            logger.info(
                "%s: Last run failed with transient error (%s). Rescheduling.",
                mp.url,
                last_run.result_code,
                extra={"mp_url": mp.url},
            )
            try:
                await do_schedule(
                    conn,
                    campaign=last_run.campaign,
                    change_set=last_run.change_set,
                    bucket="update-existing-mp",
                    refresh=False,
                    requester="publisher (transient error)",
                    codebase=last_run.codebase,
                )
            except CandidateUnavailable as e:
                logger.warning(
                    "Candidate unavailable while attempting to reschedule %s/%s: %s",
                    last_run.codebase,
                    last_run.campaign,
                    e,
                    extra={"mp_url": mp.url},
                )
        elif last_run_age.days > EXISTING_RUN_RETRY_INTERVAL:
            logger.info(
                "%s: Last run failed (%s) a long time ago (%d days). Rescheduling.",
                mp.url,
                last_run.result_code,
                last_run_age.days,
            )
            try:
                await do_schedule(
                    conn,
                    campaign=last_run.campaign,
                    change_set=last_run.change_set,
                    bucket="update-existing-mp",
                    refresh=False,
                    requester=f"publisher (retrying failed run after {last_run_age.days} days)",
                    codebase=last_run.codebase,
                )
            except CandidateUnavailable as e:
                logger.warning(
                    "Candidate unavailable while attempting to reschedule %s/%s: %s",
                    last_run.codebase,
                    last_run.campaign,
                    e,
                    extra={"mp_url": mp.url},
                )
        else:
            logger.info(
                "%s: Last run failed (%s). Not touching merge proposal.",
                mp.url,
                last_run.result_code,
                extra={"mp_url": mp.url},
            )
        return False

    campaign_config = get_campaign_config(config, mp_run["campaign"])

    if close_below_threshold and not run_sufficient_for_proposal(
        campaign_config, mp_run["value"]
    ):
        try:
            await abandon_mp(
                proposal_info_manager,
                mp,
                revision,
                codebase,
                target_branch_url,
                campaign=mp_run["campaign"],
                can_be_merged=can_be_merged,
                rate_limit_bucket=rate_limit_bucket,
                comment="This merge proposal will be closed, since only trivial changes are left.",
            )
        except PermissionDenied:
            return False
        return True

    try:
        (
            last_run_remote_branch_name,
            last_run_base_revision,
            last_run_revision,
        ) = last_run.get_result_branch(mp_run["role"])
    except KeyError:
        logger.warning(
            "%s: Merge proposal run %s had role %s but it is gone now (%s)",
            mp.url,
            mp_run["id"],
            mp_run["role"],
            last_run.id,
            extra={"mp_url": mp.url},
        )
        return False

    if (
        last_run_remote_branch_name != mp_remote_branch_name
        and last_run_remote_branch_name is not None
    ):
        logger.warning(
            "%s: Remote branch name has changed: %s ⇒ %s ",
            mp.url,
            mp_remote_branch_name,
            last_run_remote_branch_name,
            extra={"mp_url": mp.url},
        )
        # Note that we require that mp_remote_branch_name is set.
        # For some old runs it is not set because we didn't track
        # the default branch name.
        if mp_remote_branch_name is not None:
            try:
                await asyncio.to_thread(
                    mp.set_target_branch_name, last_run_remote_branch_name or ""
                )
            except NotImplementedError:
                logger.info(
                    "%s: Closing merge proposal, since branch for role "
                    "'%s' has changed from %s to %s.",
                    mp.url,
                    mp_run["role"],
                    mp_remote_branch_name,
                    last_run_remote_branch_name,
                    extra={"mp_url": mp.url},
                )
                try:
                    await abandon_mp(
                        proposal_info_manager,
                        mp,
                        revision,
                        codebase,
                        target_branch_url,
                        rate_limit_bucket=rate_limit_bucket,
                        campaign=mp_run["campaign"],
                        can_be_merged=can_be_merged,
                        comment="""\
This merge proposal will be closed, since the branch for the role '{}'
has changed from {} to {}.
""".format(mp_run["role"], mp_remote_branch_name, last_run_remote_branch_name),
                    )
                except PermissionDenied:
                    return False
                return True
            else:
                target_branch_url = role_branch_url(
                    mp_run["branch_url"], mp_remote_branch_name
                )
        else:
            return False

    if not await asyncio.to_thread(
        branches_match, mp_run["branch_url"], last_run.branch_url
    ):
        logger.warning(
            "%s: Remote branch URL appears to have have changed: %s ⇒ %s, skipping.",
            mp.url,
            mp_run["branch_url"],
            last_run.branch_url,
            extra={"mp_url": mp.url},
        )
        return False

        # TODO(jelmer): Don't do this if there's a redirect in place,
        # or if one of the branches has a branch name included and the other
        # doesn't
        try:
            await abandon_mp(
                proposal_info_manager,
                mp,
                revision,
                codebase,
                target_branch_url,
                campaign=mp_run["campaign"],
                can_be_merged=can_be_merged,
                rate_limit_bucket=rate_limit_bucket,
                comment=f"""\
This merge proposal will be closed, since the branch has moved to {last_run.branch_url}.
""",
            )
        except PermissionDenied:
            return False
        return True

    if last_run.id != mp_run["id"]:
        publish_id = str(uuid.uuid4())
        logger.info(
            "%s (%s) needs to be updated (%s ⇒ %s).",
            mp.url,
            mp_run["codebase"],
            mp_run["id"],
            last_run.id,
            extra={"mp_url": mp.url},
        )
        if last_run_revision == mp_run["revision"].encode("utf-8"):
            logger.warning(
                "%s (%s): old run (%s/%s) has same revision as new run (%s/%s): %r",
                mp.url,
                mp_run["codebase"],
                mp_run["id"],
                mp_run["role"],
                last_run.id,
                mp_run["role"],
                mp_run["revision"].encode("utf-8"),
                extra={"mp_url": mp.url},
            )
            # In some cases this can happen when we kick off two runs at
            # exactly the same time.
            return False

        if source_branch_name is None:
            source_branch_name = await derived_branch_name(
                conn, campaign_config, last_run, mp_run["role"]
            )

        unchanged_run_id = await conn.fetchval(
            "SELECT id FROM run "
            "WHERE revision = $2 AND codebase = $1 and result_code = 'success' "
            "ORDER BY finish_time DESC LIMIT 1",
            last_run.codebase,
            last_run.main_branch_revision.decode("utf-8"),
        )

        try:
            publish_result = await publish_worker.publish_one(
                campaign=last_run.campaign,
                codebase=last_run.codebase,
                extra_context={},
                command=last_run.command,
                codemod_result=last_run.result,
                target_branch_url=target_branch_url,
                mode=MODE_PROPOSE,
                role=mp_run["role"],
                revision=last_run_revision,
                log_id=last_run.id,
                unchanged_id=unchanged_run_id,
                derived_branch_name=source_branch_name,
                rate_limit_bucket=rate_limit_bucket,
                vcs_manager=vcs_managers[last_run.vcs_type],
                require_binary_diff=False,
                allow_create_proposal=True,
                bucket_rate_limiter=bucket_rate_limiter,
                result_tags=last_run.result_tags,
                commit_message_template=(
                    campaign_config.merge_proposal.commit_message
                    if campaign_config.merge_proposal
                    else None
                ),
                title_template=(
                    campaign_config.merge_proposal.title
                    if campaign_config.merge_proposal
                    else None
                ),
                existing_mp_url=mp.url,
            )
        except BranchBusy as e:
            logger.info("%s: Branch %r was busy while publishing", mp.url, e.branch_url)
            return False
        except PublishFailure as e:
            code, description = await handle_publish_failure(
                e, conn, last_run, bucket="update-existing-mp"
            )
            if code == "empty-merge-proposal":
                # The changes from the merge proposal have already made it in
                # somehow.
                logger.info(
                    "%s: Empty merge proposal, changes must have been merged "
                    "some other way. Closing.",
                    mp.url,
                    extra={"mp_url": mp.url},
                )
                try:
                    await close_applied_mp(
                        proposal_info_manager,
                        mp,
                        revision,
                        codebase,
                        target_branch_url,
                        campaign=mp_run["campaign"],
                        can_be_merged=can_be_merged,
                        rate_limit_bucket=rate_limit_bucket,
                        comment="""
This merge proposal will be closed, since all remaining changes have been \
applied independently.
""",
                    )
                except PermissionDenied as f:
                    logger.warning(
                        "Permission denied closing merge request %s: %s",
                        mp.url,
                        f,
                        extra={"mp_url": mp.url},
                    )
                    code = "empty-failed-to-close"
                    description = f"Permission denied closing merge request: {f}"
                code = "success"
                description = (
                    "Closing merge request for which changes were applied independently"
                )
            if code != "success":
                logger.info(
                    "%s: Updating merge proposal failed: %s (%s)",
                    mp.url,
                    code,
                    description,
                    extra={"mp_url": mp.url},
                )
            await store_publish(
                conn,
                change_set=last_run.change_set,
                codebase=last_run.codebase,
                branch_name=campaign_config.branch_name,
                main_branch_revision=last_run_base_revision,
                revision=last_run_revision,
                role=mp_run["role"],
                mode=e.mode,
                result_code=code,
                description=description,
                merge_proposal_url=mp.url,
                target_branch_url=target_branch_url,
                target_branch_web_url=None,
                publish_id=publish_id,
                requester="publisher (regular refresh)",
                run_id=last_run.id,
            )
        else:
            await store_publish(
                conn,
                change_set=last_run.change_set,
                codebase=last_run.codebase,
                branch_name=publish_result.branch_name,
                main_branch_revision=last_run_base_revision,
                revision=last_run_revision,
                role=mp_run["role"],
                mode=MODE_PROPOSE,
                result_code="success",
                description=(publish_result.description or "Successfully updated"),
                merge_proposal_url=publish_result.proposal_url,
                target_branch_url=publish_result.target_branch_url,
                target_branch_web_url=publish_result.target_branch_web_url,
                publish_id=publish_id,
                requester="publisher (regular refresh)",
                run_id=last_run.id,
            )

            if publish_result.is_new:
                # This can happen when the default branch changes
                logger.warning(
                    "Intended to update proposal %r, but created %r",
                    mp.url,
                    publish_result.proposal_url,
                    extra={"mp_url": mp.url},
                )
        return True
    else:
        # It may take a while for the 'conflicted' bit on the proposal to
        # be refreshed, so only check it if we haven't made any other
        # changes.
        if can_be_merged is False:
            logger.info(
                "%s can not be merged (conflict?). Rescheduling.",
                mp.url,
                extra={"mp_url": mp.url},
            )
            try:
                await do_schedule(
                    conn,
                    campaign=mp_run["campaign"],
                    change_set=mp_run["change_set"],
                    bucket="update-existing-mp",
                    refresh=True,
                    requester="publisher (merge conflict)",
                    codebase=mp_run["codebase"],
                )
            except CandidateUnavailable:
                logger.warning(
                    "Candidate unavailable while attempting to reschedule "
                    "conflicted %s/%s",
                    mp_run["codebase"],
                    mp_run["campaign"],
                    extra={"mp_url": mp.url},
                )
        return False


def iter_all_mps(
    statuses: Optional[list[str]] = None,
) -> Iterator[tuple[Forge, MergeProposal, str]]:
    """Iterate over all existing merge proposals."""
    if statuses is None:
        statuses = ["open", "merged", "closed"]
    for instance in iter_forge_instances():
        for status in statuses:
            try:
                for mp in instance.iter_my_proposals(status=status):
                    yield instance, mp, status
            except ForgeLoginRequired:
                logger.info("Skipping %r, no credentials known.", instance)
            except UnexpectedHttpStatus as e:
                logger.warning(
                    "Got unexpected HTTP status %s, skipping %r", e, instance
                )
            except UnsupportedForge as e:
                logger.warning(
                    "Unsupported host instance, skipping %r: %s", instance, e
                )


async def check_existing(
    *,
    conn,
    redis,
    config,
    publish_worker,
    bucket_rate_limiter,
    forge_rate_limiter: dict[Forge, datetime],
    vcs_managers,
    modify_limit=None,
    unexpected_limit: int = 5,
):
    mps_per_bucket: dict[str, dict[str, int]] = {
        "open": {},
        "closed": {},
        "merged": {},
        "applied": {},
        "abandoned": {},
        "rejected": {},
    }
    possible_transports: list[Transport] = []
    status_count = {
        "open": 0,
        "closed": 0,
        "merged": 0,
        "applied": 0,
        "abandoned": 0,
        "rejected": 0,
    }

    modified_mps = 0
    unexpected = 0
    check_only = False
    was_forge_ratelimited = False

    for forge, mp, status in iter_all_mps():
        status_count[status] += 1
        if forge in forge_rate_limiter:
            if datetime.utcnow() < forge_rate_limiter[forge]:
                del forge_rate_limiter[forge]
            else:
                forge_rate_limited_count.labels(forge=str(forge)).inc()
                was_forge_ratelimited = True
                continue
        try:
            modified = await check_existing_mp(
                conn=conn,
                redis=redis,
                config=config,
                publish_worker=publish_worker,
                mp=mp,
                status=status,
                vcs_managers=vcs_managers,
                bucket_rate_limiter=bucket_rate_limiter,
                possible_transports=possible_transports,
                mps_per_bucket=mps_per_bucket,
                check_only=check_only,
            )
        except NoRunForMergeProposal as e:
            logger.warning("Unable to find metadata for %s, skipping.", e.mp.url)
            modified = False
        except ForgeLoginRequired as e:
            logger.warning("Login required for forge %s, skipping.", e)
            modified = False
        except BranchRateLimited as e:
            logger.warning(
                "Rate-limited accessing %s. Skipping %r for this cycle.", mp.url, forge
            )
            if e.retry_after is None:
                retry_after = timedelta(minutes=30)
            else:
                retry_after = timedelta(seconds=e.retry_after)
            forge_rate_limiter[forge] = datetime.utcnow() + retry_after
            continue
        except UnexpectedHttpStatus as e:
            logger.warning(
                "Got unexpected HTTP status %s, skipping %r",
                e,
                mp.url,
                extra={"mp_url": mp.url},
            )
            # TODO(jelmer): print traceback?
            unexpected += 1

        if unexpected > unexpected_limit:
            unexpected_http_response_count.inc()
            logger.warning(
                "Saw %d unexpected HTTP responses, over threshold of %d. "
                "Giving up for now.",
                unexpected,
                unexpected_limit,
            )
            return

        if modified:
            modified_mps += 1
            if modify_limit and modified_mps > modify_limit:
                logger.warning(
                    "Already modified %d merge proposals, waiting with the rest.",
                    modified_mps,
                )
                check_only = True

    logger.info("Successfully scanned existing merge proposals")
    last_scan_existing_success.set_to_current_time()

    if not was_forge_ratelimited:
        for status, count in status_count.items():
            merge_proposal_count.labels(status=status).set(count)

        bucket_rate_limiter.set_mps_per_bucket(mps_per_bucket)
        total = 0
        for bucket, count in mps_per_bucket["open"].items():
            total += count
            if bucket is not None:
                bucket_proposal_count.labels(bucket=bucket).set(count)
        open_proposal_count.set(total)
    else:
        logger.info(
            "Rate-Limited for forges %r. Not updating stats", forge_rate_limiter
        )


async def get_run(conn: asyncpg.Connection, run_id):
    query = """
SELECT
    id, command, start_time, finish_time, description,
    result_code,
    value, main_branch_revision, revision, context, result, suite,
    instigated_context, vcs_type, branch_url, logfilenames, 
    worker,
    array(SELECT row(role, remote_name, base_revision,
     revision) FROM new_result_branch WHERE run_id = id) AS result_branches,
    result_tags, target_branch_url, change_set as change_set,
    failure_transient AS failure_transient, failure_stage, codebase
FROM
    run
WHERE id = $1
"""
    row = await conn.fetchrow(query, run_id)
    if row:
        return state.Run.from_row(row)
    return None


async def iter_control_matching_runs(
    conn: asyncpg.Connection, main_branch_revision: bytes, codebase: str
):
    query = """
SELECT
  id,
  command,
  start_time,
  finish_time,
  description,
  result_code,
  main_branch_revision,
  vcs_type,
  revision,
  context,
  result,
  suite,
  instigated_context,
  branch_url,
  logfilenames,
  worker,
  array(SELECT row(role, remote_name, base_revision,
   revision) FROM new_result_branch WHERE run_id = id) AS result_branches,
  result_tags,
  target_branch_url,
  change_set,
  failure_transient,
  failure_stage,
  codebase,
  value
FROM last_runs
WHERE main_branch_revision = $1 AND codebase = $2 AND main_branch_revision != revision AND suite NOT in ('unchanged', 'control')
ORDER BY start_time DESC
"""
    return [
        state.Run.from_row(row)
        for row in await conn.fetch(
            query, main_branch_revision.decode("utf-8"), codebase
        )
    ]


async def listen_to_runner(
    *,
    db,
    redis,
    config,
    publish_worker,
    bucket_rate_limiter,
    vcs_managers,
    require_binary_diff: bool = False,
):
    async def process_run(conn, run, branch_url):
        publish_policy, command, rate_limit_bucket = await get_publish_policy(
            conn, run.codebase, run.campaign
        )
        if publish_policy is None:
            logging.warning(
                "No publish policy for %s/%s, skipping", run.codebase, run.campaign
            )
            return
        for role, (mode, max_frequency_days) in publish_policy.items():
            await publish_from_policy(
                conn=conn,
                campaign_config=get_campaign_config(config, run.campaign),
                publish_worker=publish_worker,
                bucket_rate_limiter=bucket_rate_limiter,
                vcs_managers=vcs_managers,
                run=run,
                redis=redis,
                role=role,
                rate_limit_bucket=rate_limit_bucket,
                target_branch_url=branch_url,
                mode=mode,
                max_frequency_days=max_frequency_days,
                command=command,
                require_binary_diff=require_binary_diff,
                force=True,
                requester="runner",
            )

    async def handle_publish_status_message(msg):
        result = json.loads(msg["data"])
        if result["publish_status"] != "approved":
            return
        async with db.acquire() as conn:
            # TODO(jelmer): Fold these into a single query ?
            codebase = await conn.fetchrow(
                "SELECT branch_url FROM codebase WHERE name = $1", result["codebase"]
            )
            if codebase is None:
                logger.warning("Codebase %s not in database?", result["codebase"])
                return
            run = await get_run(conn, result["run_id"])
            await process_run(conn, run, codebase["branch_url"])

    try:
        async with redis.pubsub(ignore_subscribe_messages=True) as ch:
            await ch.subscribe(
                "publish-status", **{"publish-status": handle_publish_status_message}
            )
            await ch.run()
    finally:
        await redis.close()


async def refresh_bucket_mp_counts(db, bucket_rate_limiter):
    per_bucket: dict[str, dict[str, int]] = {}
    async with db.acquire() as conn:
        for row in await conn.fetch(
            """
             SELECT
             rate_limit_bucket AS rate_limit_bucket,
             status AS status,
             count(*) as c
             FROM merge_proposal
             GROUP BY 1, 2
             """
        ):
            per_bucket.setdefault(row["status"], {})[row["rate_limit_bucket"]] = row[
                "c"
            ]
    bucket_rate_limiter.set_mps_per_bucket(per_bucket)


async def main_async(argv=None):
    import argparse

    parser = argparse.ArgumentParser(
        prog="janitor.publish", formatter_class=argparse.ArgumentDefaultsHelpFormatter
    )
    parser.add_argument("--port", type=int, help="Listen port", default=9912)
    parser.add_argument(
        "--listen-address", type=str, help="Listen address", default="localhost"
    )
    parser.add_argument(
        "--config",
        type=str,
        default="janitor.conf",
        help="Path to configuration",
    )
    parser.add_argument(
        "--prometheus", type=str, help="Prometheus push gateway to export to"
    )
    parser.add_argument(
        "--max-mps-per-bucket",
        default=0,
        type=int,
        help="Maximum number of open merge proposals per bucket",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="Just do one pass over the queue, don't run as a daemon",
    )
    parser.add_argument(
        "--interval",
        type=int,
        help=("Seconds to wait in between publishing pending proposals"),
        default=7200,
    )
    parser.add_argument(
        "--no-auto-publish",
        action="store_true",
        help="Do not create merge proposals automatically",
    )
    parser.add_argument(
        "--slowstart", action="store_true", help="Use slow start rate limiter"
    )
    parser.add_argument(
        "--push-limit", type=int, help="Limit number of pushes per cycle"
    )
    parser.add_argument(
        "--require-binary-diff",
        action="store_true",
        default=False,
        help="Require a binary diff when publishing merge requests",
    )
    parser.add_argument(
        "--modify-mp-limit",
        type=int,
        default=10,
        help="Maximum number of merge proposals to update per cycle",
    )
    parser.add_argument(
        "--differ-url",
        type=str,
        help="URL for differ",
        default="http://localhost:9920/",
    )
    parser.add_argument("--external-url", type=str, help="External URL", default=None)
    parser.add_argument(
        "--template-env-path", type=str, help="Path to merge proposal templates"
    )
    parser.add_argument(
        "--gcp-logging", action="store_true", help="Use Google cloud logging"
    )
    parser.add_argument("--debug", action="store_true", help="Show debug output")

    args = parser.parse_args()

    if args.gcp_logging:
        import google.cloud.logging

        client = google.cloud.logging.Client()
        client.get_default_handler()
        client.setup_logging()
    elif args.debug:
        logging.basicConfig(level=logging.DEBUG)
    else:
        logging.basicConfig(level=logging.INFO)

    loop = asyncio.get_running_loop()
    if args.debug:
        loop.set_debug(True)
        loop.slow_callback_duration = 0.001
        warnings.simplefilter("always", ResourceWarning)

    try:
        with open(args.config) as f:
            config = read_config(f)
    except FileNotFoundError:
        parser.error(f"config path {args.config} does not exist")

    set_user_agent(config.user_agent)

    bucket_rate_limiter: RateLimiter
    if args.slowstart:
        bucket_rate_limiter = SlowStartRateLimiter(args.max_mps_per_bucket)
    elif args.max_mps_per_bucket > 0:
        bucket_rate_limiter = FixedRateLimiter(args.max_mps_per_bucket)
    else:
        bucket_rate_limiter = NonRateLimiter()

    if args.no_auto_publish and args.once:
        sys.stderr.write("--no-auto-publish and --once are mutually exclude.")
        sys.exit(1)

    forge_rate_limiter: dict[Forge, datetime] = {}

    vcs_managers = get_vcs_managers_from_config(config)
    db = await state.create_pool(config.database_location)
    async with AsyncExitStack() as stack:
        redis = Redis.from_url(config.redis_location)
        stack.push_async_callback(redis.close)

        lock_manager = aioredlock.Aioredlock([config.redis_location])
        stack.push_async_callback(lock_manager.destroy)

        publish_worker = PublishWorker(
            template_env_path=args.template_env_path,
            external_url=args.external_url,
            differ_url=args.differ_url,
            lock_manager=lock_manager,
            redis=redis,
        )

        if args.once:
            await publish_pending_ready(
                db=db,
                redis=redis,
                config=config,
                publish_worker=publish_worker,
                bucket_rate_limiter=bucket_rate_limiter,
                vcs_managers=vcs_managers,
                require_binary_diff=args.require_binary_diff,
            )
            if args.prometheus:
                await push_to_gateway(
                    args.prometheus, job="janitor.publish", registry=REGISTRY
                )
        else:
            tasks = [
                loop.create_task(
                    process_queue_loop(
                        db=db,
                        redis=redis,
                        config=config,
                        publish_worker=publish_worker,
                        bucket_rate_limiter=bucket_rate_limiter,
                        forge_rate_limiter=forge_rate_limiter,
                        vcs_managers=vcs_managers,
                        interval=args.interval,
                        auto_publish=not args.no_auto_publish,
                        push_limit=args.push_limit,
                        modify_mp_limit=args.modify_mp_limit,
                        require_binary_diff=args.require_binary_diff,
                    )
                ),
                loop.create_task(
                    run_web_server(
                        args.listen_address,
                        args.port,
                        publish_worker=publish_worker,
                        bucket_rate_limiter=bucket_rate_limiter,
                        forge_rate_limiter=forge_rate_limiter,
                        vcs_managers=vcs_managers,
                        db=db,
                        redis=redis,
                        config=config,
                        require_binary_diff=args.require_binary_diff,
                        modify_mp_limit=args.modify_mp_limit,
                        push_limit=args.push_limit,
                    )
                ),
                loop.create_task(
                    refresh_bucket_mp_counts(db, bucket_rate_limiter),
                ),
            ]
            tasks.append(
                loop.create_task(
                    listen_to_runner(
                        db=db,
                        redis=redis,
                        config=config,
                        publish_worker=publish_worker,
                        bucket_rate_limiter=bucket_rate_limiter,
                        vcs_managers=vcs_managers,
                        require_binary_diff=args.require_binary_diff,
                    )
                )
            )
            await asyncio.gather(*tasks)


def main():
    import uvloop

    asyncio.set_event_loop_policy(uvloop.EventLoopPolicy())
    sys.exit(asyncio.run(main_async(sys.argv[1:])))


if __name__ == "__main__":
    main()
