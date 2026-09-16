#!/usr/bin/python
# Copyright (C) 2018 Jelmer Vernooij <jelmer@jelmer.uk>
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


import shlex
from pathlib import Path
from urllib.request import build_opener, install_opener

from dromedary.http import set_user_agent as _dromedary_set_user_agent

__version__ = (0, 1, 0)
version_string = ".".join(map(str, __version__))


_SCHEMA_DIR = Path(__file__).resolve().parent.parent.parent / "schema"


def get_core_schema() -> str:
    """Return the core janitor database schema SQL."""
    return (_SCHEMA_DIR / "state.sql").read_text()


def get_debian_schema() -> str:
    """Return the Debian-specific database schema SQL."""
    return (_SCHEMA_DIR / "debian" / "debian.sql").read_text()


def set_user_agent(user_agent):
    if user_agent is None:
        user_agent = f"janitor/{version_string}"
    _dromedary_set_user_agent(user_agent)
    opener = build_opener()
    opener.addheaders = [("User-agent", user_agent)]
    install_opener(opener)


CAMPAIGN_REGEX = "[a-z0-9-]+"


def splitout_env(command):
    args = shlex.split(command)
    env = {}
    while len(args) > 0 and "=" in args[0]:
        (key, value) = args.pop(0).split("=", 1)
        env[key] = value
    return env, shlex.join(args)
