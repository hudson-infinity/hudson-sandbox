from __future__ import annotations
import asyncio
import re
from urllib.parse import quote, urlencode
import httpx
from ._config import Config
from ._wire import (
    ClientError,
    decode_model,
    encode_model,
    json_bytes,
    parse_json,
    one,
    range_chunk,
)


class Transport:
    def __init__(self, config_path):
        self._config = Config(config_path)
        transport = httpx.AsyncHTTPTransport(
            verify=self._config.ssl,
            trust_env=False,
            retries=0,
            limits=httpx.Limits(max_connections=32, max_keepalive_connections=0),
        )
        self._http = httpx.AsyncClient(
            transport=transport,
            trust_env=False,
            follow_redirects=False,
            timeout=httpx.Timeout(self._config.timeout, connect=5, read=20, pool=5),
        )

    def __repr__(self):
        return "Client(...)"

    @classmethod
    def from_config(cls, path):
        return cls(path)

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        await self.aclose()

    async def aclose(self):
        await self._http.aclose()

    async def _request(
        self,
        method,
        segments,
        query,
        headers,
        body_type,
        body,
        status,
        result,
        prefix,
        offset,
        limit,
    ):
        response = None
        try:
            for s in segments:
                if (
                    not isinstance(s, str)
                    or not 1 <= len(s.encode()) <= 256
                    or s in (".", "..")
                    or any(
                        ord(c) < 32 or 127 <= ord(c) <= 159 or c in "/\\%?#" for c in s
                    )
                ):
                    raise ClientError("request")
            target = (
                self._config.endpoint
                + "/"
                + "/".join(quote(s, safe="") for s in segments)
            )
            q = []
            for k, v in query:
                if v is None:
                    continue
                if type(v) is int:
                    if not 0 <= v <= 2**64 - 1:
                        raise ClientError("request")
                    v = str(v)
                if (
                    not isinstance(v, str)
                    or len(v.encode()) > (4096 if k == "path" else 2048)
                    or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in v)
                ):
                    raise ClientError("request")
                q.append((k, v))
            if q:
                target += "?" + urlencode(q)
            h = {
                "Authorization": "Bearer " + self._config.token,
                "Accept-Encoding": "identity",
            }
            for k, v in headers:
                if v is None:
                    continue
                if type(v) is int:
                    if not 0 <= v <= 2**64 - 1:
                        raise ClientError("request")
                    v = str(v)
                cap = (
                    16384
                    if k == "X-File-Capture"
                    else 2048
                    if k == "Last-Event-ID"
                    else 128
                )
                if (
                    not isinstance(v, str)
                    or not 1 <= len(v.encode()) <= cap
                    or any(ord(c) < 32 or ord(c) > 126 for c in v)
                ):
                    raise ClientError("request")
                if k == "Idempotency-Key" and not re.fullmatch(
                    "[A-Za-z0-9._-]{16,128}", v
                ):
                    raise ClientError("request")
                h[k] = v
            if body_type == "bytes":
                if not isinstance(body, bytes) or len(body) > 8388608:
                    raise ClientError("request")
                payload = body
                h["Content-Type"] = "application/octet-stream"
            elif body_type:
                payload = json_bytes(encode_model(body_type, body), 65536)
                h["Content-Type"] = "application/json"
            else:
                payload = None
            if result == "binary" and (
                type(offset) is not int
                or type(limit) is not int
                or not 0 <= offset <= 2**64 - 1
                or not 1 <= limit <= 32768
            ):
                raise ClientError("request")
            deadline = asyncio.get_running_loop().time() + (
                100 if result == "stream" else self._config.timeout
            )
            async with asyncio.timeout_at(deadline):
                request = self._http.build_request(
                    method, target, headers=h, content=payload
                )
                response = await self._http.send(request, stream=True)
                if response.status_code != status:
                    problem = None
                    try:
                        problem = decode_model(
                            "ProblemBody", parse_json(await self._read(response, 65536))
                        )
                    except ClientError:
                        pass
                    if problem is not None and problem.status != response.status_code:
                        problem = None
                    raise ClientError(
                        "http",
                        response.status_code,
                        problem.code if problem else None,
                        problem.operation_id if problem else None,
                    )
                if one(response.headers, "cache-control") != "no-store":
                    raise ClientError("protocol")
                if result == "empty":
                    await self._read(response, 0)
                    return None
                expected = (
                    "application/octet-stream"
                    if result == "binary"
                    else "text/event-stream"
                    if result == "stream"
                    else "application/json"
                )
                if (
                    one(response.headers, "content-type").split(";")[0].strip()
                    != expected
                    or "content-encoding" in response.headers
                ):
                    raise ClientError("protocol")
                if result == "stream":
                    from ._stream import EventStream

                    stream = EventStream(response, deadline)
                    response = None
                    return stream
                data = await self._read(
                    response, limit if result == "binary" else 2 * 1024 * 1024
                )
                if result == "binary":
                    return range_chunk(response.headers, data, prefix, offset, limit)
                return decode_model(result, parse_json(data))
        except ClientError:
            raise
        except (httpx.HTTPError, TimeoutError):
            if response is not None and response.status_code != status:
                raise ClientError("http", response.status_code) from None
            raise ClientError("transport") from None
        except (TypeError, ValueError, UnicodeError, OverflowError):
            raise ClientError("request") from None
        finally:
            if response is not None:
                await response.aclose()

    @staticmethod
    async def _read(response, cap):
        data = bytearray()
        length = response.headers.get("content-length")
        if length is not None and (
            not length.isascii() or not length.isdigit() or int(length) > cap
        ):
            raise ClientError("protocol")
        try:
            async for chunk in response.aiter_raw():
                if len(data) + len(chunk) > cap:
                    raise ClientError("protocol")
                data.extend(chunk)
        except httpx.HTTPError:
            raise ClientError("transport") from None
        return bytes(data)
