import html
import re

from pygments.formatters import HtmlFormatter

# An http(s) URL in plain text, without a closing punctuation mark.
_URL_RE = re.compile(
    r"https?://[^\s\x00-\x1f\x7f\"'`<>]*[^\s\x00-\x1f\x7f\"'`<>.,:;!?)\]]"
)


def _link_urls(text):
    """Escape text for HTML, with its http(s) URLs as links."""
    parts = []
    pos = 0
    for m in _URL_RE.finditer(text):
        url = html.escape(m.group(0))
        parts.append(html.escape(text[pos : m.start()], quote=False))
        parts.append(f'<a href="{url}" rel="nofollow noopener">{url}</a>')
        pos = m.end()
    parts.append(html.escape(text[pos:], quote=False))
    return "".join(parts)


class LinkedUrlHtmlFormatter(HtmlFormatter):
    """HTML formatter that turns the http(s) URLs in the text into links."""

    name = "HTML with links"
    aliases = ["html-links"]
    filenames = []

    def _translate_parts(self, value):
        return [_link_urls(line) for line in value.split("\n")]
