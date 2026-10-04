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

import os

from janitor.debian import auto_upload
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


CHANGES = "pkg_1.0-1_amd64.changes"


class _FakeArtifactManager:
    """Drops a single .changes file where retrieve_artifacts would put it."""

    async def retrieve_artifacts(self, log_id, td, **kwargs):
        with open(os.path.join(td, CHANGES), "w") as f:
            f.write("dummy\n")


async def test_debsign_gets_the_changes_path(monkeypatch):
    """The signer takes a path to the .changes file, not a directory and a name."""
    seen = []

    monkeypatch.setattr(
        auto_upload, "debsign", lambda path, keyid=None: seen.append((path, keyid))
    )
    monkeypatch.setattr(auto_upload, "dput_changes", lambda path, host=None: None)

    await auto_upload.upload_build_result(
        "run-1", _FakeArtifactManager(), "local", debsign_keyid="DEADBEEF"
    )

    assert len(seen) == 1
    path, keyid = seen[0]
    assert os.path.basename(path) == CHANGES
    assert os.path.isabs(path)
    assert keyid == "DEADBEEF"


async def test_dput_gets_the_changes_path(monkeypatch):
    """The uploader takes the same path, and the configured host."""
    seen = []

    monkeypatch.setattr(auto_upload, "debsign", lambda path, keyid=None: None)
    monkeypatch.setattr(
        auto_upload, "dput_changes", lambda path, host=None: seen.append((path, host))
    )

    await auto_upload.upload_build_result(
        "run-1", _FakeArtifactManager(), "local", debsign_keyid=None
    )

    assert len(seen) == 1
    path, host = seen[0]
    assert os.path.basename(path) == CHANGES
    assert host == "local"


async def test_debsign_failure_skips_dput(monkeypatch):
    """A changes file that could not be signed must not be uploaded."""
    dput_calls = []

    def failing_debsign(path, keyid=None):
        raise auto_upload.DebsignFailure("gpg: no secret key")

    monkeypatch.setattr(auto_upload, "debsign", failing_debsign)
    monkeypatch.setattr(
        auto_upload, "dput_changes", lambda path, host=None: dput_calls.append(path)
    )

    await auto_upload.upload_build_result(
        "run-1", _FakeArtifactManager(), "local", debsign_keyid="DEADBEEF"
    )

    assert dput_calls == []
