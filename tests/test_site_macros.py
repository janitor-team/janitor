#!/usr/bin/python
# Copyright (C) 2022 Jelmer Vernooij <jelmer@jelmer.uk>
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
from bs4 import BeautifulSoup
from jinja2 import Environment, select_autoescape

from janitor.config import read_string
from janitor.site import template_loader
from janitor.vcs import RemoteBzrVcsManager, RemoteGitVcsManager

env = Environment(loader=template_loader, autoescape=select_autoescape(["html", "xml"]))


def test_display_branch_url():
    template = env.get_template("run_util.html")
    assert (
        str(
            template.module.display_branch_url(  # type: ignore
                None, "https://github.com/jelmer/example.git"
            )
        )
        == """\

    
        <a href="https://github.com/jelmer/example.git">https://github.com/jelmer/example.git</a>
    
"""
    )
    assert (
        str(
            template.module.display_branch_url(  # type: ignore
                "https://github.com/jelmer/example.git",
                "https://github.com/jelmer/example",
            )
        )
        == """\

    
        <a href="https://github.com/jelmer/example.git">https://github.com/jelmer/example</a>
    
"""
    )


def test_display_reviews_table():
    template = env.get_template("review_util.html")
    long_comment = (
        "Please split this into smaller changes, one for each fix, "
        "so that each one can be reviewed and landed on its own."
    )
    reviews = [
        {
            "reviewer": "other@example.com",
            "verdict": "rejected",
            "comment": long_comment,
        },
        {"reviewer": "third@example.com", "verdict": "approved", "comment": None},
        {"reviewer": "me@example.com", "verdict": "rejected", "comment": "too big"},
    ]
    soup = BeautifulSoup(
        str(
            template.make_module({"config": read_string("")}).display_reviews_table(  # type: ignore
                reviews, "me@example.com"
            )
        ),
        "html.parser",
    )
    rows = []
    for tr in soup.select("table.reviews tr"):
        row: list[object] = []
        for cell in tr.find_all(["th", "td"], recursive=False):
            select = cell.find("select")
            span = cell.find("span", title=True)
            if select is not None:
                row.append(
                    [o["value"] for o in select.find_all("option", selected=True)]
                )
            elif span is not None:
                row.append((span.get_text(), span["title"]))
            else:
                row.append(cell.get_text(strip=True))
        rows.append(row)
    assert rows == [
        ["Reviewer", "Verdict", "Comment"],
        [
            "other@example.com",
            "rejected",
            (
                "Please split this into smaller changes, one for each fix, "
                "so that each one can be reviewed and...",
                long_comment,
            ),
        ],
        ["third@example.com", "approved", ""],
        ["Your review", ["rejected"], ("too big", "too big")],
    ]


def _render_local_command(vcs, **context):
    src = (
        '{% from "run_util.html" import local_command with context %}'
        '{{ local_command("brz cmd", "mycodebase", vcs, "unstable") }}'
    )
    return env.from_string(src).render(vcs=vcs, failure_stage="validate", **context)


def test_local_command_git_url():
    # Regression test for #1253: local_command must render a git-clone URL
    # that includes the "/git/" path prefix from the base VCS URL.
    rendered = _render_local_command(
        "git",
        git_vcs_manager=RemoteGitVcsManager("https://example.com/git/"),
    )
    assert "git clone https://example.com/git/mycodebase mycodebase" in rendered


def test_local_command_bzr_url():
    # Regression test for #1253: local_command must render a bzr-branch URL
    # that includes the "/bzr/" path prefix from the base VCS URL.
    rendered = _render_local_command(
        "bzr",
        bzr_vcs_manager=RemoteBzrVcsManager("https://example.com/bzr/"),
    )
    assert (
        "bzr branch https://example.com/bzr/mycodebase/unstable mycodebase" in rendered
    )


def test_display_publish_blockers():
    template = env.get_template("run_util.html")
    assert (
        str(
            template.module.display_publish_blockers(  # type: ignore
                {}
            )
        )
        == """\

    <ul>
        
    </ul>
"""
    )
    assert (
        str(
            template.module.display_publish_blockers(  # type: ignore
                {"inactive": {"result": True, "details": {}}}
            )
        )
        == """\

    <ul>
        
            <li>☑
                codebase is not inactive</li>
        
    </ul>
"""
    )


def _blocker_html(name, result, details):
    template = env.get_template("run_util.html")
    blockers = {name: {"result": result, "details": details}}
    return BeautifulSoup(
        str(template.module.display_publish_blockers(blockers)),  # type: ignore
        "html.parser",
    ).li


def _blocker_text(name, result, details):
    return " ".join(_blocker_html(name, result, details).get_text().split())


@pytest.mark.parametrize(
    "result,details,expected",
    [
        (
            True,
            {"attempt_count": 0, "next_try_time": "2026-11-05T12:00:00Z"},
            "☑ 0 publish attempts so far, not currently attempting to back off",
        ),
        (
            False,
            {"attempt_count": 1, "next_try_time": "2026-11-05T12:00:00Z"},
            "☒ 1 publish attempts so far, next attempt after 2026-11-05T12:00:00Z",
        ),
    ],
)
def test_display_publish_blockers_backoff(result, details, expected):
    assert _blocker_text("backoff", result, details) == expected


@pytest.mark.parametrize(
    "result,details,expected",
    [
        (
            True,
            {"bucket": "jelmer@debian.org"},
            "☑ proposals not currently being rate-limited for maintainer "
            "jelmer@debian.org",
        ),
        (True, {"bucket": None}, "☑ no rate limit for merge proposals"),
        (
            False,
            {"open": 3, "max_open": 2},
            "☒ merge proposals are currently being rate-limited (open: 3, max open: 2)",
        ),
        (
            False,
            {"open": 3, "max_open": None},
            "☒ merge proposals are currently being rate-limited (open: 3)",
        ),
        (
            False,
            {"open": None, "max_open": None},
            "☒ merge proposals are currently being rate-limited",
        ),
    ],
)
def test_display_publish_blockers_rate_limit(result, details, expected):
    assert _blocker_text("propose_rate_limit", result, details) == expected


def test_display_publish_blockers_previous_mp():
    li = _blocker_html(
        "previous_mp",
        False,
        [
            {"url": "https://example.com/mp/1", "status": "rejected"},
            {"url": "https://example.com/mp/2", "status": "closed"},
        ],
    )
    assert [(a["href"], a.string) for a in li.find_all("a")] == [
        ("https://example.com/mp/1", "rejected"),
        ("https://example.com/mp/2", "closed"),
    ]


# What the worker stores for a real lintian 2.141.0 run on a small package.
LINTIAN_RESULT = {
    "groups": [
        {
            "group_id": "tiny_1.0-1",
            "input_files": [
                {
                    "hints": [
                        {
                            "experimental": False,
                            "note": "",
                            "tag": "description-starts-with-package-name",
                            "visibility": "error",
                        },
                        {
                            "experimental": False,
                            "note": "usr/share/doc/tiny/changelog.Debian.gz (non-native package)",
                            "tag": "no-changelog",
                            "visibility": "error",
                        },
                        {
                            "experimental": False,
                            "note": "",
                            "tag": "no-copyright-file",
                            "visibility": "error",
                        },
                        {
                            "experimental": False,
                            "note": "",
                            "pointer": {
                                "item": {
                                    "index": "tiny_1.0-1_all.deb (installed)",
                                    "name": "usr/bin/tiny",
                                },
                                "line_position": 0,
                            },
                            "tag": "executable-not-elf-or-script",
                            "visibility": "warning",
                        },
                        {
                            "experimental": False,
                            "note": "",
                            "pointer": {
                                "item": {
                                    "index": "tiny_1.0-1_all.deb (installed)",
                                    "name": "usr/bin/tiny",
                                },
                                "line_position": 0,
                            },
                            "tag": "no-manual-page",
                            "visibility": "warning",
                        },
                    ],
                    "path": "tiny_1.0-1_all.deb",
                }
            ],
            "source_name": "tiny",
            "source_version": "1.0-1",
        }
    ],
    "lintian_version": "2.141.0",
}


def normalize_html(html):
    return BeautifulSoup(html, "html.parser").prettify()


def render_lintian_result(lintian_result):
    template = env.get_template("lintian_util.html")
    return normalize_html(
        str(template.module.display_lintian_result(lintian_result))  # type: ignore
    )


def test_display_lintian_result():
    assert render_lintian_result(LINTIAN_RESULT) == normalize_html("""
<ul class="lintian-filelist">
  <li class="lintian-fileitem"><a href="tiny_1.0-1_all.deb">tiny_1.0-1_all.deb</a></li>
  <ul class="lintian-hintlist">
    <li class="lintian-hintitem">
      <span class="tinycode" id="lintian-code-error">e</span>
      <span class="tinyhint"><a href="https://lintian.debian.org/tags/description-starts-with-package-name">description-starts-with-package-name</a></span>
      <span class="tinycontext"></span>
    </li>
    <li class="lintian-hintitem">
      <span class="tinycode" id="lintian-code-error">e</span>
      <span class="tinyhint"><a href="https://lintian.debian.org/tags/no-changelog">no-changelog</a></span>
      <span class="tinycontext">usr/share/doc/tiny/changelog.Debian.gz (non-native package)</span>
    </li>
    <li class="lintian-hintitem">
      <span class="tinycode" id="lintian-code-error">e</span>
      <span class="tinyhint"><a href="https://lintian.debian.org/tags/no-copyright-file">no-copyright-file</a></span>
      <span class="tinycontext"></span>
    </li>
    <li class="lintian-hintitem">
      <span class="tinycode" id="lintian-code-warning">w</span>
      <span class="tinyhint"><a href="https://lintian.debian.org/tags/executable-not-elf-or-script">executable-not-elf-or-script</a></span>
      <span class="tinycontext">[usr/bin/tiny]</span>
    </li>
    <li class="lintian-hintitem">
      <span class="tinycode" id="lintian-code-warning">w</span>
      <span class="tinyhint"><a href="https://lintian.debian.org/tags/no-manual-page">no-manual-page</a></span>
      <span class="tinycontext">[usr/bin/tiny]</span>
    </li>
  </ul>
</ul>
<!-- lintian version 2.141.0 -->
""")
    template = env.get_template("lintian_util.html")
    assert str(template.module.lintian_severity_short("info")) == "i"  # type: ignore


def test_display_lintian_result_string_hints():
    # Older lintian emits each hint as a plain string.
    result = {
        "groups": [
            {
                "group_id": "x_1",
                "input_files": [
                    {
                        "hints": [
                            "debian-changelog-line-too-long line 1",
                            "no-copyright-file",
                        ],
                        "path": "x.dsc",
                    }
                ],
                "source_name": "x",
                "source_version": "1",
            }
        ],
        "lintian_version": "2.100.0",
    }
    assert render_lintian_result(result) == normalize_html("""
<ul class="lintian-filelist">
  <li class="lintian-fileitem"><a href="x.dsc">x.dsc</a></li>
  <ul class="lintian-hintlist">
    <li class="lintian-hintitem">
      <span class="tinycode" id="lintian-code-">?</span>
      <span class="tinyhint"><a href="https://lintian.debian.org/tags/debian-changelog-line-too-long">debian-changelog-line-too-long</a></span>
      <span class="tinycontext">line 1</span>
    </li>
    <li class="lintian-hintitem">
      <span class="tinycode" id="lintian-code-">?</span>
      <span class="tinyhint"><a href="https://lintian.debian.org/tags/no-copyright-file">no-copyright-file</a></span>
      <span class="tinycontext"></span>
    </li>
  </ul>
</ul>
<!-- lintian version 2.100.0 -->
""")


@pytest.mark.parametrize(
    "config, user, expected",
    [
        ("", None, ("bob@example.com", [])),
        (
            "hide_reviewer_identities: true",
            {"email": "alice@example.com"},
            ("bob@example.com", []),
        ),
        (
            "hide_reviewer_identities: true",
            None,
            (
                "Login to see reviewer identities",
                ["/login?url=https%3A//example.com/cupboard/rejected"],
            ),
        ),
    ],
)
def test_display_reviewer(config, user, expected):
    src = (
        '{% from "review_util.html" import display_reviewer with context %}'
        '{{ display_reviewer("bob@example.com") }}'
    )
    soup = BeautifulSoup(
        env.from_string(src).render(
            config=read_string(config),
            user=user,
            url="https://example.com/cupboard/rejected",
        ),
        "html.parser",
    )
    assert (soup.get_text(), [a["href"] for a in soup.find_all("a")]) == expected
