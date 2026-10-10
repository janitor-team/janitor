#!/usr/bin/python
# Copyright (C) 2018 Jelmer Vernooij <jelmer@jelmer.uk>
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
from datetime import timedelta
from typing import Optional

from ._common import analyze_log as _analyze_log_rs  # type: ignore
from .schedule import do_schedule

process_build_log = _analyze_log_rs.process_build_log
process_dist_log = _analyze_log_rs.process_dist_log
process_sbuild_log = _analyze_log_rs.process_sbuild_log

__all__ = [
    "process_build_log",
    "process_dist_log",
    "process_sbuild_log",
    "reprocess_run_logs",
]


async def reprocess_run_logs(
    db,
    logfile_manager,
    *,
    codebase: str,
    campaign: str,
    log_id: str,
    command: str,
    change_set: Optional[str],
    duration: timedelta,
    result_code: str,
    description: str,
    failure_details,
    process_fns,
    dry_run: bool = False,
    reschedule: bool = False,
    log_timeout: Optional[timedelta] = None,
):
    """Reprocess run logs."""
    if result_code in ("dist-no-tarball",):
        return
    for prefix, logname, fn in process_fns:
        if not result_code.startswith(prefix):
            continue
        try:
            logf = await logfile_manager.get_log(
                codebase, log_id, logname, timeout=log_timeout
            )
        except FileNotFoundError:
            return
        else:
            (new_code, new_description, new_phase, new_failure_details) = fn(logf)
            break
    else:
        return

    if (
        new_code != result_code
        or description != new_description
        or failure_details != new_failure_details
    ):
        logging.info(
            "%s/%s: Updated %r, %r ⇒ %r, %r %r",
            codebase,
            log_id,
            result_code,
            description,
            new_code,
            new_description,
            new_phase,
        )
        if not dry_run:
            async with db.acquire() as conn:
                await conn.execute(
                    "UPDATE run SET result_code = $1, description = $2, failure_details = $3 WHERE id = $4",
                    new_code,
                    new_description,
                    new_failure_details,
                    log_id,
                )
                if reschedule and new_code != result_code:
                    await do_schedule(
                        conn,
                        campaign=campaign,
                        change_set=change_set,
                        codebase=codebase,
                        estimated_duration=duration,
                        requester="reprocess-build-results",
                        bucket="reschedule",
                    )
        return (new_code, new_description, new_failure_details)
    return
