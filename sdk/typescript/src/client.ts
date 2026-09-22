import { createHash, randomUUID } from 'node:crypto';
import { open, link, unlink } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { Requests } from './requests.js';
import { ClientError, fail } from './wire.js';
import type { FileCaptureResponse, OperationBody } from './models.js';
export function newIdempotencyKey(): string { return randomUUID(); }
export class Client extends Requests {
  async wait(operationId: string, seconds = 60): Promise<OperationBody> {
    if (!Number.isFinite(seconds) || seconds <= 0 || seconds > 86400) fail('request');
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), seconds * 1000);
    try {
      while (true) {
        const operation = await this.getOperation({ operation_id: operationId }, { signal: controller.signal });
        if (operation.operation_id !== operationId) fail('protocol');
        if (['succeeded', 'failed', 'cancelled', 'unknown'].includes(operation.status)) return operation;
        if (!['queued', 'running'].includes(operation.status)) fail('protocol');
        await delay(500, undefined, { signal: controller.signal });
      }
    } catch (error) { if (controller.signal.aborted) throw new ClientError('wait_timeout'); throw error; }
    finally { clearTimeout(timer); }
  }
  async upload(args: { sandbox_id: string; path: string; data: Uint8Array; idempotency_key: string; mode?: string }) {
    const mode = args.mode ?? '0644';
    if (!(args.data instanceof Uint8Array) || args.data.byteLength > 8388608 || !['0644','0755'].includes(mode)) fail('request');
    // Snapshot caller-owned bytes so concurrent mutation cannot invalidate the digest.
    const body = Buffer.from(args.data);
    return this.uploadFile({ sandbox_id: args.sandbox_id, path: args.path, body, idempotency_key: args.idempotency_key,
      x_file_size: body.length, x_file_sha256: createHash('sha256').update(body).digest('hex'), x_file_mode: mode });
  }
  async download(args: { sandbox_id: string; path: string; destination: string }): Promise<{ size: bigint; sha256: string; simulated: boolean; guest_reported: boolean; release_confirmed: boolean }> {
    if (typeof args.destination !== 'string' || !args.destination || args.destination.includes('\0')) fail('request');
    const temporary = join(dirname(args.destination), `.hudson-${randomUUID()}.tmp`);
    let file;
    try { file = await open(temporary, 'wx', 0o600); } catch { throw new ClientError('file'); }
    let capture: FileCaptureResponse | undefined; let released = false; let failed = false;
    try {
      capture = await this.captureFile({ sandbox_id: args.sandbox_id, body: { path: args.path } });
      try {
        if (capture.size > 8388608n || capture.chunk_size !== 32768 || !capture.guest_reported || !/^[0-9a-f]{64}$/.test(capture.sha256)) fail('protocol');
        let offset = 0n; const hash = createHash('sha256'); const signal = AbortSignal.timeout(60000);
        while (true) {
          const chunk = await this.readCapturedFile({ sandbox_id: args.sandbox_id, x_file_capture: capture.capture, offset, limit: 32768 }, { signal });
          if (chunk.size !== capture.size || chunk.sha256 !== capture.sha256 || chunk.simulated !== capture.simulated) fail('integrity');
          try { await file.writeFile(chunk.data); } catch { throw new ClientError('file'); }
          hash.update(chunk.data); offset = chunk.next_offset; if (chunk.eof) break;
        }
        if (offset !== capture.size || hash.digest('hex') !== capture.sha256) fail('integrity');
      } finally {
        try { await this.releaseFileCapture({ sandbox_id: args.sandbox_id, x_file_capture: capture.capture }, { signal: AbortSignal.timeout(2000) }); released = true; } catch { /* Capture expiry bounds uncertain release. */ }
      }
      try { await file.sync(); await link(temporary, args.destination); } catch { throw new ClientError('file'); }
      return { size: capture.size, sha256: capture.sha256, simulated: capture.simulated, guest_reported: capture.guest_reported, release_confirmed: released };
    } catch (error) { failed = true; throw error; }
    finally {
      let cleanupFailed = false;
      try { await file.close(); } catch { cleanupFailed = true; }
      try { await unlink(temporary); } catch { cleanupFailed = true; }
      if (cleanupFailed && !failed) throw new ClientError('file');
    }
  }
}
