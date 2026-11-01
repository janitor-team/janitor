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

from datetime import timedelta

from janitor import utcnow
from janitor.site.common import get_unchanged_run

from .conftest import _insert_codebase, _insert_run


async def test_get_unchanged_run(con):
    await _insert_codebase(con, "foo")
    now = utcnow()
    await _insert_run(
        con,
        run_id="control-run",
        codebase="foo",
        campaign="control",
        start_time=now - timedelta(minutes=30),
        finish_time=now,
    )

    unchanged_run = await get_unchanged_run(con, "foo", b"revid")
    expected = {
        "id": "control-run",
        "codebase": "foo",
        "suite": "control",
        "result_code": "success",
        "revision": "revid",
    }
    row = dict(unchanged_run)
    assert {key: row.get(key) for key in expected} == expected


async def test_get_unchanged_run_skips_other_change_set(con):
    await _insert_codebase(con, "foo")
    now = utcnow()
    await _insert_run(
        con,
        run_id="control-run",
        codebase="foo",
        campaign="control",
        start_time=now - timedelta(hours=2),
        finish_time=now - timedelta(hours=1),
    )
    await _insert_run(
        con,
        run_id="other-run",
        codebase="foo",
        campaign="unchanged",
        start_time=now - timedelta(minutes=30),
        finish_time=now,
    )
    # The later run is part of the control run's change set, not its own.
    await con.execute(
        "UPDATE run SET change_set = 'control-run' WHERE id = 'other-run'"
    )

    unchanged_run = await get_unchanged_run(con, "foo", b"revid")
    assert (unchanged_run["id"], unchanged_run["suite"]) == ("control-run", "control")
