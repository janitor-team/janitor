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


import asyncio
import os
import socket
import struct
from contextlib import suppress

import pytest
from dulwich.repo import Repo

from janitor.config import read_string as read_config_string
from janitor.git_store import create_web_app

try:
    from dulwich.test_utils import build_commit_graph  # type: ignore
except ImportError:
    from dulwich.tests.utils import build_commit_graph  # type: ignore


async def create_client(aiohttp_client, path, dulwich_server=False):
    config = read_config_string("")
    app, public_app = await create_web_app(
        "127.0.0.1",
        80,
        path,
        None,
        config,
        dulwich_server=dulwich_server,
    )
    return (await aiohttp_client(app), await aiohttp_client(public_app))


async def test_health(aiohttp_client):
    client, public_client = await create_client(aiohttp_client, "/tmp")

    resp = await client.get("/health")
    assert resp.status == 200
    text = await resp.text()
    assert text == "ok"


async def test_ready(aiohttp_client):
    client, public_client = await create_client(aiohttp_client, "/tmp")

    resp = await client.get("/ready")
    assert resp.status == 200
    text = await resp.text()
    assert text == "ok"


async def test_diff_nonexistent(aiohttp_client, tmp_path):
    client, public_client = await create_client(aiohttp_client, tmp_path)

    resp = await client.get("/codebase/diff?old=oldrev&new=newrev")
    assert resp.status == 503
    text = await resp.text()
    assert text == "Local VCS repository for codebase temporarily inaccessible"


async def test_diff(aiohttp_client, tmp_path):
    client, public_client = await create_client(aiohttp_client, tmp_path)

    r = Repo.init_bare(str(tmp_path / "codebase"), mkdir=True)

    c1, c2 = build_commit_graph(r.object_store, [[1], [2, 1]])

    resp = await client.get(f"/codebase/diff?old={c1.id.decode()}&new={c2.id.decode()}")
    assert resp.status == 200, await resp.text()
    text = await resp.text()
    assert text == ""


def _backend_pids(root):
    # Running processes that were started with root in their environment.
    needle = os.fsencode(str(root))
    pids = []
    for name in filter(str.isdigit, os.listdir("/proc")):
        with suppress(OSError), open(f"/proc/{name}/environ", "rb") as f:
            if needle in f.read():
                pids.append(int(name))
    return pids


@pytest.mark.skipif(not os.path.exists("/proc/self/environ"), reason="needs /proc")
async def test_cgit_backend_reaps_forked_children_on_handler_cancel(
    aiohttp_client, tmp_path
):
    """A client going away must not leave the backend's own children behind.

    git http-backend forks children that inherit the stdout and stderr pipes
    created for it. Signalling only the direct child leaves them running,
    reparented to init, where nothing reaps them.

    aiohttp's test server sets handler_cancellation=True, so the reset below
    cancels the handler rather than raising ConnectionResetError at it. That is
    the path this change removes an explicit handler from, so it is worth
    pinning. The request body stops short of its Content-Length, so the real
    git http-backend and the upload-pack it forked are both still waiting for
    the rest when the client goes away.
    """
    client, public_client = await create_client(aiohttp_client, tmp_path)
    Repo.init_bare(str(tmp_path / "codebase"), mkdir=True)

    server = client.server
    s = socket.create_connection((server.host, server.port))
    try:
        s.sendall(
            b"POST /codebase/git-upload-pack HTTP/1.1\r\n"
            b"Host: localhost\r\n"
            b"Content-Type: application/x-git-upload-pack-request\r\n"
            b"Content-Length: 500\r\n"
            b"\r\n" + b"0" * 20
        )
        # The direct child and at least one process it forked.
        for _ in range(100):
            if len(_backend_pids(tmp_path)) >= 2:
                break
            await asyncio.sleep(0.05)
        spawned = _backend_pids(tmp_path)
        # SO_LINGER with a zero timeout makes close() send RST rather than FIN.
        s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    finally:
        s.close()

    try:
        assert len(spawned) >= 2, f"git http-backend never forked: {spawned}"
        for _ in range(150):
            if not _backend_pids(tmp_path):
                break
            await asyncio.sleep(0.05)
        left = _backend_pids(tmp_path)
        assert not left, f"backend processes {left} survived the request"
    finally:
        for pid in _backend_pids(tmp_path):
            with suppress(ProcessLookupError):
                os.kill(pid, 9)
