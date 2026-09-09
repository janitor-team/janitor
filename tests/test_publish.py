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

import pytest

import janitor.publish as publish


class _FakeVcsManager:
    def get_branch_url(self, codebase, branch_name):
        return f"https://example.com/{codebase}/{branch_name}"


async def test_publish_one_sends_revision_id_and_invokes_compiled_binary(monkeypatch):
    captured = {}

    async def fake_run_worker_process(args, request, **kwargs):
        captured["args"] = args
        captured["request"] = request
        return 1, {"code": "some-failure", "description": "boom"}

    monkeypatch.setattr(publish, "run_worker_process", fake_run_worker_process)

    worker = publish.PublishWorker()

    with pytest.raises(publish.PublishFailure):
        await worker.publish_one(
            campaign="lintian-fixes",
            codebase="mypkg",
            command="lintian-brush",
            target_branch_url="https://example.com/mypkg",
            mode="propose",
            role="main",
            revision=b"somerevid",
            log_id="log-1",
            unchanged_id=None,
            derived_branch_name="lintian-fixes",
            rate_limit_bucket=None,
            vcs_manager=_FakeVcsManager(),
        )

    assert captured["args"] == ["janitor-publish-one"]
    assert captured["request"]["revision_id"] == "somerevid"
    assert "revision" not in captured["request"]


async def test_publish_one_passes_template_env_path_to_compiled_binary(monkeypatch):
    captured = {}

    async def fake_run_worker_process(args, request, **kwargs):
        captured["args"] = args
        return 1, {"code": "some-failure", "description": "boom"}

    monkeypatch.setattr(publish, "run_worker_process", fake_run_worker_process)

    worker = publish.PublishWorker(template_env_path="/etc/janitor/templates")

    with pytest.raises(publish.PublishFailure):
        await worker.publish_one(
            campaign="lintian-fixes",
            codebase="mypkg",
            command="lintian-brush",
            target_branch_url="https://example.com/mypkg",
            mode="propose",
            role="main",
            revision=b"somerevid",
            log_id="log-1",
            unchanged_id=None,
            derived_branch_name="lintian-fixes",
            rate_limit_bucket=None,
            vcs_manager=_FakeVcsManager(),
        )

    assert captured["args"] == [
        "janitor-publish-one",
        "--template-env-path=/etc/janitor/templates",
    ]
