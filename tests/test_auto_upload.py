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

import asyncio
import json
import logging
import os

from fakeredis.aioredis import FakeRedis

from janitor.artifacts import LocalArtifactManager
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


def _debian_result(log_id):
    return {
        "code": "success",
        "log_id": log_id,
        "target": {"name": "debian", "details": {"build_distribution": "unstable"}},
    }


async def _publish_until_subscribed(redis, payload):
    for _ in range(200):
        if await redis.publish("result", payload):
            return
        await asyncio.sleep(0.01)
    raise AssertionError("nothing subscribed to the result channel")


async def _wait_for(predicate):
    for _ in range(200):
        if predicate():
            return
        await asyncio.sleep(0.01)
    raise AssertionError("timed out waiting for the listener to catch up")


def _artifacts_without_changes(root, log_id):
    """Store artifacts for a run that built no .changes file."""
    os.mkdir(os.path.join(root, log_id))
    with open(os.path.join(root, log_id, "build.log"), "w") as f:
        f.write("dummy\n")


def _uploads_started(caplog):
    prefix = "Uploading results for "
    return [m.removeprefix(prefix) for m in caplog.messages if m.startswith(prefix)]


def _logged_errors(caplog):
    return [r.exc_info[0] for r in caplog.records if r.exc_info]


async def test_listener_survives_an_unexpected_upload_error(tmp_path, caplog):
    caplog.set_level(logging.INFO)
    # a directory among the artifacts can not be retrieved, which raises OSError
    os.makedirs(tmp_path / "bad" / "subdir")
    _artifacts_without_changes(tmp_path, "good")

    redis = FakeRedis()
    listener = asyncio.create_task(
        auto_upload.listen_to_runner(
            redis,
            LocalArtifactManager(str(tmp_path)),
            "local",
            distributions=["unstable"],
        )
    )
    try:
        # a non-debian result is skipped, so this only waits for the subscription
        await _publish_until_subscribed(
            redis, json.dumps({"code": "success", "target": {"name": "generic"}})
        )

        await redis.publish("result", json.dumps(_debian_result("bad")))
        await redis.publish("result", json.dumps(_debian_result("good")))

        await _wait_for(lambda: "good" in _uploads_started(caplog))
        assert _uploads_started(caplog) == ["bad", "good"]
        (error,) = _logged_errors(caplog)
        assert issubclass(error, OSError)
        assert not listener.done()
    finally:
        listener.cancel()
        try:
            await listener
        except asyncio.CancelledError:
            pass


async def test_result_without_a_log_id_is_not_uploaded(tmp_path, caplog):
    caplog.set_level(logging.INFO)
    _artifacts_without_changes(tmp_path, "good")

    redis = FakeRedis()
    listener = asyncio.create_task(
        auto_upload.listen_to_runner(
            redis,
            LocalArtifactManager(str(tmp_path)),
            "local",
            distributions=["unstable"],
        )
    )
    try:
        await _publish_until_subscribed(
            redis, json.dumps({"code": "success", "target": {"name": "generic"}})
        )

        no_log_id = _debian_result("unused")
        del no_log_id["log_id"]
        await redis.publish("result", json.dumps(no_log_id))
        await redis.publish("result", json.dumps(_debian_result("good")))

        await _wait_for(lambda: "good" in _uploads_started(caplog))
        assert _uploads_started(caplog) == ["good"]
        assert _logged_errors(caplog) == [KeyError]
        assert not listener.done()
    finally:
        listener.cancel()
        try:
            await listener
        except asyncio.CancelledError:
            pass
