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
from datetime import datetime

import pytest

from janitor.logs import (
    FileSystemLogFileManager,
    GCSLogFileManager,
    S3LogFileManager,
    get_log_manager,
)


def test_s3_log_file_manager():
    S3LogFileManager("https://some-url/")


def test_gcs_log_file_manager():
    GCSLogFileManager("gs://foo/")


async def test_file_log_file_manager():
    with tempfile.TemporaryDirectory() as td:
        async with FileSystemLogFileManager(td) as lm:
            assert not await lm.has_log("mypkg", "run-id", "foo.log")
            with pytest.raises(FileNotFoundError):
                await lm.get_log("mypkg", "run-id", "foo.log")
            with pytest.raises(FileNotFoundError):
                await lm.get_ctime("mypkg", "run-id", "foo.log")
            with pytest.raises(FileNotFoundError):
                await lm.delete_log("mypkg", "run-id", "foo.log")
            with tempfile.NamedTemporaryFile(suffix=".log") as f:
                f.write(b"foo bar\n")
                f.flush()
                await lm.import_log("mypkg", "run-id", f.name)
                logname = os.path.basename(f.name)
            assert await lm.has_log("mypkg", "run-id", logname)
            assert (await lm.get_log("mypkg", "run-id", logname)).read() == b"foo bar\n"
            assert isinstance(await lm.get_ctime("mypkg", "run-id", logname), datetime)
            assert [x async for x in lm.iter_logs()] == [("mypkg", "run-id", [logname])]
            await lm.delete_log("mypkg", "run-id", logname)
            assert not await lm.has_log("mypkg", "run-id", logname)


async def test_get_log_manager_filesystem():
    with tempfile.TemporaryDirectory() as td:
        lm = get_log_manager(td)
        with tempfile.NamedTemporaryFile(suffix=".log") as f:
            f.write(b"foo bar\n")
            f.flush()
            await lm.import_log("mypkg", "run-id", f.name, basename="build.log")
        with await lm.get_log("mypkg", "run-id", "build.log") as logf:
            assert logf.read() == b"foo bar\n"
