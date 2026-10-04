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

import asyncio
import gzip
import logging
import os
from abc import ABC, abstractmethod
from datetime import datetime, timedelta
from io import BytesIO
from typing import TYPE_CHECKING, Optional

from aiohttp import (
    ClientResponseError,
    ClientSession,
    ClientTimeout,
    ServerDisconnectedError,
)
from aiohttp_openmetrics import Counter
from yarl import URL

from . import utcnow

if TYPE_CHECKING:
    import gcloud.aio.storage

primary_logfile_upload_failed_count = Counter(
    "primary_logfile_upload_failed", "Number of failed logs to primary logfile target"
)
logfile_uploaded_count = Counter("logfile_uploads", "Number of uploaded log files")


class ServiceUnavailable(Exception):
    """The remote server is temporarily unavailable."""


class LogFileManager(ABC):
    @abstractmethod
    async def has_log(self, codebase: str, run_id: str, name: str, timeout=None):
        raise NotImplementedError(self.has_log)

    @abstractmethod
    async def get_log(self, codebase: str, run_id: str, name: str, timeout=None):
        raise NotImplementedError(self.get_log)

    @abstractmethod
    async def import_log(
        self,
        codebase: str,
        run_id: str,
        orig_path: str,
        timeout=None,
        mtime=None,
        basename: Optional[str] = None,
    ):
        raise NotImplementedError(self.import_log)

    @abstractmethod
    async def iter_logs(self):
        raise NotImplementedError(self.iter_logs)

    @abstractmethod
    async def get_ctime(self, codebase: str, run_id: str, name: str) -> datetime:
        raise NotImplementedError(self.get_ctime)

    async def __aexit__(self, exc_typ, exc_val, exc_tb):
        return False

    async def __aenter__(self):
        return self


class FileSystemLogFileManager(LogFileManager):
    def __init__(self, log_directory) -> None:
        self.log_directory = log_directory

    async def __aexit__(self, exc_typ, exc_val, exc_tb):
        return False

    async def __aenter__(self):
        return self

    def _get_paths(self, codebase: str, run_id: str, name: str):
        if "/" in codebase or "/" in run_id or "/" in name:
            return []
        return [
            os.path.join(self.log_directory, codebase, run_id, name),
            os.path.join(self.log_directory, codebase, run_id, name) + ".gz",
        ]

    async def iter_logs(self):
        for codebase in os.scandir(self.log_directory):
            for entry in os.scandir(codebase.path):
                yield (
                    codebase.name,
                    entry.name,
                    [
                        n[:-3] if n.endswith(".gz") else n
                        for n in os.listdir(entry.path)
                    ],
                )

    async def has_log(
        self, codebase: str, run_id: str, name: str, timeout: Optional[timedelta] = None
    ) -> bool:
        return any(map(os.path.exists, self._get_paths(codebase, run_id, name)))

    async def get_ctime(self, codebase: str, run_id: str, name: str) -> datetime:
        for p in self._get_paths(codebase, run_id, name):
            try:
                return datetime.fromtimestamp(os.stat(p).st_ctime)
            except FileNotFoundError:
                continue
        raise FileNotFoundError(name)

    async def get_log(self, codebase, run_id, name, timeout=None):
        for path in self._get_paths(codebase, run_id, name):
            if not os.path.exists(path):
                continue
            if path.endswith(".gz"):
                return gzip.GzipFile(path, mode="rb")
            else:
                return open(path, "rb")
        raise FileNotFoundError(name)

    async def import_log(
        self,
        codebase,
        run_id,
        orig_path,
        timeout=None,
        mtime=None,
        basename: Optional[str] = None,
    ):
        dest_dir = os.path.join(self.log_directory, codebase, run_id)
        os.makedirs(dest_dir, exist_ok=True)
        with open(orig_path, "rb") as inf:
            if basename is None:
                basename = os.path.basename(orig_path)
            dest_path = os.path.join(dest_dir, basename + ".gz")
            with gzip.GzipFile(dest_path, mode="wb", mtime=mtime) as outf:
                outf.write(inf.read())

    async def delete_log(self, codebase, run_id, name):
        for path in self._get_paths(codebase, run_id, name):
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
            else:
                break
        else:
            raise FileNotFoundError(name)


class LogRetrievalError(Exception):
    """Unable to retrieve log file."""


class S3LogFileManager(LogFileManager):
    def __init__(
        self, endpoint_url, bucket_name="debian-janitor", trace_configs=None
    ) -> None:
        self.base_url = endpoint_url + (f"/{bucket_name}/")
        self.trace_configs = trace_configs
        self.bucket_name = bucket_name
        self.endpoint_url = endpoint_url

    async def __aenter__(self):
        import boto3

        self.session = ClientSession(trace_configs=self.trace_configs)
        self.s3 = boto3.resource("s3", endpoint_url=self.endpoint_url)
        self.s3_bucket = self.s3.Bucket(self.bucket_name)
        return self

    async def __aexit__(self, exc_typ, exc_val, exc_tb):
        return False

    def _get_key(self, codebase, run_id, name):
        return f"logs/{codebase}/{run_id}/{name}.gz"

    def _get_url(self, codebase, run_id, name):
        return f"{self.base_url}{self._get_key(codebase, run_id, name)}"

    async def has_log(
        self, codebase, run_id, name, timeout: Optional[timedelta] = None
    ) -> bool:
        url = self._get_url(codebase, run_id, name)
        async with self.session.head(url) as resp:
            if resp.status == 404:
                return False
            if resp.status == 200:
                return True
            if resp.status == 403:
                return False
            raise LogRetrievalError(
                "Unexpected response code {{resp.status}}: ".format()
            )

    async def get_log(
        self, codebase, run_id, name, timeout: Optional[timedelta] = None
    ):
        if timeout is None:
            timeout = timedelta(minutes=5)
        url = self._get_url(codebase, run_id, name)
        client_timeout = ClientTimeout(int(timeout.total_seconds()))
        async with self.session.get(url, timeout=client_timeout) as resp:
            if resp.status == 404:
                raise FileNotFoundError(name)
            if resp.status == 200:
                return BytesIO(gzip.decompress(await resp.read()))
            if resp.status == 403:
                raise PermissionError(await resp.text())
            raise LogRetrievalError(
                f"Unexpected response code {resp.status}: {await resp.text()}"
            )

    async def import_log(
        self,
        codebase,
        run_id,
        orig_path,
        timeout: Optional[timedelta] = None,
        mtime=None,
        basename: Optional[str] = None,
    ):
        if timeout is None:
            timeout = timedelta(minutes=5)
        with open(orig_path, "rb") as f:
            data = gzip.compress(f.read(), mtime=mtime)

        if basename is None:
            basename = os.path.basename(orig_path)
        key = self._get_key(codebase, run_id, basename)
        self.s3_bucket.put_object(Key=key, Body=data, ACL="public-read")

    async def delete_log(self, codebase, run_id, name):
        key = self._get_key(codebase, run_id, name)
        self.s3_bucket.delete_objects(Delete={"Objects": [{"Key": key}]})

    async def iter_logs(self):
        # TODO(jelmer)
        raise NotImplementedError(self.iter_logs)

    async def get_ctime(self, codebase, run_id, name):
        # TODO(jelmer)
        raise NotImplementedError(self.get_ctime)


class GCSLogFileManager(LogFileManager):
    session: "gcloud.aio.storage.storage.Session"

    def __init__(self, location, creds_path=None, trace_configs=None) -> None:
        hostname = URL(location).host
        if hostname is None:
            raise ValueError(f"invalid location missing bucket name: {location}")
        self.bucket_name = hostname
        self.trace_configs = trace_configs
        self.creds_path = creds_path

    async def __aenter__(self):
        from gcloud.aio.storage import Storage

        self.session = ClientSession(trace_configs=self.trace_configs)  # type: ignore
        self.storage = Storage(service_file=self.creds_path, session=self.session)
        self.bucket = self.storage.get_bucket(self.bucket_name)
        return self

    async def __aexit__(self, exc_typ, exc_val, exc_tb):
        return False

    async def iter_logs(self):
        seen: dict[tuple[str, str], list[str]] = {}
        for name in await self.bucket.list_blobs():
            codebase, log_id, lfn = name.split("/")
            seen.setdefault((codebase, log_id), []).append(lfn)
        for (codebase, log_id), lfns in seen.items():
            yield codebase, log_id, lfns

    def _get_object_name(self, codebase, run_id, name):
        return f"{codebase}/{run_id}/{name}.gz"

    async def has_log(
        self, codebase, run_id, name, timeout: Optional[timedelta] = None
    ) -> bool:
        object_name = self._get_object_name(codebase, run_id, name)
        return await self.bucket.blob_exists(object_name, session=self.session)

    async def get_ctime(self, codebase, run_id, name):
        from iso8601 import parse_date

        object_name = self._get_object_name(codebase, run_id, name)
        try:
            blob = await self.bucket.get_blob(object_name, session=self.session)
        except ClientResponseError as e:
            if e.status == 404:
                raise FileNotFoundError(name) from e
            raise ServiceUnavailable() from e
        except ServerDisconnectedError as e:
            raise ServiceUnavailable() from e
        return parse_date(blob.timeCreated)  # type: ignore

    async def get_log(
        self, codebase, run_id, name, timeout: Optional[timedelta] = None
    ):
        if timeout is None:
            timeout = timedelta(minutes=5)
        object_name = self._get_object_name(codebase, run_id, name)
        try:
            data = await self.storage.download(
                self.bucket_name,
                object_name,
                session=self.session,
                timeout=int(timeout.total_seconds()),
            )
            return BytesIO(gzip.decompress(data))
        except ClientResponseError as e:
            if e.status == 404:
                raise FileNotFoundError(name) from e
            raise ServiceUnavailable() from e
        except ServerDisconnectedError as e:
            raise ServiceUnavailable() from e

    async def import_log(
        self,
        codebase,
        run_id,
        orig_path,
        timeout: Optional[timedelta] = None,
        mtime=None,
        basename: Optional[str] = None,
    ):
        if basename is None:
            basename = os.path.basename(orig_path)
        if timeout is None:
            timeout = timedelta(minutes=5)
        object_name = self._get_object_name(codebase, run_id, basename)
        with open(orig_path, "rb") as f:
            plain_data = f.read()
        compressed_data = gzip.compress(plain_data, mtime=mtime)
        try:
            await self.storage.upload(
                self.bucket_name,
                object_name,
                compressed_data,
                timeout=int(timeout.total_seconds()),
            )
        except ClientResponseError as e:
            if e.status == 503:
                raise ServiceUnavailable() from e
            if e.status == 403:
                data = await self.storage.download(
                    self.bucket_name,
                    object_name,
                    session=self.session,
                    timeout=int(timeout.total_seconds()),
                )
                if data == plain_data:
                    return
                raise PermissionError(e.message) from e
            raise


def get_log_manager(location, trace_configs=None):
    if location is None:
        # TODO(jelmer): Use a temporary directory
        return FileSystemLogFileManager("/tmp")
    if location.startswith("gs://"):
        return GCSLogFileManager(location, trace_configs=trace_configs)
    if location.startswith("http:") or location.startswith("https:"):
        return S3LogFileManager(location, trace_configs=trace_configs)
    return FileSystemLogFileManager(location)


async def import_log(
    logfile_manager: LogFileManager,
    pkg: str,
    log_id: str,
    name: str,
    path: str,
    *,
    mtime: Optional[int] = None,
    backup_logfile_manager: Optional[LogFileManager] = None,
):
    try:
        await logfile_manager.import_log(pkg, log_id, path, mtime=mtime)
    except ServiceUnavailable as e:
        logging.warning("Unable to upload logfile %s: %s", name, e)
        primary_logfile_upload_failed_count.inc()
        if backup_logfile_manager:
            await backup_logfile_manager.import_log(pkg, log_id, path, mtime=mtime)
    except asyncio.TimeoutError as e:
        logging.warning("Timeout uploading logfile %s: %s", name, e)
        primary_logfile_upload_failed_count.inc()
        if backup_logfile_manager:
            await backup_logfile_manager.import_log(pkg, log_id, path, mtime=mtime)
    except PermissionError as e:
        logging.warning(
            "Permission denied error while uploading logfile %s: %s", name, e
        )
        # It may just be that the file already exists
        try:
            suffix = utcnow().isoformat(timespec="seconds")
            alternative_basename = os.path.basename(path) + "." + suffix
            await logfile_manager.import_log(
                pkg, log_id, path, mtime=mtime, basename=alternative_basename
            )
        except (asyncio.TimeoutError, PermissionError, ServiceUnavailable):
            pass
        else:
            logfile_uploaded_count.inc()
            return
        primary_logfile_upload_failed_count.inc()
        if backup_logfile_manager:
            await backup_logfile_manager.import_log(pkg, log_id, path, mtime=mtime)
    else:
        logfile_uploaded_count.inc()


async def import_logs(
    entries,
    logfile_manager: LogFileManager,
    pkg: str,
    log_id: str,
    *,
    backup_logfile_manager: Optional[LogFileManager] = None,
    mtime: Optional[int] = None,
):
    await asyncio.gather(
        *[
            import_log(
                logfile_manager,
                pkg,
                log_id,
                entry.name,
                entry.path,
                mtime=mtime,
                backup_logfile_manager=backup_logfile_manager,
            )
            for entry in entries
        ]
    )
