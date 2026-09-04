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

from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def _read_ignore_lines(name):
    path = REPO_ROOT / name
    with path.open() as f:
        return {line.strip() for line in f if line.strip() and not line.startswith("#")}


def test_containerignore_dockerignore_stay_in_sync():
    # buildah reads .containerignore, some CI/tooling paths read
    # .dockerignore - they need the same excludes (credentials.*, *.secret,
    # data/, etc.) or one of the two build paths silently loses coverage
    # for whatever the other one just gained. See the PR that added this.
    containerignore = _read_ignore_lines(".containerignore")
    dockerignore = _read_ignore_lines(".dockerignore")
    assert containerignore == dockerignore, (
        f"in .containerignore but not .dockerignore: {containerignore - dockerignore}\n"
        f"in .dockerignore but not .containerignore: {dockerignore - containerignore}"
    )
