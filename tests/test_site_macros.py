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


def test_display_reviews_table_comments():
    template = env.get_template("review_util.html")
    reviews = [
        {"reviewer": "other@example.com", "verdict": "rejected", "comment": "not now"},
        {"reviewer": "me@example.com", "verdict": "rejected", "comment": "too big"},
    ]
    soup = BeautifulSoup(
        str(
            template.module.display_reviews_table(  # type: ignore
                reviews, "me@example.com"
            )
        ),
        "html.parser",
    )
    comments = [
        tr.find_all("td", recursive=False)[2].get_text(strip=True)
        for tr in soup.find_all("tr")[1:]
    ]
    assert comments == ["not now", "too big"]


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
