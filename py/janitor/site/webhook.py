#!/usr/bin/python
# Copyright (C) 2019-2021 Jelmer Vernooij <jelmer@jelmer.uk>
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

import json
import logging
from typing import Union

import asyncpg
from aiohttp import ClientSession, web
from breezy import urlutils
from breezy.forge import UnsupportedForge, get_forge
from breezy.git.mapping import default_mapping
from breezy.git.refs import ref_to_branch_name
from breezy.git.urls import git_url_to_bzr_url
from yarl import URL


def subscribe_webhook_github(branch, github, callback_url):
    from breezy.plugins.github.forge import parse_github_branch_url

    (owner, repo_name, branch_name) = parse_github_branch_url(branch)

    path = f"repos/{owner}/{repo_name}/hooks"

    data = {
        "name": "web",
        "active": True,
        "events": ["push"],
        "config": {
            "url": callback_url,
            "content_type": "json",
            "insecure_ssl": "0",
        },
    }

    response = github._api_request("POST", path, body=json.dumps(data).encode("utf-8"))

    if response.status in (200, 201):
        return True
    if response.status == 422:
        data = json.loads(response.text)
        if any(
            [
                x["message"] == "Hook already exists on this repository"
                for x in data["errors"]  # type: ignore
            ]
        ):
            return True
        logging.warning(
            "Unable to subscribe to %s/%s: %d: %s",
            owner,
            repo_name,
            response.status,
            response.text,
        )
        return False
    return True


def subscribe_webhook_gitlab(branch, gitlab, callback_url):
    from breezy.plugins.gitlab.forge import NotGitLabUrl, parse_gitlab_branch_url

    try:
        (host, project_name, branch_name) = parse_gitlab_branch_url(branch)
    except NotGitLabUrl as e:
        raise UnsupportedForge(branch.user_url) from e

    project = gitlab._get_project(project_name)
    path = "projects/{}/hooks".format(urlutils.quote(str(project["id"]), ""))

    response = gitlab._api_request(
        "POST", path, fields={"url": callback_url, "push_events": True}
    )
    if response.status not in (200, 201):
        logging.warning(
            "Unable to subscribe to %s: %d %s",
            project_name,
            response.status,
            response.text,
        )
        return False
    return True


def subscribe_webhook(branch, callback_url):
    from breezy.plugins.github.forge import GitHub
    from breezy.plugins.gitlab.forge import GitLab

    forge = get_forge(branch)

    if isinstance(forge, GitHub):
        return subscribe_webhook_github(branch, forge, callback_url)
    elif isinstance(forge, GitLab):
        return subscribe_webhook_gitlab(branch, forge, callback_url)
    else:
        raise UnsupportedForge(branch)


def is_webhook_request(request):
    return (
        "X-Gitlab-Event" in request.headers
        or "X-GitHub-Event" in request.headers
        or "X-Gitea-Event" in request.headers
        or "X-Gogs-Event" in request.headers
        or "X-Launchpad-Event-Type" in request.headers
    )


class GitChange:
    urls: set[str]
    after: bytes

    def __init__(self, urls, after) -> None:
        self.urls = urls
        self.after = after

    def after_revision_id(self) -> bytes:
        if self.after is None:
            return None
        return default_mapping.revision_id_foreign_to_bzr(self.after)


class BzrChange:
    urls: set[str]
    after: bytes

    def __init__(self, urls, after) -> None:
        self.urls = urls
        self.after = after

    def after_revision_id(self):
        return self.after


def get_changes_from_github_webhook(body):
    # https://docs.github.com/en/developers/webhooks-and-events/webhooks/webhook-events-and-payloads#push
    url_keys = ["clone_url", "html_url", "git_url", "ssh_url"]
    urls = []
    for url_key in url_keys:
        try:
            url = body["repository"][url_key]
        except KeyError:
            logging.warning(
                "URL key %r not present for repository: %r", url_key, body["repository"]
            )
            continue
        urls.append(git_url_to_bzr_url(url, ref=body["ref"].encode()))
        try:
            branch_name = ref_to_branch_name(body["ref"].encode())
        except ValueError:
            pass
        else:
            if branch_name == body["repository"].get("default_branch"):
                urls.append(git_url_to_bzr_url(url))
    return [GitChange(urls, body["after"].encode())]


def get_bzr_changes_from_launchpad_webhook(body):
    urls = [
        base + body["bzr_branch_path"]
        for base in [
            "https://code.launchpad.net/",
            "https://bazaar.launchpad.net/",
            "lp:",
        ]
    ]

    return [BzrChange(urls, body["new"]["revision_id"].encode("utf-8"))]


def get_git_changes_from_launchpad_webhook(body):
    # See composeWebhookPayload in Launchpad's lib/lp/code/model/gitjob.py.
    paths = [body["git_repository"].lstrip("/"), body["git_repository_path"]]
    base_urls = []
    for path in dict.fromkeys(paths):
        base_urls.extend(
            [
                "https://git.launchpad.net/" + path,
                "git+ssh://git.launchpad.net/" + path,
            ]
        )
    ret = []
    for ref, changes in body["ref_changes"].items():
        if changes["new"] is None:
            continue
        urls = []
        for base_url in base_urls:
            urls.append(git_url_to_bzr_url(base_url, ref=ref.encode()))
        ret.append(GitChange(urls, after=changes["new"]["commit_sha1"].encode()))
    # No idea what the default branch is, so let's trigger on everything
    # for now:
    for base_url in base_urls:
        ret.append(GitChange([git_url_to_bzr_url(base_url)], after=None))
    return ret


def get_changes_from_gitlab_webhook(body):
    # https://docs.gitlab.com/ee/user/project/integrations/webhook_events.html#push-events
    url_keys = ["git_http_url", "git_ssh_url"]
    urls = []
    for url_key in url_keys:
        url = body["project"][url_key]
        urls.append(git_url_to_bzr_url(url, ref=body["ref"].encode()))
        try:
            branch_name = ref_to_branch_name(body["ref"].encode())
        except ValueError:
            pass
        else:
            if branch_name == body["project"].get("default_branch"):
                urls.append(git_url_to_bzr_url(url))
    return [GitChange(urls, body["after"].encode())]


async def get_codebases_by_change(
    conn: asyncpg.Connection, change: Union[GitChange, BzrChange]
):
    query = """
SELECT
  name, branch_url
FROM
  codebase
WHERE
  branch_url = ANY($1::text[])
"""
    candidates = []
    for url in change.urls:
        candidates.extend([url.rstrip("/"), url.rstrip("/") + "/"])
    return await conn.fetch(query, candidates)


async def parse_webhook(request, db):
    if request.content_type == "application/json":
        body = await request.json()
    elif request.content_type == "application/x-www-form-urlencoded":
        post = await request.post()
        body = json.loads(post["payload"])
    else:
        raise web.HTTPUnsupportedMediaType(
            text=f"Invalid content type {request.content_type}"
        )
    changes: list[Union[GitChange, BzrChange]]
    if "X-Gitlab-Event" in request.headers:
        if request.headers["X-Gitlab-Event"] != "Push Hook":
            return
        changes = get_changes_from_gitlab_webhook(body)
        # TODO(jelmer: If nothing found, then maybe fall back to
        # urlutils.basename(body['project']['path_with_namespace'])?
    elif "X-GitHub-Event" in request.headers:
        if request.headers["X-GitHub-Event"] not in ("push",):
            return
        changes = get_changes_from_github_webhook(body)
    elif "X-Gitea-Event" in request.headers:
        if request.headers["X-Gitea-Event"] not in ("push",):
            return
        changes = get_changes_from_github_webhook(body)
    elif "X-Gogs-Event" in request.headers:
        if request.headers["X-Gogs-Event"] not in ("push",):
            return
        changes = get_changes_from_github_webhook(body)
    elif "X-Launchpad-Event-Type" in request.headers:
        if request.headers["X-Launchpad-Event-Type"] not in (
            "bzr:push:0.1",
            "git:push:0.1",
        ):
            return
        if request.headers["X-Launchpad-Event-Type"] == "bzr:push:0.1":
            changes = get_bzr_changes_from_launchpad_webhook(body)
        elif request.headers["X-Launchpad-Event-Type"] == "git:push:0.1":
            changes = get_git_changes_from_launchpad_webhook(body)
        else:
            return
    else:
        raise web.HTTPBadRequest(text="Unrecognized webhook")

    async with db.acquire() as conn:
        for change in changes:
            for row in await get_codebases_by_change(conn, change):
                if change.after_revision_id() is not None:
                    await conn.execute(
                        "UPDATE codebase SET "
                        "vcs_last_revision = $1, last_scanned = NOW() "
                        "WHERE branch_url = $2",
                        change.after_revision_id().decode("utf-8"),
                        row["branch_url"],
                    )
                yield row


async def get_codebases(runner_url):
    async with (
        ClientSession() as session,
        session.get(URL(runner_url) / "codebases", raise_for_status=True) as resp,
    ):
        return await resp.json()


def main(argv=None):
    import argparse
    import asyncio
    import logging

    import breezy.bzr  # noqa: F401
    import breezy.git  # noqa: F401
    from silver_platter import (
        BranchMissing,
        BranchUnavailable,
    )
    from silver_platter import (
        _open_branch as open_branch,
    )

    parser = argparse.ArgumentParser(
        prog="janitor.site.webhook",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument("--runner-url", type=str)
    parser.add_argument("callback_url", type=str)
    args = parser.parse_args(argv)

    logging.basicConfig(level=logging.INFO, format="%(message)s")

    codebases = asyncio.run(get_codebases(args.runner_url))

    for codebase in codebases:
        if not codebase["branch_url"]:
            continue
        try:
            b = open_branch(codebase["branch_url"])
        except (BranchUnavailable, BranchMissing):
            continue
        try:
            present = subscribe_webhook(b, args.callback_url)
        except UnsupportedForge:
            logging.warning("Ignoring branch with unknown forge: %s", b.user_url)
            continue
        if present:
            logging.info("Registered webhook for %s", codebase["name"])


if __name__ == "__main__":
    import sys

    sys.exit(main(sys.argv[1:]))
