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

from bs4 import BeautifulSoup
from jinja2 import Environment, select_autoescape

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


def lintian_hints(lintian_result):
    template = env.get_template("lintian_util.html")
    rendered = str(
        template.module.display_lintian_result(lintian_result)  # type: ignore
    )
    soup = BeautifulSoup(rendered, "html.parser")
    hints = []
    for item in soup.find_all("li", class_="lintian-hintitem"):
        code, hint, context = item.find_all("span")
        (link,) = hint.find_all("a")
        hints.append(
            (
                code["id"],
                code.get_text(),
                link["href"],
                link.get_text(),
                context.get_text(strip=True),
            )
        )
    return hints


def test_display_lintian_result():
    tags = "https://lintian.debian.org/tags/"
    assert lintian_hints(LINTIAN_RESULT) == [
        (
            "lintian-code-error",
            "e",
            tags + "description-starts-with-package-name",
            "description-starts-with-package-name",
            "",
        ),
        (
            "lintian-code-error",
            "e",
            tags + "no-changelog",
            "no-changelog",
            "usr/share/doc/tiny/changelog.Debian.gz (non-native package)",
        ),
        (
            "lintian-code-error",
            "e",
            tags + "no-copyright-file",
            "no-copyright-file",
            "",
        ),
        (
            "lintian-code-warning",
            "w",
            tags + "executable-not-elf-or-script",
            "executable-not-elf-or-script",
            "[usr/bin/tiny]",
        ),
        (
            "lintian-code-warning",
            "w",
            tags + "no-manual-page",
            "no-manual-page",
            "[usr/bin/tiny]",
        ),
    ]
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
    tags = "https://lintian.debian.org/tags/"
    assert lintian_hints(result) == [
        (
            "lintian-code-",
            "?",
            tags + "debian-changelog-line-too-long",
            "debian-changelog-line-too-long",
            "line 1",
        ),
        ("lintian-code-", "?", tags + "no-copyright-file", "no-copyright-file", ""),
    ]
