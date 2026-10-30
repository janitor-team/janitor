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

import os
import tempfile

import pytest

from janitor.logs import get_log_manager


async def test_get_log():
    with tempfile.TemporaryDirectory() as td:
        os.makedirs(os.path.join(td, "mypkg", "run-id"))
        with open(os.path.join(td, "mypkg", "run-id", "build.log"), "wb") as f:
            f.write(b"foo bar\n")
        async with get_log_manager(td) as lm:
            logf = await lm.get_log("mypkg", "run-id", "build.log")
            assert await logf.read() == b"foo bar\n"
            with pytest.raises(FileNotFoundError):
                await lm.get_log("mypkg", "run-id", "foo.log")


async def test_invalid_identifiers_rejected():
    with tempfile.TemporaryDirectory() as td:
        async with get_log_manager(td) as lm:
            with pytest.raises(ValueError):
                await lm.get_log("../etc", "run-id", "foo.log")
            with pytest.raises(ValueError):
                await lm.get_log("mypkg", "run/../id", "foo.log")
            with pytest.raises(ValueError):
                await lm.get_log("mypkg", "run-id", "foo/log")
