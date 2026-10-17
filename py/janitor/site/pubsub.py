#!/usr/bin/python3
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

import asyncio
import json

from aiohttp import web
from aiohttp_openmetrics import Gauge

subscription_count = Gauge(
    "subscriptions", "Subscriptions per topic", labelnames=("topic",)
)


class Subscription:
    """A pubsub subscription."""

    def __init__(self, topic: "Topic") -> None:
        self.topic = topic
        self.queue: asyncio.Queue = asyncio.Queue(maxsize=100)
        if topic.last:
            self.queue.put_nowait(topic.last)

    def __enter__(self):
        self.topic.subscriptions.add(self.queue)
        subscription_count.labels(self.topic.name).inc()
        return self.queue

    def __exit__(self, type, value, traceback):
        subscription_count.labels(self.topic.name).dec()
        self.topic.subscriptions.remove(self.queue)


class Topic:
    """A pubsub topic."""

    def __init__(self, name, repeat_last: bool = False) -> None:
        self.name = name
        self.subscriptions: set[asyncio.Queue] = set()
        self.last = None
        self.repeat_last = repeat_last

    def publish(self, message):
        if self.repeat_last:
            self.last = message
        for queue in self.subscriptions:
            if queue.full():
                # A subscriber that does not read loses the oldest message.
                queue.get_nowait()
            queue.put_nowait(message)


async def pubsub_handler(topic: Topic, request) -> web.WebSocketResponse:
    ws = web.WebSocketResponse()
    await ws.prepare(request)

    with Subscription(topic) as queue:
        while True:
            msg = await queue.get()
            try:
                await ws.send_str(json.dumps(msg))
            except TypeError as e:
                raise TypeError(f"not jsonable: {msg!r}") from e
            except ConnectionResetError:
                break

    return ws
