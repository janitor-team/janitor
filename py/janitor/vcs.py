#!/usr/bin/python3
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

__all__ = [
    "is_alioth_url",
    "is_authenticated_url",
    "BranchOpenFailure",
    "open_branch_ext",
    "UnsupportedVcs",
    "VcsStoreError",
    "VcsManager",
    "LocalGitVcsManager",
    "LocalBzrVcsManager",
    "RemoteGitVcsManager",
    "RemoteBzrVcsManager",
    "get_run_diff",
    "get_vcs_managers_from_config",
    "get_branch_vcs_type",
]

import sys
from io import BytesIO
from typing import Optional

import breezy.bzr  # noqa: F401
import breezy.git  # noqa: F401
from breezy import urlutils
from breezy.diff import show_diff_trees
from breezy.errors import (
    NoSuchRevision,
    NotBranchError,
)
from breezy.repository import Repository

from ._common import (
    get_branch_vcs_type,
    is_alioth_url,
    is_authenticated_url,
)
from ._common import (  # type: ignore
    vcs as _vcs_rs,
)

VcsManager = _vcs_rs.VcsManager
RemoteBzrVcsManager = _vcs_rs.RemoteBzrVcsManager
RemoteGitVcsManager = _vcs_rs.RemoteGitVcsManager
LocalBzrVcsManager = _vcs_rs.LocalBzrVcsManager
LocalGitVcsManager = _vcs_rs.LocalGitVcsManager
get_local_vcs_manager = _vcs_rs.get_local_vcs_manager
get_remote_vcs_manager = _vcs_rs.get_remote_vcs_manager
get_vcs_manager = _vcs_rs.get_vcs_manager
get_vcs_managers = _vcs_rs.get_vcs_managers
BranchOpenFailure = _vcs_rs.BranchOpenFailure
open_branch_ext = _vcs_rs.open_branch_ext
UnsupportedVcs = _vcs_rs.UnsupportedVcs
VcsStoreError = _vcs_rs.VcsStoreError


def get_run_diff(vcs_manager: VcsManager, run, role) -> bytes:  # type: ignore
    f = BytesIO()
    repo: Optional[Repository]
    try:
        repo = vcs_manager.get_repository(run.codebase)  # type: ignore
    except NotBranchError:
        repo = None
    if repo is None:
        return b"Local VCS repository for %s temporarily inaccessible" % (
            run.codebase.encode("ascii")
        )
    for actual_role, _, base_revision, revision in run.result_branches:
        if role == actual_role:
            old_revid = base_revision
            new_revid = revision
            break
    else:
        return b"No branch with role %s" % role.encode()

    try:
        old_tree = repo.revision_tree(old_revid)
    except NoSuchRevision:
        return b"Old revision %s temporarily missing" % (old_revid,)
    try:
        new_tree = repo.revision_tree(new_revid)
    except NoSuchRevision:
        return b"New revision %s temporarily missing" % (new_revid,)
    show_diff_trees(old_tree, new_tree, to_file=f)
    return f.getvalue()


def get_vcs_managers_from_config(config) -> dict[str, VcsManager]:  # type: ignore
    ret: dict[str, VcsManager] = {}  # type: ignore
    if config.git_location:
        parsed = urlutils.URL.from_string(config.git_location)
        if parsed.scheme in ("", "file"):
            ret["git"] = LocalGitVcsManager(parsed.path)
        else:
            ret["git"] = RemoteGitVcsManager(
                config.git_location,
            )
    if config.bzr_location:
        parsed = urlutils.URL.from_string(config.bzr_location)
        if parsed.scheme in ("", "file"):
            ret["bzr"] = LocalBzrVcsManager(parsed.path)
        else:
            ret["bzr"] = RemoteBzrVcsManager(
                config.bzr_location,
            )
    return ret


def main(argv=None):
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("url", type=str)
    args = parser.parse_args(argv)
    open_branch_ext(args.url)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
