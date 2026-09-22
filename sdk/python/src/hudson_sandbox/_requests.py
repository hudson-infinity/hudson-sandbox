# Generated from api/openapi.json. Do not edit.
from __future__ import annotations
from typing import TYPE_CHECKING
from .models import *  # noqa: F403
from ._transport import Transport
if TYPE_CHECKING:
    from ._stream import EventStream
    from ._wire import RangeChunk

class Requests(Transport):
    async def create_sandbox(self, *, idempotency_key: str, body: CreateRequest) -> AdmittedResponse:
        return await self._request('POST', ['v1', 'sandboxes'],
            [], [('Idempotency-Key', idempotency_key)],
            'CreateRequest', body, 202, 'AdmittedResponse',
            'file', 0, 32768)

    async def list_sandboxes(self, *, limit: int | None = None, cursor: str | None = None) -> SandboxList:
        return await self._request('GET', ['v1', 'sandboxes'],
            [('limit', limit), ('cursor', cursor)], [],
            None, None, 200, 'SandboxList',
            'file', 0, 32768)

    async def get_sandbox(self, *, sandbox_id: str) -> SandboxBody:
        return await self._request('GET', ['v1', 'sandboxes', sandbox_id],
            [], [],
            None, None, 200, 'SandboxBody',
            'file', 0, 32768)

    async def execute_command(self, *, sandbox_id: str, idempotency_key: str, body: CommandInput) -> AdmittedResponse:
        return await self._request('POST', ['v1', 'sandboxes', sandbox_id, 'execute'],
            [], [('Idempotency-Key', idempotency_key)],
            'CommandInput', body, 202, 'AdmittedResponse',
            'file', 0, 32768)

    async def destroy_sandbox(self, *, sandbox_id: str, idempotency_key: str, body: DestroyRequest) -> AdmittedResponse:
        return await self._request('POST', ['v1', 'sandboxes', sandbox_id, 'destroy'],
            [], [('Idempotency-Key', idempotency_key)],
            'DestroyRequest', body, 202, 'AdmittedResponse',
            'file', 0, 32768)

    async def list_operations(self, *, limit: int | None = None, cursor: str | None = None, sandbox_id: str | None = None) -> OperationList:
        return await self._request('GET', ['v1', 'operations'],
            [('limit', limit), ('cursor', cursor), ('sandbox_id', sandbox_id)], [],
            None, None, 200, 'OperationList',
            'file', 0, 32768)

    async def get_operation(self, *, operation_id: str) -> OperationBody:
        return await self._request('GET', ['v1', 'operations', operation_id],
            [], [],
            None, None, 200, 'OperationBody',
            'file', 0, 32768)

    async def cancel_command(self, *, operation_id: str, idempotency_key: str, body: CancelRequest) -> AdmittedResponse:
        return await self._request('POST', ['v1', 'operations', operation_id, 'cancel'],
            [], [('Idempotency-Key', idempotency_key)],
            'CancelRequest', body, 202, 'AdmittedResponse',
            'file', 0, 32768)

    async def read_output(self, *, operation_id: str, output_name: str, offset: int | None = None, limit: int | None = None) -> RangeChunk:
        return await self._request('GET', ['v1', 'operations', operation_id, 'outputs', output_name],
            [('offset', offset), ('limit', limit)], [],
            None, None, 200, 'binary',
            'output', offset or 0, limit if limit is not None else 32768)

    async def stream_output(self, *, operation_id: str, cursor: str | None = None, last_event_id: str | None = None) -> EventStream:
        return await self._request('GET', ['v1', 'operations', operation_id, 'stream'],
            [('cursor', cursor)], [('Last-Event-ID', last_event_id)],
            None, None, 200, 'stream',
            'file', 0, 32768)

    async def upload_file(self, *, sandbox_id: str, idempotency_key: str, path: str, x_file_size: int, x_file_sha256: str, x_file_mode: str | None = None, body: bytes) -> AdmittedResponse:
        return await self._request('PUT', ['v1', 'sandboxes', sandbox_id, 'files'],
            [('path', path)], [('Idempotency-Key', idempotency_key), ('X-File-Size', x_file_size), ('X-File-SHA256', x_file_sha256), ('X-File-Mode', x_file_mode)],
            'bytes', body, 202, 'AdmittedResponse',
            'file', 0, 32768)

    async def capture_file(self, *, sandbox_id: str, body: FileCaptureRequest) -> FileCaptureResponse:
        return await self._request('POST', ['v1', 'sandboxes', sandbox_id, 'files', 'captures'],
            [], [],
            'FileCaptureRequest', body, 201, 'FileCaptureResponse',
            'file', 0, 32768)

    async def read_captured_file(self, *, sandbox_id: str, x_file_capture: str, offset: int | None = None, limit: int | None = None) -> RangeChunk:
        return await self._request('GET', ['v1', 'sandboxes', sandbox_id, 'files', 'captures'],
            [('offset', offset), ('limit', limit)], [('X-File-Capture', x_file_capture)],
            None, None, 200, 'binary',
            'file', offset or 0, limit if limit is not None else 32768)

    async def release_file_capture(self, *, sandbox_id: str, x_file_capture: str) -> None:
        return await self._request('DELETE', ['v1', 'sandboxes', sandbox_id, 'files', 'captures'],
            [], [('X-File-Capture', x_file_capture)],
            None, None, 204, 'empty',
            'file', 0, 32768)
