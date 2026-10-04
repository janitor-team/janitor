#!/usr/bin/python3
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

__all__ = [
    "run_diffoscope",
]

import json
import logging
import os
import sys
from io import StringIO

from breezy.patches import (
    MalformedHunkHeader,
)

from ._differ import (
    filter_boring_udiff,
    run_diffoscope,
)


class DiffoscopeError(Exception):
    """An error occurred while running diffoscope."""


def filter_boring_detail(detail, old_version, new_version, display_version):
    if detail["unified_diff"] is not None:
        try:
            detail["unified_diff"] = filter_boring_udiff(
                detail["unified_diff"], old_version, new_version, display_version
            )
        except MalformedHunkHeader as e:
            logging.warning("Error parsing hunk: %r", e)
    detail["source1"] = detail["source1"].replace(old_version, display_version)
    detail["source2"] = detail["source2"].replace(new_version, display_version)
    if detail.get("details"):
        i = 0
        for subdetail in list(detail["details"]):
            if not filter_boring_detail(
                subdetail, old_version, new_version, display_version
            ):
                del detail["details"][i]
                continue
            i += 1
    if not detail.get("unified_diff") and not detail.get("details"):
        return False
    return True


def filter_boring(diff, old_version, new_version, old_campaign, new_campaign):
    display_version = new_version.rsplit("~", 1)[0]
    # Changes file differences
    BORING_FIELDS = ["Date", "Distribution", "Version"]
    i = 0
    for detail in list(diff["details"]):
        if detail["source1"] in BORING_FIELDS and detail["source2"] in BORING_FIELDS:
            del diff["details"][i]
            continue
        if detail["source1"].endswith(".buildinfo") and detail["source2"].endswith(
            ".buildinfo"
        ):
            del diff["details"][i]
            continue
        if not filter_boring_detail(detail, old_version, new_version, display_version):
            del diff["details"][i]
            continue
        i += 1


def filter_irrelevant(diff):
    diff["source1"] = os.path.basename(diff["source1"])
    diff["source2"] = os.path.basename(diff["source2"])


async def format_diffoscope(root_difference, content_type, title, css_url=None):
    if content_type == "application/json":
        return json.dumps(root_difference)
    from diffoscope.readers.json import JSONReaderV1

    root_difference = JSONReaderV1().load_rec(root_difference)
    if content_type == "text/html":
        from diffoscope.presenters.html.html import HTMLPresenter

        p = HTMLPresenter()
        old_stdout = sys.stdout
        sys.stdout = f = StringIO()
        old_argv = sys.argv
        sys.argv = title.split(" ")
        try:
            p.output_html("-", root_difference, css_url=css_url)
        finally:
            sys.stdout = old_stdout
            sys.argv = old_argv
        return f.getvalue()
    if content_type == "text/markdown":
        from diffoscope.presenters.markdown import MarkdownTextPresenter

        out = []

        def printfn(t=""):
            out.append(t + "\n")

        p = MarkdownTextPresenter(printfn)
        p.start(root_difference)
        return "".join(out)
    if content_type == "text/plain":
        from diffoscope.presenters.text import TextPresenter

        out = []

        def printfn(t=""):
            out.append(t + "\n")

        p = TextPresenter(printfn, False)
        p.start(root_difference)
        return "".join(out)
    raise AssertionError(f"unknown content type {content_type!r}")
