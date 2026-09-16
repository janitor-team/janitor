"""Re-derive result_code/description/failure_details from stored logs.

Without re-running the build. Useful after a log-parser update, to
re-classify already-completed runs.
"""

import io
import json
import logging
from typing import Any, Callable, Optional

from buildlog_consultant.common import find_build_failure_description
from buildlog_consultant.sbuild import worker_failure_from_sbuild_log

from .logs import LogRetrievalError


def _is_universal(problem: Any) -> bool:
    # Some buildlog-consultant problem kinds aren't tied to a particular
    # build stage. Older bindings don't expose is_universal() at all, in
    # which case this falls back to stage-qualifying the code.
    is_universal = getattr(problem, "is_universal", None)
    if is_universal is None:
        return False
    return is_universal() if callable(is_universal) else bool(is_universal)


def process_sbuild_log(log_bytes: bytes) -> tuple[str, str, Any]:
    """Classify a build.log, returning (result_code, description, failure_details)."""
    failure = worker_failure_from_sbuild_log(io.BytesIO(log_bytes))

    if failure.error is not None:
        error = failure.error
        if failure.stage and not _is_universal(error):
            code = f"{failure.stage}-{error.kind}"
        else:
            code = error.kind
        return (code, str(error), error.json())

    description = failure.description or "Build failed"
    if failure.stage:
        return (f"build-failed-stage-{failure.stage}", description, None)

    return ("build-failed", description, None)


def process_dist_log(log_bytes: bytes) -> tuple[str, str, Any]:
    """Classify a dist.log, returning (result_code, description, failure_details)."""
    lines = log_bytes.decode("utf-8", "replace").splitlines(keepends=True)
    _match, problem = find_build_failure_description(lines)

    if problem is None:
        return ("dist-command-failed", "Dist command failed", None)

    code = problem.kind if _is_universal(problem) else f"dist-{problem.kind}"
    return (code, str(problem), problem.json())


async def reprocess_run_logs(
    *,
    db,
    logfile_manager,
    codebase: str,
    campaign: str,
    log_id: str,
    command: Optional[str],
    change_set: Optional[str],
    duration,
    result_code: str,
    description: Optional[str],
    failure_details: Any,
    process_fns: list[
        tuple[str, str, Callable[[bytes], Optional[tuple[str, str, Any]]]]
    ],
    dry_run: bool = False,
    reschedule: bool = False,
) -> Optional[tuple[str, str, Any]]:
    """Re-derive a run's classification from its stored log files.

    Returns the new (result_code, description, failure_details) if
    reprocessing found a different classification than what is currently
    stored, or None if nothing changed.
    """
    new_result: Optional[tuple[str, str, Any]] = None
    for prefix, log_name, process_fn in process_fns:
        if not result_code.startswith(prefix):
            continue
        try:
            log_file = await logfile_manager.get_log(codebase, log_id, log_name)
        except FileNotFoundError:
            continue
        except LogRetrievalError as e:
            logging.warning("Failed to retrieve %s for %s: %s", log_name, log_id, e)
            continue
        if log_file is None:
            continue
        new_result = process_fn(log_file.read())
        break

    if new_result is None:
        return None

    new_code, new_description, new_failure_details = new_result
    if (
        new_code == result_code
        and new_description == description
        and new_failure_details == failure_details
    ):
        return None

    logging.info(
        "Reprocessing %s: %s (%r) -> %s (%r)",
        log_id,
        result_code,
        description,
        new_code,
        new_description,
    )

    if not dry_run:
        async with db.acquire() as conn:
            await conn.execute(
                "UPDATE run SET result_code = $1, description = $2, "
                "failure_details = $3 WHERE id = $4",
                new_code,
                new_description,
                json.dumps(new_failure_details)
                if new_failure_details is not None
                else None,
                log_id,
            )
            if reschedule:
                await conn.execute(
                    "INSERT INTO queue (codebase, suite, command, change_set, requester) "
                    "VALUES ($1, $2, $3, $4, 'reprocess-logs') "
                    "ON CONFLICT (codebase, suite, coalesce(change_set, '')) DO NOTHING",
                    codebase,
                    campaign,
                    command,
                    change_set,
                )

    return new_result
