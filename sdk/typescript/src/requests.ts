// Generated from api/openapi.json. Do not edit.
import { Transport, type RequestOptions } from "./transport.js";
import type { RangeChunk } from "./wire.js";
import type { EventStream } from "./stream.js";
import type * as M from "./models.js";
export class Requests extends Transport {
  async createSandbox(args: { idempotency_key: string; body: M.Input<M.CreateRequest> }, options: RequestOptions = {}): Promise<M.AdmittedResponse> {
    return await this.request("POST", ["v1", "sandboxes"],
      [], [["Idempotency-Key", args.idempotency_key]],
      "CreateRequest", args.body, 202, "AdmittedResponse",
      "file", 0n, 32768n, options) as M.AdmittedResponse;
  }
  async listSandboxes(args: { limit?: bigint | number; cursor?: string }, options: RequestOptions = {}): Promise<M.SandboxList> {
    return await this.request("GET", ["v1", "sandboxes"],
      [["limit", args.limit], ["cursor", args.cursor]], [],
      null, undefined, 200, "SandboxList",
      "file", 0n, 32768n, options) as M.SandboxList;
  }
  async getSandbox(args: { sandbox_id: string }, options: RequestOptions = {}): Promise<M.SandboxBody> {
    return await this.request("GET", ["v1", "sandboxes", args.sandbox_id],
      [], [],
      null, undefined, 200, "SandboxBody",
      "file", 0n, 32768n, options) as M.SandboxBody;
  }
  async executeCommand(args: { sandbox_id: string; idempotency_key: string; body: M.Input<M.CommandInput> }, options: RequestOptions = {}): Promise<M.AdmittedResponse> {
    return await this.request("POST", ["v1", "sandboxes", args.sandbox_id, "execute"],
      [], [["Idempotency-Key", args.idempotency_key]],
      "CommandInput", args.body, 202, "AdmittedResponse",
      "file", 0n, 32768n, options) as M.AdmittedResponse;
  }
  async destroySandbox(args: { sandbox_id: string; idempotency_key: string; body: M.Input<M.DestroyRequest> }, options: RequestOptions = {}): Promise<M.AdmittedResponse> {
    return await this.request("POST", ["v1", "sandboxes", args.sandbox_id, "destroy"],
      [], [["Idempotency-Key", args.idempotency_key]],
      "DestroyRequest", args.body, 202, "AdmittedResponse",
      "file", 0n, 32768n, options) as M.AdmittedResponse;
  }
  async listOperations(args: { limit?: bigint | number; cursor?: string; sandbox_id?: string }, options: RequestOptions = {}): Promise<M.OperationList> {
    return await this.request("GET", ["v1", "operations"],
      [["limit", args.limit], ["cursor", args.cursor], ["sandbox_id", args.sandbox_id]], [],
      null, undefined, 200, "OperationList",
      "file", 0n, 32768n, options) as M.OperationList;
  }
  async getOperation(args: { operation_id: string }, options: RequestOptions = {}): Promise<M.OperationBody> {
    return await this.request("GET", ["v1", "operations", args.operation_id],
      [], [],
      null, undefined, 200, "OperationBody",
      "file", 0n, 32768n, options) as M.OperationBody;
  }
  async cancelCommand(args: { operation_id: string; idempotency_key: string; body: M.Input<M.CancelRequest> }, options: RequestOptions = {}): Promise<M.AdmittedResponse> {
    return await this.request("POST", ["v1", "operations", args.operation_id, "cancel"],
      [], [["Idempotency-Key", args.idempotency_key]],
      "CancelRequest", args.body, 202, "AdmittedResponse",
      "file", 0n, 32768n, options) as M.AdmittedResponse;
  }
  async readOutput(args: { operation_id: string; output_name: string; offset?: bigint | number; limit?: bigint | number }, options: RequestOptions = {}): Promise<RangeChunk> {
    return await this.request("GET", ["v1", "operations", args.operation_id, "outputs", args.output_name],
      [["offset", args.offset], ["limit", args.limit]], [],
      null, undefined, 200, "binary",
      "output", args.offset ?? 0n, args.limit ?? 32768n, options) as RangeChunk;
  }
  async streamOutput(args: { operation_id: string; cursor?: string; last_event_id?: string }, options: RequestOptions = {}): Promise<EventStream> {
    return await this.request("GET", ["v1", "operations", args.operation_id, "stream"],
      [["cursor", args.cursor]], [["Last-Event-ID", args.last_event_id]],
      null, undefined, 200, "stream",
      "file", 0n, 32768n, options) as EventStream;
  }
  async uploadFile(args: { sandbox_id: string; idempotency_key: string; path: string; x_file_size: bigint | number; x_file_sha256: string; x_file_mode?: string; body: Uint8Array }, options: RequestOptions = {}): Promise<M.AdmittedResponse> {
    return await this.request("PUT", ["v1", "sandboxes", args.sandbox_id, "files"],
      [["path", args.path]], [["Idempotency-Key", args.idempotency_key], ["X-File-Size", args.x_file_size], ["X-File-SHA256", args.x_file_sha256], ["X-File-Mode", args.x_file_mode]],
      "bytes", args.body, 202, "AdmittedResponse",
      "file", 0n, 32768n, options) as M.AdmittedResponse;
  }
  async captureFile(args: { sandbox_id: string; body: M.Input<M.FileCaptureRequest> }, options: RequestOptions = {}): Promise<M.FileCaptureResponse> {
    return await this.request("POST", ["v1", "sandboxes", args.sandbox_id, "files", "captures"],
      [], [],
      "FileCaptureRequest", args.body, 201, "FileCaptureResponse",
      "file", 0n, 32768n, options) as M.FileCaptureResponse;
  }
  async readCapturedFile(args: { sandbox_id: string; x_file_capture: string; offset?: bigint | number; limit?: bigint | number }, options: RequestOptions = {}): Promise<RangeChunk> {
    return await this.request("GET", ["v1", "sandboxes", args.sandbox_id, "files", "captures"],
      [["offset", args.offset], ["limit", args.limit]], [["X-File-Capture", args.x_file_capture]],
      null, undefined, 200, "binary",
      "file", args.offset ?? 0n, args.limit ?? 32768n, options) as RangeChunk;
  }
  async releaseFileCapture(args: { sandbox_id: string; x_file_capture: string }, options: RequestOptions = {}): Promise<void> {
    return await this.request("DELETE", ["v1", "sandboxes", args.sandbox_id, "files", "captures"],
      [], [["X-File-Capture", args.x_file_capture]],
      null, undefined, 204, "empty",
      "file", 0n, 32768n, options) as void;
  }
}
