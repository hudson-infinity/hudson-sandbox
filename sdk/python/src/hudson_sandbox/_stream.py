from __future__ import annotations
import asyncio
import base64
import dataclasses
import httpx
from ._wire import ClientError, decode_model, parse_json


@dataclasses.dataclass(repr=False)
class Event:
    event: str
    data: object
    cursor: str | None = None

    def __repr__(self):
        return "Event(...)"

    def to_wire(self):
        value = {"event": self.event, "data": self.data.to_wire()}
        if self.cursor is not None:
            value["cursor"] = self.cursor
        return value


class Parser:
    def __init__(self):
        self.line = bytearray()
        self.data = []
        self.event = ""
        self.id = None
        self.frame_bytes = 0
        self.after_cr = False
        self.first = True

    def byte(self, byte):
        if self.after_cr and byte == 10:
            self.after_cr = False
            return None
        self.after_cr = byte == 13
        self.frame_bytes += 1
        if self.frame_bytes > 65536:
            raise ClientError("protocol")
        if byte not in (10, 13):
            self.line.append(byte)
            return None
        try:
            line = self.line.decode("utf-8")
        except UnicodeError:
            raise ClientError("protocol") from None
        self.line.clear()
        if self.first:
            line = line.lstrip("\ufeff")
            self.first = False
        if not line:
            self.frame_bytes = 0
            data, kind, cursor = "\n".join(self.data), self.event, self.id
            self.data = []
            self.event = ""
            self.id = None
            if not data:
                return None
            return decode_event(kind, cursor, data)
        if line.startswith(":"):
            return None
        field, _, value = line.partition(":")
        if value.startswith(" "):
            value = value[1:]
        if field == "event":
            self.event = value
        elif field == "id" and "\0" not in value:
            self.id = value
        elif field == "data":
            self.data.append(value)
        return None


def decode_event(kind, cursor, raw):
    if kind in ("output", "end"):
        if (
            not cursor
            or len(cursor.encode()) > 2048
            or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in cursor)
        ):
            raise ClientError("protocol")
    elif kind not in ("gap", "error") or cursor is not None:
        raise ClientError("protocol")
    name = {
        "output": "OutputEvent",
        "end": "EndEvent",
        "gap": "StreamProblemEvent",
        "error": "StreamProblemEvent",
    }[kind]
    data = decode_model(name, parse_json(raw))
    if kind == "output":
        try:
            binary = base64.b64decode(data.data_base64, validate=True)
        except ValueError:
            raise ClientError("protocol") from None
        if (
            base64.b64encode(binary).decode() != data.data_base64
            or len(binary) > 32768
            or data.stream not in ("stdout", "stderr")
            or data.offset + len(binary) != data.next_offset
            or data.next_offset > data.stored
            or data.stored > 10485760
            or data.stored > data.seen
            or data.at_end != (data.next_offset == data.stored)
            or not data.guest_reported
            or data.truncated != (data.seen > data.stored)
        ):
            raise ClientError("protocol")
    if kind == "end":
        if (
            data.reason != "complete"
            or not data.guest_reported
            or any(
                s.stored > s.seen
                or s.stored > 10485760
                or s.truncated != (s.seen > s.stored)
                for s in (data.stdout, data.stderr)
            )
        ):
            raise ClientError("protocol")
    return Event(kind, data, cursor)


class EventStream:
    def __init__(self, response, deadline):
        self._response = response
        self._deadline = deadline
        self._chunks = response.aiter_raw().__aiter__()
        self._pending = b""
        self._position = 0
        self._parser = Parser()
        self._ended = False

    def __repr__(self):
        return "EventStream(...)"

    def __aiter__(self):
        return self

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        await self.aclose()

    async def aclose(self):
        self._ended = True
        await self._response.aclose()

    async def __anext__(self):
        if self._ended:
            raise StopAsyncIteration
        try:
            if asyncio.get_running_loop().time() >= self._deadline:
                raise ClientError("transport")
            async with asyncio.timeout_at(self._deadline):
                while True:
                    while self._position < len(self._pending):
                        byte = self._pending[self._position]
                        self._position += 1
                        event = self._parser.byte(byte)
                        if event is not None:
                            if event.event != "output":
                                await self.aclose()
                            return event
                    try:
                        self._pending = await anext(self._chunks)
                    except StopAsyncIteration:
                        await self.aclose()
                        raise  # Drop incomplete frames; never advance a cursor.
                    self._position = 0
                    if len(self._pending) > 262144:
                        raise ClientError("protocol")
        except (httpx.HTTPError, TimeoutError):
            await self.aclose()
            raise ClientError("transport") from None
        except BaseException:
            await self.aclose()
            raise
