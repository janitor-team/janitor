"""Sphinx configuration for the Janitor documentation."""

import re
from pathlib import Path

project = "Janitor"
html_title = "Janitor documentation"

# Take the copyright and license from the web UI footer, so they are set in
# one place.
_footer = Path(__file__).parent.parent / "py/janitor/site/templates/footer.html"
_text = _footer.read_text(encoding="utf-8")
_m = re.search(r"©\s*(\S+)\s*<a [^>]*>([^<]+)</a>", _text)
_license = re.search(r"Published under [^<.]+\.", _text)
if _m is None or _license is None:
    raise RuntimeError(f"no copyright or license line found in {_footer}")
copyright = f"{_m.group(1)} {_m.group(2)}"
html_context = {"license": _license.group(0)}

extensions = ["myst_parser"]
# Lets pages link to a section, e.g. containers.md#custom-worker-tooling.
myst_heading_anchors = 3

html_theme = "alabaster"
# Pages narrower than page_width switch to the one-column layout.
html_theme_options = {
    "page_width": "1000px",
    "sidebar_width": "240px",
    "body_max_width": "none",
    "show_relbars": True,
    "footer_text": "#666",
    "show_powered_by": False,
    "description": "Automated code changes across many repositories",
    "extra_nav_links": {"Source code": "https://github.com/janitor-team/janitor"},
}
# No "Related Topics" box: the relbars already link the previous and next page.
html_sidebars = {"**": ["about.html", "searchfield.html", "navigation.html"]}
templates_path = ["_templates"]
html_static_path = ["_static"]

# Some pages start at a second-level heading.
suppress_warnings = ["myst.header"]
