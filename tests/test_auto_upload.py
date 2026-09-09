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

from janitor.debian.auto_upload import is_debian_upload_target


def test_non_success_run_has_no_target_name():
    # regression: a non-success run publishes "target": {}, which crashed
    # handle_result_message with KeyError: 'name' before this check
    result = {
        "code": "codemod-error",
        "log_id": "abc123",
        "target": {},
    }
    assert is_debian_upload_target(result, None) is False


def test_successful_debian_build_matches():
    result = {
        "code": "success",
        "log_id": "abc123",
        "target": {
            "name": "debian",
            "details": {"build_distribution": "lintian-fixes"},
        },
    }
    assert is_debian_upload_target(result, None) is True
    assert is_debian_upload_target(result, ["lintian-fixes"]) is True
    assert is_debian_upload_target(result, ["other-distribution"]) is False


def test_successful_non_debian_target_is_skipped():
    result = {
        "code": "success",
        "log_id": "abc123",
        "target": {"name": "generic", "details": {}},
    }
    assert is_debian_upload_target(result, None) is False
