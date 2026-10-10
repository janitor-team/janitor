#!/usr/bin/python
# Copyright (C) 2026 Jelmer Vernooij <jelmer@jelmer.uk>
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

from io import BytesIO

from janitor.reprocess_logs import process_dist_log


def test_process_dist_log_takes_a_file_object():
    # logfile_manager.get_log returns gzip.GzipFile, open(..., "rb") or
    # BytesIO, never bytes, so the binding has to accept a reader.
    code, description, phase, failure_details = process_dist_log(
        BytesIO(b"checking for foo... no\nconfigure: error: foo not found\n")
    )
    assert code == "dist-missing-vague-dependency"
    assert description


def test_failure_details_is_a_mapping_not_a_string():
    # The run.failure_details column is json and the pool installs a
    # json.dumps codec, so a str here would be stored double-encoded and would
    # also defeat the change-detection guard in reprocess_run_logs.
    _code, _description, _phase, failure_details = process_dist_log(
        BytesIO(b"configure: error: foo not found\n")
    )
    assert isinstance(failure_details, dict)
    assert failure_details["name"] == "foo"


def test_read_error_is_raised_not_swallowed():
    class Broken(BytesIO):
        def read(self, *args):
            raise OSError("truncated stream")

    # An unreadable log must not analyse as an empty one, or the handler would
    # write a bogus result_code over the real one.
    try:
        process_dist_log(Broken(b""))
    except OSError:
        pass
    else:
        raise AssertionError("a read error must propagate")


def test_invalid_utf8_is_replaced_not_fatal():
    # Build logs carry raw bytes, and BufRead::lines() returns InvalidData for
    # them, which the unwrap on the collect turned into a panic.
    code, _description, _phase, failure_details = process_dist_log(
        BytesIO(b"gcc: \xff\xfe: No such file or directory\nconfigure: error: foo not found\n")
    )
    assert code == "dist-missing-vague-dependency"
    assert failure_details["name"] == "foo"


def test_invalid_utf8_on_the_matched_line_keeps_the_replacement():
    # The replacement character lands in the extracted name rather than
    # stopping the analysis.
    _code, _description, _phase, failure_details = process_dist_log(
        BytesIO(b"configure: error: caf\xe9 not found\n")
    )
    assert failure_details["name"] == "caf�"
