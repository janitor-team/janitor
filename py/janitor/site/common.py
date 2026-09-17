#!/usr/bin/python3

from typing import Any, Optional

import aiohttp_jinja2
import asyncpg
from aiohttp import ClientConnectorError, ClientResponseError, web
from breezy.revision import NULL_REVISION

from janitor import splitout_env, state
from janitor.config import get_campaign_config
from janitor.queue import Queue
from janitor.site import (
    BuildDiffUnavailable,
    DebdiffRetrievalError,
    get_archive_diff,
    update_vars_from_request,
)


async def get_previous_runs(conn: asyncpg.Connection, codebase: str, suite: str):
    return await conn.fetch(
        """
SELECT
  id,
  start_time,
  finish_time,
  finish_time - start_time AS duration,
  description,
  codebase,
  result_code,
  failure_transient
FROM
  run
WHERE
  codebase = $1 AND suite = $2
ORDER BY start_time DESC
""",
        codebase,
        suite,
    )


async def get_candidate(conn: asyncpg.Connection, codebase: str, campaign: str):
    return await conn.fetchrow(
        "SELECT context, value, success_chance FROM candidate "
        "WHERE codebase = $1 AND suite = $2",
        codebase,
        campaign,
    )


async def iter_candidates(
    conn: asyncpg.Connection,
    codebases: Optional[list[str]] = None,
    campaign: Optional[str] = None,
):
    query = """
SELECT
  codebase AS codebase,
  suite AS suite,
  context AS context,
  value AS value,
  success_chance AS success_chance
FROM candidate
"""
    args = []
    if campaign is not None and codebases is not None:
        query += " WHERE codebase = ANY($1::text[]) AND suite = $2"
        args.extend([codebases, campaign])
    elif campaign is not None:
        query += " WHERE suite = $1"
        args.append(campaign)
    elif codebases is not None:
        query += " WHERE codebase = ANY($1::text[])"
        args.append(codebases)
    return await conn.fetch(query, *args)


async def get_last_unabsorbed_run(conn: asyncpg.Connection, codebase: str, suite: str):
    args = []
    query = """
SELECT
  id,
  command,
  start_time,
  finish_time,
  description,
  debian_build.version AS build_version,
  debian_build.distribution AS build_distribution,
  debian_build.lintian_result AS lintian_result,
  debian_build.binary_packages AS binary_packages,
  result_code,
  main_branch_revision,
  revision,
  context,
  result,
  suite,
  instigated_context,
  vcs_type,
  branch_url,
  logfilenames,
  last_unabsorbed_runs.worker AS worker_name,
  worker.link AS worker_link,
  array(SELECT row(role, remote_name, base_revision,
   revision) FROM new_result_branch WHERE run_id = id) AS result_branches,
  result_tags,
  change_set,
  failure_stage,
  failure_transient,
  codebase,
  publish_status
FROM
  last_unabsorbed_runs
LEFT JOIN worker ON worker.name = last_unabsorbed_runs.worker
LEFT JOIN debian_build ON last_unabsorbed_runs.id = debian_build.run_id
WHERE codebase = $1 AND suite = $2
ORDER BY codebase, suite DESC, start_time DESC
LIMIT 1
"""
    args = [codebase, suite]
    return await conn.fetchrow(query, *args)


async def get_run(conn: asyncpg.Connection, run_id):
    query = """
SELECT
    id, command, start_time, finish_time, description, codebase,
    debian_build.version AS build_version,
    debian_build.distribution AS build_distribution,
    debian_build.lintian_result AS lintian_result,
    debian_build.binary_packages AS binary_packages,
    result_code,
    main_branch_revision, revision, context, result, suite,
    instigated_context, vcs_type, branch_url, logfilenames,
    run.worker AS worker_name,
    worker.link AS worker_link,
    array(SELECT row(role, remote_name, base_revision,
     revision) FROM new_result_branch WHERE run_id = id) AS result_branches,
    result_tags,
    resume_from,
    change_set,
    failure_stage,
    failure_transient,
    codebase,
    publish_status
FROM
    run
LEFT JOIN worker ON worker.name = run.worker
LEFT JOIN
    debian_build ON debian_build.run_id = run.id
WHERE id = $1
"""
    return await conn.fetchrow(query, run_id)


async def get_unchanged_run(
    conn: asyncpg.Connection, codebase: str, main_branch_revision: bytes
):
    query = """
SELECT
    id, command, start_time, finish_time, description,
    debian_build.version AS build_version,
    debian_build.distribution AS build_distribution, result_code, value,
    main_branch_revision, revision, context, result, suite,
    instigated_context, vcs_type, branch_url, logfilenames,
    worker,
    array(SELECT row(role, remote_name, base_revision, revision) FROM
     new_result_branch WHERE run_id = id) AS result_branches,
    result_tags, target_branch_url, change_set AS change_set,
    failure_transient AS failure_transient, failure_stage, publish_status
FROM
    last_runs
LEFT JOIN
    debian_build ON debian_build.run_id = last_runs.id
WHERE
    suite in ('control', 'unchanged') AND revision = $1 AND
    codebase = $2 AND
    result_code = 'success' AND
    change_set = id
ORDER BY finish_time DESC
"""
    return await conn.fetchrow(query, main_branch_revision.decode("utf-8"), codebase)


async def generate_codebase_context(
    db,
    config,
    suite,
    client,
    differ_url,
    vcs_managers,
    codebase_name,
    span,
    run_id=None,
):
    async with db.acquire() as conn:
        # TODO(jelmer): Run these in parallel with gather()
        with span.new_child("sql:codebase"):
            codebase = await conn.fetchrow(
                """\
SELECT codebase.*, named_publish_policy.per_branch_policy AS publish_policy
FROM codebase
LEFT JOIN candidate ON codebase.name = candidate.codebase AND candidate.suite = $2
LEFT JOIN named_publish_policy ON named_publish_policy.name = candidate.publish_policy
WHERE codebase.name = $1""",
                codebase_name,
                suite,
            )
        if codebase is None:
            raise web.HTTPNotFound(text=f"no such codebase: {codebase_name}")
        if run_id is not None:
            with span.new_child("sql:run"):
                run = await get_run(conn, run_id)
            if not run:
                raise web.HTTPNotFound(text=f"no such run: {run_id}")
            merge_proposals = []
        else:
            with span.new_child("sql:unchanged-run"):
                run = await get_last_unabsorbed_run(conn, codebase_name, suite)
            with span.new_child("sql:merge-proposals"):
                merge_proposals = await conn.fetch(
                    """\
SELECT
    DISTINCT ON (merge_proposal.url)
    merge_proposal.url AS url, merge_proposal.status AS status
FROM
    merge_proposal
LEFT JOIN run
ON merge_proposal.revision = run.revision AND run.result_code = 'success'
WHERE run.codebase = $1 AND run.suite = $2
""",
                    codebase["name"],
                    suite,
                )
        if run is None:
            # No runs recorded
            run_id = None
            unchanged_run = None
        else:
            run_id = run["id"]
            if run["main_branch_revision"]:
                with span.new_child("sql:unchanged-run"):
                    unchanged_run = await get_unchanged_run(
                        conn,
                        run["codebase"],
                        run["main_branch_revision"].encode("utf-8"),
                    )
            else:
                unchanged_run = None

        with span.new_child("sql:candidate"):
            candidate = await get_candidate(conn, codebase["name"], suite)
        if candidate is not None:
            (candidate_context, candidate_value, candidate_success_chance) = candidate
        else:
            candidate_context = None
            candidate_value = None
            candidate_success_chance = None
        with span.new_child("sql:previous-runs"):
            previous_runs = await get_previous_runs(conn, codebase["name"], suite)
        with span.new_child("sql:queue-position"):
            queue = Queue(conn)
            (queue_position, queue_wait_time) = await queue.get_position(
                suite, codebase["name"]
            )
        if run_id:
            with span.new_child("sql:reviews"):
                reviews = await conn.fetch(
                    "SELECT * FROM review WHERE run_id = $1 ORDER BY reviewed_at ASC",
                    run_id,
                )
        else:
            reviews = None

    async def show_diff(role):
        try:
            (remote_name, base_revid, revid) = state.get_result_branch(
                run["result_branches"], role
            )
        except KeyError:
            return f"no result branch with role {role}"
        if base_revid == revid:
            return ""
        if run["vcs_type"] is None:
            return "not in a VCS"
        if revid is None:
            return "Branch deleted"
        try:
            with span.new_child("vcs-diff"):
                diff = await vcs_managers[run["vcs_type"]].get_diff(
                    run["codebase"],
                    base_revid.encode("utf-8")
                    if base_revid is not None
                    else NULL_REVISION,
                    revid.encode("utf-8"),
                )
                return diff.decode("utf-8", "replace")
        except ClientResponseError as e:
            return f"Unable to retrieve diff; error {e.status}"
        except ClientConnectorError as e:
            return f"Unable to retrieve diff; error {e}"

    async def show_debdiff():
        if not run["build_version"]:
            return ""
        if not unchanged_run or not unchanged_run["build_version"]:
            return ""
        try:
            with span.new_child("archive-diff"):
                debdiff, content_type = await get_archive_diff(
                    client,
                    differ_url,
                    run["id"],
                    unchanged_run["id"],
                    kind="debdiff",
                    filter_boring=True,
                    accept="text/html",
                )
                return debdiff.decode("utf-8", "replace")
        except BuildDiffUnavailable:
            return ""
        except DebdiffRetrievalError as e:
            return f"Error retrieving debdiff: {e}"

    kwargs: dict[str, Any] = {}
    kwargs.update([(k, v) for (k, v) in codebase.items() if k != "name"])

    if run:
        kwargs.update(run)
        env, plain_command = splitout_env(run["command"])
        kwargs["env"] = env
        kwargs["plain_command"] = plain_command
    else:
        env = {}

    campaign = get_campaign_config(config, suite)

    kwargs.update(
        {
            "codebase": codebase["name"],
            "reviews": reviews,
            "unchanged_run": unchanged_run,
            "merge_proposals": merge_proposals,
            "run_id": run_id,
            "suite": suite,
            "campaign": campaign,
            "show_diff": show_diff,
            "show_debdiff": show_debdiff,
            "previous_runs": previous_runs,
            "run": run,
            "candidate_context": candidate_context,
            "candidate_success_chance": candidate_success_chance,
            "candidate_value": candidate_value,
            "queue_position": queue_position,
            "queue_wait_time": queue_wait_time,
            "changelog_policy": env.get("DEB_UPDATE_CHANGELOG", "auto"),
            "config": config,
        }
    )
    return kwargs


async def generate_candidates(db, suite):
    candidates = []
    async with db.acquire() as conn:
        for row in await iter_candidates(conn, campaign=suite):
            candidates.append((row["codebase"], row["value"]))
        candidates.sort(
            key=lambda x: x[1] if x[1] is not None else float("-inf"), reverse=True
        )
    return {"candidates": candidates, "suite": suite, "campaign": suite}


def html_template(template_name, headers={}):  # noqa: B006
    def decorator(fn):
        async def handle(request):
            vs = await fn(request)
            if isinstance(vs, web.Response):
                return vs
            text = await render_template_for_request(template_name, request, vs)
            return web.Response(content_type="text/html", text=text, headers=headers)

        return handle

    return decorator


async def render_template_for_request(templatename, request, vs: dict[str, Any]):
    update_vars_from_request(vs, request)
    return await aiohttp_jinja2.render_string_async(templatename, request, vs)
