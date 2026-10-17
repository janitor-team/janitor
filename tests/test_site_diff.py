#!/usr/bin/python3

import time
from html.parser import HTMLParser

import pytest
from pygments import highlight
from pygments.formatters import HtmlFormatter
from pygments.lexers.diff import DiffLexer

from janitor.site import highlight_diff

_WS_CLASS = "gi gi-TrailingWhitespace"
_WS = f'<span class="{_WS_CLASS}">{{}}</span>'


def _plain(diff):
    return highlight(diff, DiffLexer(stripnl=False), HtmlFormatter())


def test_trailing_whitespace_on_added_lines():
    diff = (
        "--- a/debian/control \n"
        "+++ b/debian/control \n"
        "@@ -1,2 +1,4 @@\n"
        " Section: misc \n"
        "-Priority: extra \n"
        "+Priority: optional \n"
        "+Depends: a,\t \n"
        "+ \t\n"
        "+Suggests: b\n"
    )
    assert highlight_diff(diff) == (
        '<div class="highlight"><pre><span></span>'
        '<span class="gd">--- a/debian/control </span>\n'
        '<span class="gi">+++ b/debian/control </span>\n'
        '<span class="gu">@@ -1,2 +1,4 @@</span>\n'
        '<span class="w"> </span>Section: misc \n'
        '<span class="gd">-Priority: extra </span>\n'
        '<span class="gi">+Priority: optional</span>' + _WS.format(" ") + "\n"
        '<span class="gi">+Depends: a,</span>' + _WS.format("\t ") + "\n"
        '<span class="gi">+</span>' + _WS.format(" \t") + "\n"
        '<span class="gi">+Suggests: b</span>\n'
        "</pre></div>\n"
    )


@pytest.mark.parametrize(
    "diff",
    [
        "",
        "+\n",
        "+a\n",
        "+ a\tb\n",
        "+a",
        "--- a/x \n+++ b/x \n",
        "-a \n a \n!a \n> a \n< a \n@@ a \n=== a \nIndex: a \ndiff a \na \n",
        "+a\r\n",
    ],
)
def test_no_trailing_whitespace_is_unchanged(diff):
    assert highlight_diff(diff) == _plain(diff)


@pytest.mark.parametrize(
    "diff, marked",
    [
        ("+a ", [" "]),
        ("+a \r\n+b\t\r\n", [" ", "\t"]),
        ("+++ b/x \n", [" "]),
        ("--- a/x\n@@ -1 +1 @@\n+++ b/x \n", [" "]),
        ("--- a/x\n+++ b/x \n+++ b/x \n", [" "]),
        ("---\n+++ b/x \n", [" "]),
        ("--- a/x\n+a \n+++ b \n", [" ", " "]),
        ("+a \n" * 3, [" "] * 3),
        ("+a\xa0\u3000 \n", [" "]),
    ],
)
def test_marked_whitespace(diff, marked):
    html = highlight_diff(diff)
    assert html.count(f'<span class="{_WS_CLASS}">') == len(marked)
    for part in marked:
        assert _WS.format(part) in html


class _Collector(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.classes: set[str] = set()
        self.open: list[str] = []
        self.text = ""

    def handle_starttag(self, tag, attrs):
        if tag == "span":
            assert [k for k, v in attrs] in ([], ["class"])
            self.classes.update(v for k, v in attrs)
        else:
            assert (tag, attrs) in [("div", [("class", "highlight")]), ("pre", [])]
        self.open.append(tag)

    def handle_endtag(self, tag):
        assert self.open.pop() == tag

    def handle_data(self, data):
        self.text += data


@pytest.mark.parametrize(
    "line",
    [
        "<script>alert(1)</script> ",
        '"><img src=x onerror=alert(1)> \t',
        "&lt;b&gt; &amp; &#x27; &quot; ",
        "</span></pre></div><script>alert(1)</script>\t",
        " <b> ",
        "' onmouseover='alert(1) ",
        pytest.param("x" * 100000 + " " * 100000, id="long"),
    ],
)
def test_untrusted_diff(line):
    for prefix in ["+", "++", "+++ ", "--- a\n+++ ", "-", " ", "@@ ", "=== ", ""]:
        diff = prefix + line + "\n"
        html = highlight_diff(diff)
        assert "<script" not in html and "<img" not in html and "<b>" not in html
        parser = _Collector()
        parser.feed(html)
        parser.close()
        assert parser.open == []
        assert parser.text == diff + "\n"
        assert parser.classes <= {"gi", _WS_CLASS, "gd", "gu", "gh", "w"}
        assert (_WS_CLASS in parser.classes) == (prefix in ("+", "++", "+++ "))


def test_trailing_whitespace_is_linear():
    start = time.monotonic()
    highlight_diff("+" + " " * 1000000 + "\n")
    highlight_diff("+" + " x" * 500000 + "\n")
    highlight_diff("+ \n" * 300000)
    highlight_diff("+" + " " * 1000000 + "x\n")
    assert time.monotonic() - start < 20
