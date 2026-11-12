#!/usr/bin/python
# Copyright (C) 2022 Jelmer Vernooij <jelmer@jelmer.uk>
#
# This program is free software; you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation; either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.


from io import BytesIO, StringIO

import pytest

from janitor.config import get_campaign_config, get_distribution, read_config


def test_config():
    c = read_config(
        BytesIO(
            b"""\
logs_location: 'https://s3.nl-ams.scw.cloud'
"""
        )
    )
    assert c.logs_location == "https://s3.nl-ams.scw.cloud"


def test_distribution():
    c = read_config(
        BytesIO(
            b"""\
distribution {
  name: "unstable"
  archive_mirror_uri: "http://deb.debian.org/debian"
  component: "main"
  chroot: "unstable-amd64-sbuild"
  lintian_profile: "debian"
  lintian_suppress_tag: "bad-distribution-in-changes-file"
  lintian_suppress_tag: "no-nmu-in-changelog"
  lintian_suppress_tag: "source-nmu-has-incorrect-version-number"
  lintian_suppress_tag: "changelog-distribution-does-not-match-changes-file"
  lintian_suppress_tag: "distribution-and-changes-mismatch"
  build_command: "sbuild -Asv"
  vendor: "debian"
}
"""
        )
    )
    with pytest.raises(KeyError):
        get_distribution(c, "foo")
    assert get_distribution(c, "unstable").name == "unstable"


def test_campaign_config():
    c = read_config(
        BytesIO(
            b"""\
campaign {
  name: "lintian-fixes"
  command: "lintian-brush"
  branch_name: "lintian-fixes"
  debian_build {
    build_distribution: "lintian-fixes"
    build_suffix: "jan+lint"
    base_distribution: "unstable"
    build_command: "sbuild -Asv"
  }
  bugtracker {
    kind: debian
    url: "https://bugs.debian.org/lintian-brush"
    name: "lintian-brush"
  }
}

campaign {
  name: "unchanged"
  command: "true"
  branch_name: "master"
  debian_build {
    build_distribution: "unchanged"
    build_suffix: "jan+unchanged"
    base_distribution: "unstable"
  }
}
"""
        )
    )

    with pytest.raises(KeyError):
        get_campaign_config(c, "foo")
    cc = get_campaign_config(c, "lintian-fixes")
    assert cc.name == "lintian-fixes"
    assert cc.command == "lintian-brush"


def test_read_from_text():
    c = read_config(
        StringIO(
            """\
logs_location: 'https://s3.nl-ams.scw.cloud'
"""
        )
    )
    assert c.logs_location == "https://s3.nl-ams.scw.cloud"
