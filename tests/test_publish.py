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

import janitor.publish as publish


async def test_store_publish_failure_with_no_target_branch(con):
    await con.execute(
        "INSERT INTO codebase (name, branch_url, url) VALUES ($1, $2, $2)",
        "mypkg",
        "https://example.com/mypkg.git",
    )
    await con.execute(
        "INSERT INTO change_set (id, campaign) VALUES ($1, $2)",
        "cs1",
        "lintian-fixes",
    )

    # A publish attempt that never resolved a target branch (e.g. an
    # unsupported forge) has no target_branch_url to record - this must
    # not crash the way it did when the column was NOT NULL.
    await publish.store_publish(
        con,
        change_set="cs1",
        codebase="mypkg",
        branch_name=None,
        target_branch_url=None,
        target_branch_web_url=None,
        main_branch_revision=None,
        revision=None,
        role="main",
        mode="propose",
        result_code="hoster-unsupported",
        description="Forge unsupported: example.com.",
    )

    row = await con.fetchrow(
        "SELECT target_branch_url FROM publish WHERE codebase = $1", "mypkg"
    )
    assert row["target_branch_url"] is None
