#!/usr/bin/python3
# Copyright (C) 2019 Jelmer Vernooij <jelmer@jelmer.uk>
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

"""Manage VCS repositories."""

import asyncio
import logging
import os
import sys
import warnings
from contextlib import closing, suppress
from http.client import parse_headers  # type: ignore
from io import BytesIO
from typing import Optional, cast

import aiohttp_jinja2
import aiozipkin
import asyncpg.pool
import mimeparse
from aiohttp import web
from aiohttp.web_middlewares import normalize_path_middleware
from aiohttp_openmetrics import metrics, metrics_middleware
from dulwich.errors import HangupException, MissingCommitError
from dulwich.objects import ZERO_SHA, ObjectID, valid_hexsha
from dulwich.protocol import ReceivableProtocol
from dulwich.repo import NotGitRepository, Repo
from dulwich.server import DEFAULT_HANDLERS as DULWICH_SERVICE_HANDLERS
from dulwich.server import BackendRepo, DictBackend
from dulwich.web import NO_CACHE_HEADERS, HTTPGitApplication
from jinja2 import select_autoescape

from . import state
from .config import read_config
from .site import template_loader
from .worker_creds import is_worker

GIT_BACKEND_CHUNK_SIZE = 4096


async def git_diff_request(request: web.Request) -> web.Response:
    span = aiozipkin.request_span(request)
    codebase = request.match_info["codebase"]
    try:
        old_sha = request.query["old"].encode("utf-8")
        new_sha = request.query["new"].encode("utf-8")
    except KeyError as e:
        raise web.HTTPBadRequest(text="need both old and new") from e
    path = request.query.get("path")
    repo_path = os.path.join(request.app["local_path"], codebase)

    if not os.path.isdir(repo_path):
        raise web.HTTPServiceUnavailable(
            text=f"Local VCS repository for {codebase} temporarily inaccessible"
        )

    if not valid_hexsha(old_sha) or not valid_hexsha(new_sha):
        raise web.HTTPBadRequest(text="invalid shas specified")

    args = ["git", "diff", old_sha, new_sha]
    if path:
        args.extend(["--", path])

    p = await asyncio.create_subprocess_exec(
        *args,  # type: ignore
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        stdin=asyncio.subprocess.DEVNULL,
        cwd=repo_path,
    )

    # TODO(jelmer): Stream this
    try:
        with span.new_child("subprocess:communicate"):
            (stdout, stderr) = await asyncio.wait_for(p.communicate(None), 30.0)
    except asyncio.TimeoutError as e:
        with suppress(ProcessLookupError):
            p.kill()
        raise web.HTTPRequestTimeout(text="diff generation timed out") from e
    except BaseException:
        with suppress(ProcessLookupError):
            p.kill()
        raise

    if p.returncode == 0:
        return web.Response(body=stdout, content_type="text/x-diff")
    logging.warning("git diff failed: %s", stderr.decode())
    raise web.HTTPInternalServerError(text=f"git diff failed: {stderr.decode()}")


async def git_revision_info_request(request: web.Request) -> web.Response:
    span = aiozipkin.request_span(request)
    codebase = request.match_info["codebase"]
    try:
        old_sha = request.query["old"].encode("utf-8")
        new_sha = request.query["new"].encode("utf-8")
    except KeyError as e:
        raise web.HTTPBadRequest(text="need both old and new") from e
    try:
        with span.new_child("open-repo"):
            repo = Repo(os.path.join(request.app["local_path"], codebase))
    except NotGitRepository as e:
        raise web.HTTPServiceUnavailable(
            text=f"Local VCS repository for {codebase} temporarily inaccessible"
        ) from e

    with closing(repo):
        if not valid_hexsha(old_sha) or not valid_hexsha(new_sha):
            raise web.HTTPBadRequest(text="invalid shas specified")
        ret = []
        try:
            with span.new_child("get-walker"):
                walker = repo.get_walker(
                    include=[ObjectID(new_sha)],
                    exclude=([ObjectID(old_sha)] if old_sha != ZERO_SHA else []),
                )
        except MissingCommitError:
            return web.json_response({}, status=404)
        for entry in walker:
            ret.append(
                {
                    "commit-id": entry.commit.id.decode("ascii"),
                    "revision-id": "git-v1:" + entry.commit.id.decode("ascii"),
                    "link": "/git/{}/commit/{}/".format(
                        codebase, entry.commit.id.decode("ascii")
                    ),
                    "message": entry.commit.message.decode("utf-8", "replace"),
                }
            )
            await asyncio.sleep(0)
        return web.json_response(ret)


async def _git_open_repo(local_path: str, db, codebase: str) -> Repo:
    repo_path = os.path.join(local_path, codebase)
    try:
        repo = Repo(repo_path)
    except NotGitRepository as e:
        async with db.acquire() as conn:
            if not await codebase_exists(conn, codebase):
                raise web.HTTPNotFound(text=f"no such codebase: {codebase}") from e
        repo = Repo.init_bare(repo_path, mkdir=(not os.path.isdir(repo_path)))
        logging.info("Created missing git repository for %s at %s", codebase, repo.path)
    return repo


def _git_check_service(service: str, allow_writes: bool = False) -> None:
    if service == "git-upload-pack":
        return

    if service == "git-receive-pack":
        if not allow_writes:
            raise web.HTTPUnauthorized(
                text="git-receive-pack requires login",
                headers={"WWW-Authenticate": 'Basic Realm="Janitor"'},
            )
        return

    raise web.HTTPForbidden(text=f"Unsupported service {service}")


async def handle_klaus(request: web.Request) -> web.Response:
    codebase = request.match_info["codebase"]

    span = aiozipkin.request_span(request)
    with span.new_child("open-repo"):
        repo = await _git_open_repo(
            request.app["local_path"], request.app["db"], codebase
        )

    from flask import Flask
    from klaus import KLAUS_VERSION, utils, views
    from klaus.repo import FancyRepo

    class Klaus(Flask):
        def __init__(self, codebase, repo) -> None:
            super().__init__("klaus")
            self.codebase = codebase
            self.valid_repos = {codebase: FancyRepo(repo.path, namespace=None)}

        def should_use_ctags(self, git_repo, git_commit):
            return False

        def create_jinja_environment(self):
            """Called by Flask.__init__."""
            env = super().create_jinja_environment()
            for func in [
                "force_unicode",
                "timesince",
                "shorten_sha1",
                "shorten_message",
                "extract_author_name",
                "formattimestamp",
            ]:
                env.filters[func] = getattr(utils, func)

            env.globals["KLAUS_VERSION"] = KLAUS_VERSION
            env.globals["USE_SMARTHTTP"] = False
            env.globals["SITE_NAME"] = "Codebase list"
            return env

    app = Klaus(codebase, repo)

    for endpoint, rule in [
        ("blob", "/blob/"),
        ("blob", "/blob/<rev>/<path:path>"),
        ("blame", "/blame/"),
        ("blame", "/blame/<rev>/<path:path>"),
        ("raw", "/raw/<path:path>/"),
        ("raw", "/raw/<rev>/<path:path>"),
        ("submodule", "/submodule/<rev>/"),
        ("submodule", "/submodule/<rev>/<path:path>"),
        ("commit", "/commit/<path:rev>/"),
        ("patch", "/commit/<path:rev>.diff"),
        ("patch", "/commit/<path:rev>.patch"),
        ("index", "/"),
        ("index", "/<path:rev>"),
        ("history", "/tree/<rev>/"),
        ("history", "/tree/<rev>/<path:path>"),
        ("download", "/tarball/<path:rev>/"),
        ("repo_list", "/.."),
    ]:
        app.add_url_rule(
            rule, view_func=getattr(views, endpoint), defaults={"repo": codebase}
        )

    from aiohttp_wsgi import WSGIHandler

    wsgi_handler = WSGIHandler(app)  # type: ignore

    await asyncio.sleep(0)

    return await wsgi_handler(request)


async def handle_set_git_remote(request: web.Request) -> web.Response:
    codebase = request.match_info["codebase"]
    remote = request.match_info["remote"]

    span = aiozipkin.request_span(request)
    with span.new_child("open-repo"):
        repo = await _git_open_repo(
            request.app["local_path"], request.app["db"], codebase
        )

    with closing(repo):
        post = await request.post()
        c = repo.get_config()
        section = ("remote", remote)
        c.set(section, "url", str(post["url"]))
        c.set(section, "fetch", f"+refs/heads/*:refs/remotes/{remote}/*")
        c.write_to_path()

    # TODO(jelmer): Run 'git fetch $remote'?

    return web.Response()


async def cgit_backend(request: web.Request) -> web.Response:
    codebase = request.match_info["codebase"]
    subpath = request.match_info["subpath"]
    span = aiozipkin.request_span(request)

    allow_writes = request.app["allow_writes"]
    if allow_writes is None:
        with span.new_child("is-worker"):
            allow_writes = await is_worker(request.app["db"], request)
    service = request.query.get("service")
    if service is not None:
        _git_check_service(service, allow_writes)

    with span.new_child("open-repo"):
        repo = await _git_open_repo(
            request.app["local_path"], request.app["db"], codebase
        )

    args = ["/usr/bin/git"]
    if allow_writes:
        args.extend(["-c", "http.receivepack=1"])
    args.append("http-backend")
    full_path = os.path.join(repo.path, subpath.lstrip("/"))

    repo.close()
    env: dict[str, str] = {
        "GIT_HTTP_EXPORT_ALL": "true",
        "REQUEST_METHOD": request.method,
        "CONTENT_TYPE": request.content_type,
        "PATH_TRANSLATED": full_path,
        "QUERY_STRING": request.query_string,
        # REMOTE_USER is not set
    }

    if request.remote:
        env["REMOTE_ADDR"] = request.remote

    if request.content_type is not None:
        env["CONTENT_TYPE"] = request.content_type

    for key, value in request.headers.items():
        if key.lower() == "content-encoding":
            # aiohttp already decompressed the body; http-backend would double-decode it
            continue
        env["HTTP_" + key.replace("-", "_").upper()] = value

    p = await asyncio.create_subprocess_exec(
        *args,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=env,
        stdin=asyncio.subprocess.PIPE,
    )

    assert p.stdin

    try:
        async for chunk in request.content.iter_any():
            p.stdin.write(chunk)
            await p.stdin.drain()
        p.stdin.close()
        await p.stdin.wait_closed()

        async def read_stderr(stream):
            line = await stream.readline()
            while line:
                logging.warning("git: %s", line.decode().rstrip("\n"))
                line = await stream.readline()

        async def read_stdout(stream):
            b = BytesIO()
            line = await stream.readline()
            while line != b"\r\n":
                b.write(line)
                line = await stream.readline()
            b.seek(0)
            headers = parse_headers(b)
            status = headers.get("Status")
            if status:
                del headers["Status"]
                (status_code_text, status_reason) = status.split(" ", 1)
                status_code = int(status_code_text)
                status_reason = status_reason
            else:
                status_code = 200
                status_reason = "OK"

            # Don't cross the streams
            assert p.stdin
            assert p.stdin.is_closing()
            assert p.stdout

            if "Content-Length" in headers:
                content_length = int(headers["Content-Length"])  # type: ignore
                return web.Response(
                    headers=dict(headers),  # type: ignore
                    status=status_code,
                    reason=status_reason,
                    body=await p.stdout.read(content_length),
                )  # type: ignore
            else:
                response = web.StreamResponse(
                    headers=dict(headers),  # type: ignore
                    status=status_code,
                    reason=status_reason,
                )

                if tuple(request.version) == (1, 1):
                    response.enable_chunked_encoding()

                await response.prepare(request)

                chunk = await p.stdout.read(GIT_BACKEND_CHUNK_SIZE)  # type: ignore
                while chunk:
                    try:
                        await response.write(chunk)
                    except ConnectionResetError:
                        # Client already got what it wanted and hung up -
                        # nothing left to write, nothing to report.
                        with suppress(ProcessLookupError):
                            p.kill()
                        return response
                    except BaseException:
                        with suppress(ProcessLookupError):
                            p.kill()
                        raise
                    chunk = await p.stdout.read(GIT_BACKEND_CHUNK_SIZE)  # type: ignore

                with suppress(ConnectionResetError):
                    await response.write_eof()

                return response

        with span.new_child("git-backend"):
            try:
                _stderr_reader, response = await asyncio.gather(
                    read_stderr(p.stderr),
                    read_stdout(p.stdout),
                    return_exceptions=False,
                )
            except asyncio.CancelledError:
                p.terminate()
                await p.wait()
                raise

    except BaseException:
        with suppress(ProcessLookupError):
            p.kill()
        raise

    return response


async def dulwich_refs(request: web.Request) -> web.StreamResponse:
    codebase = request.match_info["codebase"]

    allow_writes = request.app["allow_writes"]
    if allow_writes is None:
        allow_writes = await is_worker(request.app["db"], request)

    span = aiozipkin.request_span(request)
    with span.new_child("open-repo"):
        repo = await _git_open_repo(
            request.app["local_path"], request.app["db"], codebase
        )

    with closing(repo):
        service = request.query.get("service")
        if service is None:
            raise web.HTTPBadRequest(text="dumb retrieval not supported")
        _git_check_service(service, allow_writes)

        headers = {
            "Content-Type": f"application/x-{service}-advertisement",
        }
        headers.update(NO_CACHE_HEADERS)

        handler_cls = DULWICH_SERVICE_HANDLERS[service.encode("ascii")]

        response = web.StreamResponse(status=200, headers=headers)

        await response.prepare(request)

        out = BytesIO()
        proto = ReceivableProtocol(BytesIO().read, out.write)
        handler = handler_cls(
            DictBackend({".": cast(BackendRepo, repo)}),
            ["."],
            proto,
            stateless_rpc=True,
            advertise_refs=True,
        )
        handler.proto.write_pkt_line(b"# service=" + service.encode("ascii") + b"\n")
        handler.proto.write_pkt_line(None)

        await asyncio.to_thread(handler.handle)

        with suppress(ConnectionResetError):
            await response.write(out.getvalue())
            await response.write_eof()

        return response


async def dulwich_service(request: web.Request) -> web.StreamResponse:
    codebase = request.match_info["codebase"]
    service = request.match_info["service"]

    allow_writes = request.app["allow_writes"]
    if allow_writes is None:
        allow_writes = await is_worker(request.app["db"], request)

    span = aiozipkin.request_span(request)
    with span.new_child("open-repo"):
        repo = await _git_open_repo(
            request.app["local_path"], request.app["db"], codebase
        )

    with closing(repo):
        _git_check_service(service, allow_writes)

        headers = {"Content-Type": f"application/x-{service}-result"}
        headers.update(NO_CACHE_HEADERS)
        handler_cls = DULWICH_SERVICE_HANDLERS[service.encode("ascii")]

        response = web.StreamResponse(status=200, headers=headers)

        await response.prepare(request)

        inf = BytesIO(await request.read())
        outf = BytesIO()

        def handle():
            proto = ReceivableProtocol(inf.read, outf.write)
            handler = handler_cls(
                DictBackend({".": cast(BackendRepo, repo)}),
                ["."],
                proto,
                stateless_rpc=True,
            )
            try:
                handler.handle()
            except HangupException:
                response.force_close()

        await asyncio.to_thread(handle)

        with suppress(ConnectionResetError):
            await response.write(outf.getvalue())
            await response.write_eof()
        return response


async def codebase_exists(conn, codebase: str) -> bool:
    return bool(await conn.fetchrow("SELECT 1 FROM codebase WHERE name = $1", codebase))


async def handle_repo_list(request: web.Request) -> web.Response:
    span = aiozipkin.request_span(request)
    with span.new_child("list-repositories"):
        names = [
            entry.name for entry in os.scandir(os.path.join(request.app["local_path"]))
        ]
        names.sort()
    best_match = mimeparse.best_match(
        ["text/html", "text/plain", "application/json"],
        request.headers.get("Accept", "*/*"),
    )
    if best_match == "application/json":
        return web.json_response(names)
    elif best_match == "text/plain":
        return web.Response(
            text="".join([line + "\n" for line in names]), content_type="text/plain"
        )
    elif best_match == "text/html":
        return await aiohttp_jinja2.render_template_async(
            "repo-list.html", request, {"vcs": "git", "repositories": names}
        )
    raise web.HTTPNotAcceptable()


async def handle_health(request: web.Request) -> web.Response:
    return web.Response(text="ok")


async def handle_ready(request: web.Request) -> web.Response:
    return web.Response(text="ok")


async def handle_home(request: web.Request) -> web.Response:
    return web.Response(text="")


async def create_web_app(
    listen_addr: str,
    port: int,
    local_path: str,
    db: asyncpg.pool.Pool,
    config,
    *,
    dulwich_server: bool = False,
    client_max_size: Optional[int] = None,
) -> tuple[web.Application, web.Application]:
    trailing_slash_redirect = normalize_path_middleware(append_slash=True)
    app = web.Application(
        middlewares=[trailing_slash_redirect, state.asyncpg_error_middleware],
        client_max_size=(client_max_size or 0),
    )
    app["local_path"] = local_path
    app["db"] = db
    app["allow_writes"] = True
    public_app = web.Application(
        middlewares=[trailing_slash_redirect, state.asyncpg_error_middleware],
        client_max_size=(client_max_size or 0),
    )
    aiohttp_jinja2.setup(
        public_app,
        loader=template_loader,
        enable_async=True,
        autoescape=select_autoescape(["html", "xml"]),
    )
    public_app["local_path"] = local_path
    public_app["db"] = db
    public_app["allow_writes"] = None
    public_app.middlewares.insert(0, metrics_middleware)
    app.middlewares.insert(0, metrics_middleware)
    app.router.add_get("/metrics", metrics, name="metrics")
    if dulwich_server:
        app.router.add_post(
            "/{codebase}/{service:git-receive-pack|git-upload-pack}",
            dulwich_service,
            name="dulwich-service",
        )
        public_app.router.add_post(
            "/git/{codebase}/{service:git-receive-pack|git-upload-pack}",
            dulwich_service,
            name="dulwich-service-public",
        )
        app.router.add_get("/{codebase}/info/refs", dulwich_refs, name="dulwich-refs")
        public_app.router.add_get(
            "/git/{codebase}/info/refs", dulwich_refs, name="dulwich-refs-public"
        )
    else:
        for method, regex in HTTPGitApplication.services.keys():
            app.router.add_route(
                method,
                "/{codebase}{subpath:" + regex.pattern + "}",
                cgit_backend,
            )
            public_app.router.add_route(
                method,
                "/git/{codebase}{subpath:" + regex.pattern + "}",
                cgit_backend,
            )

    public_app.router.add_get("/", handle_home, name="home")
    public_app.router.add_get("/git/", handle_repo_list, name="public-repo-list")
    app.router.add_get("/", handle_repo_list, name="repo-list")
    app.router.add_get("/health", handle_health, name="health")
    app.router.add_get("/ready", handle_ready, name="ready")
    app.router.add_get("/{codebase}/diff", git_diff_request, name="git-diff")
    app.router.add_get(
        "/{codebase}/revision-info", git_revision_info_request, name="git-revision-info"
    )
    public_app.router.add_get(
        "/git/{codebase}/{path_info:.*}", handle_klaus, name="klaus"
    )
    app.router.add_post(
        "/{codebase}/remotes/{remote}", handle_set_git_remote, name="git-remote"
    )
    endpoint = aiozipkin.create_endpoint(
        "janitor.git_store", ipv4=listen_addr, port=port
    )
    if config.zipkin_address:
        tracer = await aiozipkin.create(
            config.zipkin_address, endpoint, sample_rate=0.1
        )
    else:
        tracer = await aiozipkin.create_custom(endpoint)
    aiozipkin.setup(app, tracer)
    aiozipkin.setup(public_app, tracer)
    return app, public_app


async def main_async(argv=None):
    import argparse

    parser = argparse.ArgumentParser(
        prog="janitor.git_store", formatter_class=argparse.ArgumentDefaultsHelpFormatter
    )
    parser.add_argument("--port", type=int, help="Listen port", default=9923)
    parser.add_argument(
        "--public-port",
        type=int,
        help="Public listen port for a reverse proxy",
        default=9924,
    )
    parser.add_argument(
        "--listen-address", type=str, help="Listen address", default="localhost"
    )
    parser.add_argument(
        "--config",
        type=str,
        default="janitor.conf",
        help="Path to configuration",
    )
    parser.add_argument(
        "--vcs-path", default=None, type=str, help="Path to local vcs storage"
    )
    parser.add_argument(
        "--client-max-size",
        type=int,
        default=1024**3,
        help="Maximum client body size (0 for no limit)",
    )
    parser.add_argument(
        "--dulwich-server",
        action="store_true",
        help="Use dulwich server implementation",
    )
    parser.add_argument(
        "--gcp-logging", action="store_true", help="Use Google cloud logging"
    )
    parser.add_argument("--debug", action="store_true", help="Show debug output")

    args = parser.parse_args()

    if args.gcp_logging:
        import google.cloud.logging

        client = google.cloud.logging.Client()
        client.get_default_handler()
        client.setup_logging()
    elif args.debug:
        logging.basicConfig(level=logging.DEBUG)
    else:
        logging.basicConfig(level=logging.INFO)

    if args.debug:
        loop = asyncio.get_running_loop()
        loop.set_debug(True)
        loop.slow_callback_duration = 0.001
        warnings.simplefilter("always", ResourceWarning)

    if not os.path.exists(args.vcs_path):
        parser.error(f"vcs path {args.vcs_path} does not exist")

    try:
        with open(args.config) as f:
            config = read_config(f)
    except FileNotFoundError:
        parser.error(f"config path {args.config} does not exist")

    db = await state.create_pool(config.database_location)
    app, public_app = await create_web_app(
        args.listen_address,
        args.port,
        args.vcs_path,
        db,
        config,
        dulwich_server=args.dulwich_server,
        client_max_size=args.client_max_size,
    )

    runner = web.AppRunner(app)
    public_runner = web.AppRunner(public_app)
    await runner.setup()
    await public_runner.setup()

    site = web.TCPSite(runner, args.listen_address, port=args.port)
    await site.start()
    logging.info("Admin API listening on %s:%s", args.listen_address, args.port)

    site = web.TCPSite(public_runner, args.listen_address, port=args.public_port)
    await site.start()
    logging.info(
        "Public website and API listening on %s:%s",
        args.listen_address,
        args.public_port,
    )

    while True:
        await asyncio.sleep(3600)


def main():
    import uvloop

    asyncio.set_event_loop_policy(uvloop.EventLoopPolicy())
    sys.exit(asyncio.run(main_async(sys.argv[1:])))


if __name__ == "__main__":
    main()
