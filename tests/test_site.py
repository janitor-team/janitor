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

import re
import time
import tracemalloc
from datetime import datetime, timedelta
from html.parser import HTMLParser
from typing import Optional
from unittest.mock import MagicMock

import pytest
from pygments import highlight
from pygments.formatters import HtmlFormatter
from pygments.lexers.diff import DiffLexer
from yarl import URL

from janitor.site import (
    format_duration,
    format_timestamp,
    highlight_diff,
    linkify_html,
    update_vars_from_request,
)


def test_duration():
    assert "10s" == format_duration(timedelta(seconds=10))
    assert "1m10s" == format_duration(timedelta(seconds=70))
    assert "1h0m" == format_duration(timedelta(hours=1))
    assert "1d1h" == format_duration(timedelta(days=1, hours=1))
    assert "2w1d" == format_duration(timedelta(weeks=2, days=1))


def test_timestamp():
    assert "2022-10-01T11:10" == format_timestamp(datetime(2022, 10, 1, 11, 10, 22))


def _make_request(external_url=None, request_url="https://example.com/some/path"):
    app = MagicMock()
    app.__getitem__.side_effect = {
        "external_url": external_url,
        "config": MagicMock(campaign=[]),
    }.__getitem__
    app.__contains__.side_effect = lambda k: k in {"config", "external_url"}
    request = MagicMock()
    request.__getitem__.side_effect = {"user": None}.__getitem__
    request.rel_url = URL(request_url).relative()
    request.url = URL(request_url)
    request.app = app
    return request


def test_update_vars_from_request_manager_urls_include_vcs_prefix():
    # Regression test for #1253: the VCS manager base URLs supplied to the
    # template context must produce repository URLs that keep the "/git/" and
    # "/bzr/" path prefix, not drop them via Url::join replacing the last
    # segment.
    vs: dict = {}
    update_vars_from_request(vs, _make_request())
    assert (
        vs["git_vcs_manager"].get_repository_url("mycb")
        == "https://example.com/git/mycb"
    )
    assert (
        vs["bzr_vcs_manager"].get_repository_url("mycb")
        == "https://example.com/bzr/mycb"
    )


def test_update_vars_from_request_uses_external_url():
    vs: dict = {}
    update_vars_from_request(
        vs, _make_request(external_url=URL("https://public.example.org/"))
    )
    assert (
        vs["git_vcs_manager"].get_repository_url("mycb")
        == "https://public.example.org/git/mycb"
    )
    assert (
        vs["bzr_vcs_manager"].get_repository_url("mycb")
        == "https://public.example.org/bzr/mycb"
    )


def _a(url):
    return f'<a href="{url}" rel="nofollow noopener">{url}</a>'


_EX = "https://example.org/"
_NOT_URLS = "ftp://example.org/ xhttps://example.org/ //example.org https:// http://."


@pytest.mark.parametrize(
    "html, expected",
    [
        ("+Homepage: https://example.org/foo.\n", f"+Homepage: {_a(_EX + 'foo')}.\n"),
        (
            "(see http://example.org/a), [https://example.org/b].",
            f"(see {_a('http://example.org/a')}), [{_a(_EX + 'b')}].",
        ),
        (_EX + "?a=1&amp;b=2&amp;c", _a(_EX + "?a=1&amp;b=2&amp;c")),
        (_EX + "?a=1&amp; x", _a(_EX + "?a=1") + "&amp; x"),
        (_EX + "a " + _EX + "b", _a(_EX + "a") + " " + _a(_EX + "b")),
        (f"&lt;{_EX}a&gt;", f"&lt;{_a(_EX + 'a')}&gt;"),
        # A literal "&quot;" or "&#x27;" in the source text stays text.
        (_EX + "&amp;quot;&amp;#x27;x", _a(_EX + "&amp;quot;&amp;#x27;x")),
        (
            "https://bücher.example/ü x",
            _a("https://bücher.example/ü") + " x",
        ),
        ("https://xn--bcher-kva.example/", _a("https://xn--bcher-kva.example/")),
        pytest.param(_EX + "a" * 100000, _a(_EX + "a" * 1989) + "a" * 98011, id="long"),
        # Quotes, escaped or not, end the URL.
        (_EX + '"onmouseover="alert(1)', _a(_EX) + '"onmouseover="alert(1)'),
        (_EX + "'onclick='x", _a(_EX) + "'onclick='x"),
        (_EX + "&quot;onmouseover=&quot;x", _a(_EX) + "&quot;onmouseover=&quot;x"),
        (_EX + "&#39;&gt;&lt;img src=x&gt;", _a(_EX) + "&#39;&gt;&lt;img src=x&gt;"),
        (_EX + "&lt;script&gt;alert(1)", _a(_EX) + "&lt;script&gt;alert(1)"),
        (_EX + "`x` " + _EX + "\x00x", _a(_EX) + "`x` " + _a(_EX) + "\x00x"),
        ("&amp;lt;" + _EX + "a&amp;gt;", "&amp;lt;" + _a(_EX + "a&amp;gt") + ";"),
        (_EX + "a\nb", _a(_EX + "a") + "\nb"),
        (_EX + "a\tb", _a(_EX + "a") + "\tb"),
        (_EX + "a b", _a(_EX + "a") + " b"),
        (_EX + "a\x7fb", _a(_EX + "a") + "\x7fb"),
        (_EX + "a&amp", _a(_EX + "a") + "&amp"),
        (_EX + "a&lt;b", _a(_EX + "a") + "&lt;b"),
        ("&gt;" + _EX + "a&gt;", "&gt;" + _a(_EX + "a") + "&gt;"),
        ("[t](" + _EX + "a_(b))", "[t](" + _a(_EX + "a_(b") + "))"),
        # Markup is left alone.
        pytest.param("<" * 100000, "<" * 100000, id="tag-starts"),
        (
            f'<span class="gi">+{_EX}x</span>',
            f'<span class="gi">+{_a(_EX + "x")}</span>',
        ),
        (f'<span title="{_EX}x">y</span>', f'<span title="{_EX}x">y</span>'),
        # Not a http(s) URL.
        ("javascript:alert(1)", "javascript:alert(1)"),
        ("data:text/html,x www.example.org", "data:text/html,x www.example.org"),
        (_NOT_URLS, _NOT_URLS),
        ("HTTPS://EXAMPLE.ORG/", "HTTPS://EXAMPLE.ORG/"),
        ("\u00e9" + _EX + " _" + _EX, "\u00e9" + _EX + " _" + _EX),
    ],
)
def test_linkify_html(html, expected):
    assert linkify_html(html) == expected


def test_linkify_html_is_linear():
    start = time.monotonic()
    for part in ["h", "http", "https://", "https://.", "https://a&amp;", "https://a)"]:
        linkify_html(part * 20000)
        linkify_html(part + part[-1] * 100000)
    assert time.monotonic() - start < 5


def test_linkify_html_memory():
    tracemalloc.start()
    try:
        linkify_html(_EX + "a" * 2000000)
        peak = tracemalloc.get_traced_memory()[1]
    finally:
        tracemalloc.stop()
    assert peak < 50 * 1024 * 1024


class _Collector(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.tags: list[tuple[str, dict[str, Optional[str]]]] = []
        self.open: list[str] = []
        self.text = ""

    def handle_starttag(self, tag, attrs):
        assert len(attrs) == len(dict(attrs))
        self.tags.append((tag, dict(attrs)))
        self.open.append(tag)

    def handle_endtag(self, tag):
        assert self.open.pop() == tag

    def handle_data(self, data):
        self.text += data


def _check_highlighted(diff):
    """Check that the markup is only what is expected; return the link targets."""
    html = highlight_diff(diff)
    plain = re.sub('<a href="[^"]*" rel="nofollow noopener">|</a>', "", html)
    assert plain == highlight(diff, DiffLexer(stripnl=False), HtmlFormatter())
    parser = _Collector()
    parser.feed(html)
    parser.close()
    assert parser.open == []
    assert parser.text == diff + "\n"
    hrefs = []
    for tag, attrs in parser.tags:
        if tag == "a":
            assert set(attrs) == {"href", "rel"}
            assert attrs["rel"] == "nofollow noopener"
            href = attrs["href"]
            assert href and href.startswith(("http://", "https://"))
            hrefs.append(href)
        elif tag == "span":
            assert set(attrs) <= {"class"}
        else:
            assert (tag, attrs) in [("div", {"class": "highlight"}), ("pre", {})]
    return hrefs


_DIFF = """\
--- a/debian/control
+++ b/debian/control
@@ -1 +1 @@
-Homepage: http://example.org/foo
+Homepage: https://example.org/foo.
"""


def test_highlight_diff_without_url():
    diff = _DIFF.replace("http://", "").replace("https://", "")
    assert highlight_diff(diff) == highlight(
        diff, DiffLexer(stripnl=False), HtmlFormatter()
    )
    assert _check_highlighted(diff) == []


def test_highlight_diff_with_url():
    assert _check_highlighted(_DIFF) == ["http://example.org/foo", _EX + "foo"]


@pytest.mark.parametrize(
    "line, hrefs",
    [
        ("(https://example.org/a).", [_EX + "a"]),
        ("https://example.org/?a=1&b=2", [_EX + "?a=1&b=2"]),
        ("https://example.org/a, https://example.org/b", [_EX + "a", _EX + "b"]),
        ('https://example.org/"onmouseover="alert(1)', [_EX]),
        ("https://example.org/<script>alert(1)</script>", [_EX]),
        ("https://example.org/'><img src=x onerror=alert(1)>", [_EX]),
        ("https://example.org/&quot;&#x27;x", [_EX + "&quot;&#x27;x"]),
        ("https://example.org/`onerror=alert(1) x", [_EX]),
        ("https://bücher.example/ü", ["https://bücher.example/ü"]),
        pytest.param(_EX + "a" * 100000, [_EX + "a" * 1989], id="long"),
        ("https://example.org/</pre></div><script>alert(1)</script>", [_EX]),
        ("<https://example.org/a>.", [_EX + "a"]),
        ("https://example.org/a\nonmouseover=alert(1)", [_EX + "a"]),
        ('https://example.org/a\x00"onmouseover="alert(1)', [_EX + "a"]),
        ("&quot; https://example.org/a&#34;x", [_EX + "a&#34;x"]),
        ("javascript:alert(1)", []),
        ('<a href="javascript:alert(1)">x</a>', []),
        pytest.param("http" * 20000 + " " + "h" * 20000, [], id="fragments"),
    ],
)
def test_highlight_diff_untrusted(line, hrefs):
    for prefix in ["", *"+- !<>", "@@ ", "--- ", "+++ ", "=== ", "diff ", "Index: "]:
        assert _check_highlighted(prefix + line + "\n") == hrefs
