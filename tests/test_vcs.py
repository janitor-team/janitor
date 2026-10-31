#!/usr/bin/python
# Copyright (C) 2020 Jelmer Vernooij <jelmer@jelmer.uk>
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

from breezy import controldir, urlutils
from breezy.bzr import BzrProber
from breezy.git import LocalGitProber
from breezy.tests import TestCaseWithTransport
from breezy.transport import Transport

from janitor.vcs import (
    BranchOpenFailure,
    LocalBzrVcsManager,
    RemoteBzrVcsManager,
    RemoteGitVcsManager,
    get_run_diff,
    get_vcs_managers,
    is_alioth_url,
    is_authenticated_url,
    open_branch_ext,
)


class GetRunDiffsTests(TestCaseWithTransport):
    def test_diff(self):
        vcs_manager = LocalBzrVcsManager(".")
        self.make_repository("pkg", shared=True)
        branch = controldir.ControlDir.create_branch_convenience(
            "pkg/trunk", force_new_repo=False
        )
        tree = branch.controldir.open_workingtree()
        self.build_tree_contents(
            [
                (
                    "pkg/trunk/a",
                    """\
File a
""",
                )
            ]
        )
        tree.add("a")
        old_revid = tree.commit("base")

        self.build_tree_contents(
            [
                (
                    "pkg/trunk/a",
                    """\
File a
File b
""",
                )
            ]
        )
        new_revid = tree.commit("actual")

        class Run:
            codebase = "pkg"
            main_branch_revision = old_revid
            revision = new_revid
            result_branches = [("main", "", old_revid, new_revid)]

        lines = get_run_diff(vcs_manager, Run(), "main").splitlines()
        self.assertEqual(b"=== modified file 'a'", lines[0])
        self.assertEqual(b"@@ -1,1 +1,2 @@", lines[3])
        self.assertEqual(b" File a", lines[4])
        self.assertEqual(b"+File b", lines[5])
        self.assertEqual(b"", lines[6])


class OpenBranchExtTests(TestCaseWithTransport):
    def test_probers(self):
        self.make_branch("branch")
        url = urlutils.local_path_to_url("branch")
        open_branch_ext(url, probers=[BzrProber])
        self.assertRaises(
            BranchOpenFailure, open_branch_ext, url, probers=[LocalGitProber]
        )

    def test_possible_transports(self):
        self.make_branch("branch")
        possible_transports: list[Transport] = []
        branch = open_branch_ext(
            urlutils.local_path_to_url("branch"),
            possible_transports=possible_transports,
        )
        self.assertEqual([branch.user_url], [t.base for t in possible_transports])


def test_authenticated():
    assert is_authenticated_url("git+ssh://git@github.com/jelmer/janitor")
    assert is_authenticated_url("bzr+ssh://git@github.com/jelmer/janitor")


def test_not_authenticated():
    assert not is_authenticated_url("https://github.com/jelmer/janitor")
    assert not is_authenticated_url("git://github.com/jelmer/janitor")


def test_is_alioth_url():
    assert is_alioth_url("https://svn.debian.org/svn/blah")
    assert is_alioth_url("git+ssh://git.debian.org/blah")
    assert is_alioth_url("https://git.debian.org/blah")
    assert is_alioth_url("http://alioth.debian.org/blah")
    assert not is_alioth_url("https://salsa.debian.org/blah")


def test_get_vcs_managers():
    assert {
        "bzr": RemoteBzrVcsManager("https://example.com/bzr/"),
        "git": RemoteGitVcsManager("https://example.com/git/"),
    } == get_vcs_managers("https://example.com/")
    assert {"git": RemoteGitVcsManager("https://example.com/git/")} == get_vcs_managers(
        "git=https://example.com/git/"
    )
    assert {
        "bzr": RemoteBzrVcsManager("https://example.com/bzr"),
        "git": RemoteGitVcsManager("https://example.com/git/"),
    } == get_vcs_managers("git=https://example.com/git/,bzr=https://example.com/bzr")
