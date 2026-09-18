#!/usr/bin/python
# Copyright (C) 2019 Jelmer Vernooij <jelmer@jelmer.uk>
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

from datetime import datetime
from typing import Optional

from aiohttp import ClientConnectorError, web
from jinja2 import PackageLoader
from yarl import URL

from janitor.vcs import RemoteBzrVcsManager, RemoteGitVcsManager

BUG_ERROR_RESULT_CODES = [
    "worker-failure",
    "worker-exception",
    "worker-clone-incomplete-read",
    "worker-clone-malformed-transform",
    "chroot-not-found",
    "worker-killed",
]


TRANSIENT_ERROR_RESULT_CODES = [
    "cancelled",
    "aborted",
    "install-deps-file-fetch-failure",
    "apt-get-update-file-fetch-failure",
    "build-failed-stage-apt-get-update",
    "build-failed-stage-apt-get-dist-upgrade",
    "build-failed-stage-explain-bd-uninstallable",
    "502-bad-gateway",
    "worker-502-bad-gateway",
    "build-failed-stage-create-session",
    "apt-get-update-missing-release-file",
    "no-space-on-device",
    "worker-killed",
    "too-many-requests",
    "check-space-insufficient-disk-space",
    "worker-resume-branch-unavailable",
    "worker-timeout",
    "worker-clone-bad-gateway",
    "worker-clone-temporary-transport-error",
    "push-failed",
    "push-bad-gateway",
    "testbed-chroot-disappeared",
    "apt-file-fetch-failure",
    "pull-rate-limited",
    "session-setup-failure",
    "run-disappeared",
    "branch-temporarily-unavailable",
]


def json_chart_data(max_age=None):
    if max_age is not None:
        headers = {"Cache-Control": f"max-age={max_age}"}
    else:
        headers = {}

    def decorator(fn):
        async def handle(request):
            async with request.app.database.acquire() as conn:
                return web.json_response(await fn(request, conn), headers=headers)

        return handle

    return decorator


def update_vars_from_request(vs, request):
    vs["is_admin"] = is_admin(request)
    vs["is_qa_reviewer"] = is_qa_reviewer(request)
    vs["user"] = request["user"]
    vs["rel_url"] = request.rel_url
    vs["suites"] = [c for c in request.app["config"].campaign]
    vs["campaigns"] = [c for c in request.app["config"].campaign]
    vs["openid_configured"] = bool(request.app.get("openid_config"))

    def url_for(name, **kwargs):
        return request.app.router[name].url_for(**kwargs)

    vs["url_for"] = url_for

    if request.app["external_url"] is not None:
        vs["url"] = request.app["external_url"].join(request.rel_url)
        vcs_base_url = request.app["external_url"]
    else:
        vs["url"] = request.url
        vcs_base_url = request.url.with_path("/")
    # RemoteGitVcsManager.get_repository_url() does a raw Url::join(codebase)
    # on this base - without a trailing slash, join() replaces the last path
    # segment ("git") instead of appending to it, silently dropping it from
    # every URL this manager builds.
    vs["git_vcs_manager"] = RemoteGitVcsManager(str(vcs_base_url / "git") + "/")
    vs["bzr_vcs_manager"] = RemoteBzrVcsManager(str(vcs_base_url / "bzr") + "/")
    vs["config"] = request.app["config"]


def format_duration(duration):
    weeks = duration.days // 7
    days = duration.days % 7
    if weeks > 0:
        return f"{weeks}w{days}d"
    if duration.days > 0:
        return f"{duration.days}d{duration.seconds // (60 * 60)}h"
    hours = duration.seconds // (60 * 60)
    seconds = duration.seconds % (60 * 60)
    minutes = seconds // 60
    seconds %= 60
    if hours:
        return f"{hours}h{minutes}m"
    if minutes:
        return f"{minutes}m{seconds}s"
    return f"{seconds}s"


def format_timestamp(ts):
    return ts.isoformat(timespec="minutes")


template_loader = PackageLoader("janitor.site")


def highlight_diff(diff):
    from pygments import highlight
    from pygments.formatters import HtmlFormatter
    from pygments.lexers.diff import DiffLexer

    return highlight(diff, DiffLexer(stripnl=False), HtmlFormatter())


def classify_result_code(result_code, transient: Optional[bool]):
    if result_code in ("success", "nothing-to-do", "nothing-new-to-do"):
        return result_code
    if result_code in BUG_ERROR_RESULT_CODES:
        return "bug"
    if transient is None:
        transient = result_code in TRANSIENT_ERROR_RESULT_CODES
    if transient:
        return "transient-failure"
    return "failure"


class DebdiffRetrievalError(Exception):
    """Error occurred while retrieving debdiff."""


class BuildDiffUnavailable(Exception):
    """The build diff is not available."""

    def __init__(self, unavailable_run_id) -> None:
        self.unavailable_run_id = unavailable_run_id


async def get_archive_diff(
    client, differ_url, run_id, unchanged_run_id, kind, accept=None, filter_boring=False
):
    if kind not in ("debdiff", "diffoscope"):
        raise DebdiffRetrievalError(f"invalid diff kind {kind!r}")
    url = URL(differ_url) / kind / unchanged_run_id / run_id
    params = {
        "jquery_url": "/_static/jquery.js",
    }
    # TODO(jelmer): Set css_url
    if filter_boring:
        params["filter_boring"] = "yes"
    headers = {}
    if accept:
        headers["Accept"] = ", ".join(accept) if isinstance(accept, list) else accept
    try:
        async with client.get(url, params=params, headers=headers) as resp:
            if resp.status == 200:
                return await resp.read(), resp.content_type
            elif resp.status == 404:
                raise BuildDiffUnavailable(resp.headers.get("unavailable_run_id"))
            else:
                raise DebdiffRetrievalError(
                    "Unable to get debdiff: %s" % await resp.text()
                )
    except ClientConnectorError as e:
        raise DebdiffRetrievalError(str(e)) from e


def is_admin(request: web.Request) -> bool:
    if not request["user"]:
        return False
    admin_group = request.app["config"].oauth2_provider.admin_group
    if admin_group is None:
        return True
    return admin_group in request["user"]["groups"]


def is_qa_reviewer(request: web.Request) -> bool:
    if not request["user"]:
        return False
    qa_reviewer_group = request.app["config"].oauth2_provider.qa_reviewer_group
    if qa_reviewer_group is None:
        return True
    return qa_reviewer_group in request["user"]["groups"]


def check_admin(request: web.Request) -> None:
    if not is_admin(request):
        raise web.HTTPUnauthorized()


def check_logged_in(request: web.Request) -> None:
    if not request["user"]:
        raise web.HTTPUnauthorized()


def worker_link_is_global(url):
    import ipaddress
    from urllib.parse import urlparse

    if not url:
        return False
    parsed_url = urlparse(url)
    if not parsed_url.hostname:
        return False
    try:
        return ipaddress.ip_address(parsed_url.hostname).is_global
    except ValueError:
        # Assume yes
        return True


TEMPLATE_ENV = {
    "utcnow": datetime.utcnow,
    "enumerate": enumerate,
    "format_duration": format_duration,
    "format_timestamp": format_timestamp,
    "highlight_diff": highlight_diff,
    "classify_result_code": classify_result_code,
    "URL": URL,
    "worker_link_is_global": worker_link_is_global,
}
