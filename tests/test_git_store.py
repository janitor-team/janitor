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
import sys

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


# Forks a child that outlives it, then blocks. Stands in for git http-backend,
# which also forks children that inherit this process's stdout and stderr.
_FORKING_BACKEND = (
    "import os,sys,time\n"
    "pid=os.fork()\n"
    "if pid==0:\n"
    "    time.sleep(120)\n"
    "    sys.exit(0)\n"
    "open(sys.argv[1]+'.tmp','w').write(str(pid))\n"
    "os.rename(sys.argv[1]+'.tmp',sys.argv[1])\n"
    "time.sleep(120)\n"
)


def _alive(pid):
    try:
        os.kill(pid, 0)
    except (ProcessLookupError, PermissionError):
        return False
    return True


@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs fork")
async def test_cgit_backend_reaps_forked_children_on_handler_cancel(
    aiohttp_client, tmp_path, monkeypatch
):
    """A client going away must not leave the backend's own children behind.

    git http-backend forks children that inherit the stdout and stderr pipes
    created for it. Signalling only the direct child leaves them running,
    reparented to init, where nothing reaps them.

    aiohttp's test server sets handler_cancellation=True, so the reset below
    cancels the handler rather than raising ConnectionResetError at it. That is
    the path this change removes an explicit handler from, so it is worth
    pinning. The backend is stubbed with a process of the same shape, because
    real git http-backend exits too promptly on a partial request to leak
    anything here.
    """
    import janitor.git_store as git_store

    childfile = tmp_path / "grandchild.pid"
    real_exec = asyncio.create_subprocess_exec
    spawned = []

    async def fake_exec(*args, **kwargs):
        p = await real_exec(
            sys.executable, "-c", _FORKING_BACKEND, str(childfile), **kwargs
        )
        spawned.append(p.pid)
        return p

    monkeypatch.setattr(git_store.asyncio, "create_subprocess_exec", fake_exec)

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
        for _ in range(100):
            if childfile.exists():
                break
            await asyncio.sleep(0.05)
        # SO_LINGER with a zero timeout makes close() send RST rather than FIN.
        s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    finally:
        s.close()

    assert spawned, "cgit_backend never spawned a backend"
    assert childfile.exists(), "stub backend never forked"
    grandchild = int(childfile.read_text())

    try:
        for _ in range(150):
            if not _alive(grandchild):
                break
            await asyncio.sleep(0.05)
        assert not _alive(grandchild), (
            f"backend's forked child {grandchild} survived the request"
        )
    finally:
        for pid in [grandchild, *spawned]:
            try:
                os.kill(pid, 9)
            except (ProcessLookupError, PermissionError):
                pass
