#!/usr/bin/python
# Copyright (C) 2026 Jelmer Vernooij <jelmer@jelmer.uk>
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

import io
from datetime import datetime, timedelta
from unittest.mock import MagicMock, patch

from janitor.reprocess_logs import (
    process_dist_log,
    process_sbuild_log,
    reprocess_run_logs,
)
from janitor.runner import store_change_set, store_run


def test_process_sbuild_log_unclassified():
    result_code, description, failure_details = process_sbuild_log(
        b"nothing interesting here\n"
    )
    assert result_code == "build-failed"
    assert description == "build failed"
    assert failure_details is None


def test_process_sbuild_log_stage_qualified_error():
    error = MagicMock()
    error.kind = "some-kind"
    error.is_universal.return_value = False
    error.json.return_value = {"kind": "some-kind"}
    failure = MagicMock(error=error, stage="build")
    with patch("janitor.reprocess_logs.worker_failure_from_sbuild_log", return_value=failure):
        result_code, description, failure_details = process_sbuild_log(b"log")
    assert result_code == "build-some-kind"
    assert description == str(error)
    assert failure_details == {"kind": "some-kind"}


def test_process_sbuild_log_universal_error_not_stage_qualified():
    error = MagicMock()
    error.kind = "some-kind"
    error.is_universal.return_value = True
    error.json.return_value = {"kind": "some-kind"}
    failure = MagicMock(error=error, stage="build")
    with patch("janitor.reprocess_logs.worker_failure_from_sbuild_log", return_value=failure):
        result_code, _description, _failure_details = process_sbuild_log(b"log")
    assert result_code == "some-kind"


def test_process_sbuild_log_error_without_is_universal_attribute():
    # Older buildlog-consultant bindings don't expose is_universal() at all;
    # _is_universal() falls back to treating the error as stage-qualified.
    error = MagicMock(spec=["kind", "json"])
    error.kind = "some-kind"
    error.json.return_value = {"kind": "some-kind"}
    failure = MagicMock(error=error, stage="build")
    with patch("janitor.reprocess_logs.worker_failure_from_sbuild_log", return_value=failure):
        result_code, _description, _failure_details = process_sbuild_log(b"log")
    assert result_code == "build-some-kind"


def test_process_sbuild_log_no_error_but_stage():
    failure = MagicMock(error=None, stage="apply", description=None)
    with patch("janitor.reprocess_logs.worker_failure_from_sbuild_log", return_value=failure):
        result_code, description, failure_details = process_sbuild_log(b"log")
    assert result_code == "build-failed-stage-apply"
    assert description == "Build failed"
    assert failure_details is None


def test_process_dist_log_unclassified():
    result_code, description, failure_details = process_dist_log(
        b"nothing interesting here\n"
    )
    assert result_code == "dist-command-failed"
    assert description == "Dist command failed"
    assert failure_details is None


def test_process_dist_log_stage_qualified_problem():
    problem = MagicMock()
    problem.kind = "some-problem"
    problem.is_universal.return_value = False
    problem.json.return_value = {"kind": "some-problem"}
    with patch(
        "janitor.reprocess_logs.find_build_failure_description",
        return_value=(None, problem),
    ):
        result_code, description, failure_details = process_dist_log(b"log")
    assert result_code == "dist-some-problem"
    assert description == str(problem)
    assert failure_details == {"kind": "some-problem"}


def test_process_dist_log_universal_problem_not_prefixed():
    problem = MagicMock()
    problem.kind = "some-problem"
    problem.is_universal.return_value = True
    problem.json.return_value = {"kind": "some-problem"}
    with patch(
        "janitor.reprocess_logs.find_build_failure_description",
        return_value=(None, problem),
    ):
        result_code, _description, _failure_details = process_dist_log(b"log")
    assert result_code == "some-problem"


class FakeLogfileManager:
    def __init__(self, logs):
        self._logs = logs

    async def get_log(self, codebase, run_id, name):
        try:
            return io.BytesIO(self._logs[name])
        except KeyError:
            raise FileNotFoundError(name) from None


async def _insert_codebase(conn, name):
    await conn.execute(
        "INSERT INTO codebase (name, branch_url, url) VALUES ($1, $2, $2)",
        name,
        f"https://example.com/{name}.git",
    )


async def _insert_run(conn, *, run_id, codebase, result_code):
    await store_change_set(conn, run_id, campaign="mycampaign")
    await store_run(
        conn,
        run_id=run_id,
        codebase=codebase,
        campaign="mycampaign",
        vcs_type="git",
        subpath="",
        start_time=datetime.utcnow() - timedelta(minutes=5),
        finish_time=datetime.utcnow(),
        command="true",
        result_code=result_code,
        codemod_result={},
        main_branch_revision=b"revid",
        revision=b"revid",
        description="old description",
        context=None,
        instigated_context=None,
        logfilenames=[],
        value=1,
        change_set=run_id,
        worker_name=None,
        branch_url=f"https://example.com/{codebase}.git",
    )


async def test_reprocess_run_logs_updates_on_new_classification(db):
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_run(conn, run_id="r1", codebase="foo", result_code="build-failed")

    logfile_manager = FakeLogfileManager({"build.log": b"nothing interesting here\n"})
    result = await reprocess_run_logs(
        db=db,
        logfile_manager=logfile_manager,
        codebase="foo",
        campaign="mycampaign",
        log_id="r1",
        command="true",
        change_set="r1",
        duration=timedelta(minutes=5),
        result_code="build-failed",
        description="old description",
        failure_details=None,
        process_fns=[("build-", "build.log", process_sbuild_log)],
    )

    assert result == ("build-failed", "build failed", None)
    async with db.acquire() as conn:
        row = await conn.fetchrow(
            "SELECT result_code, description FROM run WHERE id = 'r1'"
        )
    assert row["result_code"] == "build-failed"
    assert row["description"] == "build failed"


async def test_reprocess_run_logs_no_change_returns_none(db):
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_run(conn, run_id="r1", codebase="foo", result_code="build-failed")
        await conn.execute(
            "UPDATE run SET description = 'build failed' WHERE id = 'r1'"
        )

    logfile_manager = FakeLogfileManager({"build.log": b"nothing interesting here\n"})
    result = await reprocess_run_logs(
        db=db,
        logfile_manager=logfile_manager,
        codebase="foo",
        campaign="mycampaign",
        log_id="r1",
        command="true",
        change_set="r1",
        duration=timedelta(minutes=5),
        result_code="build-failed",
        description="build failed",
        failure_details=None,
        process_fns=[("build-", "build.log", process_sbuild_log)],
    )

    assert result is None


async def test_reprocess_run_logs_dry_run_does_not_update_db(db):
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_run(conn, run_id="r1", codebase="foo", result_code="build-failed")

    logfile_manager = FakeLogfileManager({"build.log": b"nothing interesting here\n"})
    result = await reprocess_run_logs(
        db=db,
        logfile_manager=logfile_manager,
        codebase="foo",
        campaign="mycampaign",
        log_id="r1",
        command="true",
        change_set="r1",
        duration=timedelta(minutes=5),
        result_code="build-failed",
        description="old description",
        failure_details=None,
        process_fns=[("build-", "build.log", process_sbuild_log)],
        dry_run=True,
    )

    assert result == ("build-failed", "build failed", None)
    async with db.acquire() as conn:
        row = await conn.fetchrow(
            "SELECT result_code, description FROM run WHERE id = 'r1'"
        )
    assert row["description"] == "old description"


async def test_reprocess_run_logs_reschedule_inserts_queue_entry(db):
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_run(conn, run_id="r1", codebase="foo", result_code="build-failed")

    logfile_manager = FakeLogfileManager({"build.log": b"nothing interesting here\n"})
    await reprocess_run_logs(
        db=db,
        logfile_manager=logfile_manager,
        codebase="foo",
        campaign="mycampaign",
        log_id="r1",
        command="true",
        change_set="r1",
        duration=timedelta(minutes=5),
        result_code="build-failed",
        description="old description",
        failure_details=None,
        process_fns=[("build-", "build.log", process_sbuild_log)],
        reschedule=True,
    )

    async with db.acquire() as conn:
        row = await conn.fetchrow(
            "SELECT codebase, suite, requester FROM queue WHERE codebase = 'foo'"
        )
    assert row["suite"] == "mycampaign"
    assert row["requester"] == "reprocess-logs"


async def test_reprocess_run_logs_no_matching_prefix_returns_none(db):
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_run(conn, run_id="r1", codebase="foo", result_code="success")

    logfile_manager = FakeLogfileManager({"build.log": b"nothing interesting here\n"})
    result = await reprocess_run_logs(
        db=db,
        logfile_manager=logfile_manager,
        codebase="foo",
        campaign="mycampaign",
        log_id="r1",
        command="true",
        change_set="r1",
        duration=timedelta(minutes=5),
        result_code="success",
        description="all good",
        failure_details=None,
        process_fns=[("build-", "build.log", process_sbuild_log)],
    )

    assert result is None


async def test_reprocess_run_logs_missing_log_file_skipped(db):
    async with db.acquire() as conn:
        await _insert_codebase(conn, "foo")
        await _insert_run(conn, run_id="r1", codebase="foo", result_code="build-failed")

    logfile_manager = FakeLogfileManager({})
    result = await reprocess_run_logs(
        db=db,
        logfile_manager=logfile_manager,
        codebase="foo",
        campaign="mycampaign",
        log_id="r1",
        command="true",
        change_set="r1",
        duration=timedelta(minutes=5),
        result_code="build-failed",
        description="old description",
        failure_details=None,
        process_fns=[("build-", "build.log", process_sbuild_log)],
    )

    assert result is None
