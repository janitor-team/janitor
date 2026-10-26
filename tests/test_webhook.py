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
import logging

from aiohttp import web
from breezy.controldir import ControlDir

from janitor.site import webhook


async def test_main_tries_each_codebase_with_a_branch(aiohttp_server, caplog, tmp_path):
    branch = ControlDir.create_branch_convenience(str(tmp_path / "foo"))
    requests = []

    async def handle_codebases(request):
        requests.append(request.path)
        return web.json_response(
            [
                {"name": "foo", "branch_url": branch.user_url},
                {"name": "no-url", "branch_url": None},
                {"name": "gone", "branch_url": (tmp_path / "gone").as_uri()},
            ]
        )

    runner = web.Application()
    runner.router.add_get("/codebases", handle_codebases)
    server = await aiohttp_server(runner)

    # main() starts its own event loop, so it can not run inside this one.
    with caplog.at_level(logging.WARNING):
        await asyncio.to_thread(
            webhook.main, ["--runner-url", str(server.make_url("/")), "http://callback"]
        )

    assert requests == ["/codebases"]
    assert caplog.messages == [f"Ignoring branch with unknown forge: {branch.user_url}"]


def _launchpad_git_push(repository, path, ref_changes):
    # Shape of GitRefScanJob.composeWebhookPayload in Launchpad.
    body = {
        "git_repository": repository,
        "git_repository_path": path,
        "ref_changes": ref_changes,
    }
    return [
        (change.urls, change.after)
        for change in webhook.get_git_changes_from_launchpad_webhook(body)
    ]


def test_launchpad_git_push_updates_creates_and_deletes_refs():
    changes = _launchpad_git_push(
        "/~user/project/+git/repo",
        "~user/project/+git/repo",
        {
            "refs/heads/master": {
                "old": {"commit_sha1": "a" * 40},
                "new": {"commit_sha1": "b" * 40},
            },
            "refs/tags/1.0": {"old": {"commit_sha1": "c" * 40}, "new": None},
            "refs/tags/2.0": {"old": None, "new": {"commit_sha1": "d" * 40}},
        },
    )
    assert changes == [
        (
            [
                "https://git.launchpad.net/~user/project/+git/repo,branch=master",
                "git+ssh://git.launchpad.net/~user/project/+git/repo,branch=master",
            ],
            b"b" * 40,
        ),
        (
            [
                "https://git.launchpad.net/~user/project/+git/repo,ref=refs%2Ftags%2F2.0",
                "git+ssh://git.launchpad.net/~user/project/+git/repo,ref=refs%2Ftags%2F2.0",
            ],
            b"d" * 40,
        ),
        (["https://git.launchpad.net/~user/project/+git/repo"], None),
        (["git+ssh://git.launchpad.net/~user/project/+git/repo"], None),
    ]


def test_launchpad_git_push_to_default_repository():
    changes = _launchpad_git_push(
        "/~user/project/+git/project",
        "project",
        {"refs/heads/main": {"old": None, "new": {"commit_sha1": "e" * 40}}},
    )
    assert changes == [
        (
            [
                "https://git.launchpad.net/~user/project/+git/project,branch=main",
                "git+ssh://git.launchpad.net/~user/project/+git/project,branch=main",
                "https://git.launchpad.net/project,branch=main",
                "git+ssh://git.launchpad.net/project,branch=main",
            ],
            b"e" * 40,
        ),
        (["https://git.launchpad.net/~user/project/+git/project"], None),
        (["git+ssh://git.launchpad.net/~user/project/+git/project"], None),
        (["https://git.launchpad.net/project"], None),
        (["git+ssh://git.launchpad.net/project"], None),
    ]
