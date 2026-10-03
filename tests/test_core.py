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


from janitor import MAX_RETRY_AFTER, retry_after_seconds, splitout_env


def test_splitout_env():
    assert splitout_env("ls") == ({}, "ls")
    assert splitout_env("PATH=/bin ls") == ({"PATH": "/bin"}, "ls")
    assert splitout_env("PATH=/bin FOO=bar ls") == (
        {"PATH": "/bin", "FOO": "bar"},
        "ls",
    )
    assert splitout_env("PATH=/bin FOO=bar ls -l") == (
        {"PATH": "/bin", "FOO": "bar"},
        "ls -l",
    )


def test_retry_after_seconds():
    assert retry_after_seconds(None) is None
    assert retry_after_seconds(0) is None
    assert retry_after_seconds(-5) is None
    assert retry_after_seconds(float("nan")) is None
    assert retry_after_seconds(float("inf")) is None
    assert retry_after_seconds(0.2) == 1
    assert retry_after_seconds(41.2) == 42
    assert retry_after_seconds(MAX_RETRY_AFTER) == MAX_RETRY_AFTER
    assert retry_after_seconds(1e12) == MAX_RETRY_AFTER
