import { Agent, request as httpsRequest } from 'node:https';
import type { IncomingMessage } from 'node:http';
import { inspect } from 'node:util';
import { Config } from './config.js';
import { ClientError, fail, integer, model, one, parseJson, rangeChunk, stringify, type Headers } from './wire.js';
import { EventStream } from './stream.js';
export interface RequestOptions { signal?: AbortSignal }
export interface Response {
  message: IncomingMessage; headers: Headers; status: number; deadline: number; close(): void;
}
export class Transport {
  #config: Config; #agent: Agent;
  constructor(configPath: string) {
    this.#config = new Config(configPath);
    this.#agent = new Agent({ keepAlive: false, maxSockets: 32, rejectUnauthorized: true, ca: this.#config.ca });
  }
  close(): void { this.#agent.destroy(); }
  [Symbol.dispose](): void { this.close(); }
  [inspect.custom](): string { return 'Client(...)'; }
  protected async request(method: string, segments: string[], query: [string, unknown][], headers: [string, unknown][], bodyType: string | null, body: unknown, status: number, result: string, prefix: string, rawOffset: bigint | number, rawLimit: bigint | number, options: RequestOptions): Promise<unknown> {
    let response: Response | undefined;
    try {
      for (const s of segments) if (typeof s !== 'string' || !s.isWellFormed() || !Buffer.byteLength(s) || Buffer.byteLength(s) > 256 || s === '.' || s === '..' || /[\x00-\x1f\x7f-\x9f/\\%?#]/u.test(s)) fail('request');
      const url = new URL('/' + segments.map(encodeURIComponent).join('/'), this.#config.endpoint);
      for (const [key, raw] of query) {
        if (raw === undefined || raw === null) continue;
        let value = raw;
        if (typeof raw === 'number' || typeof raw === 'bigint') { const n = integer(raw); if (n < 0n || n > 18446744073709551615n) fail('request'); value = String(n); }
        if (typeof value !== 'string' || !value.isWellFormed() || Buffer.byteLength(value) > (key === 'path' ? 4096 : 2048) || /[\x00-\x1f\x7f-\x9f]/u.test(value)) fail('request');
        url.searchParams.append(key, value);
      }
      const h: Record<string, string> = { Authorization: this.#config.authorization(), 'Accept-Encoding': 'identity' };
      for (const [key, raw] of headers) {
        if (raw === undefined || raw === null) continue;
        let value = raw;
        if (typeof raw === 'number' || typeof raw === 'bigint') { const n = integer(raw); if (n < 0n || n > 18446744073709551615n) fail('request'); value = String(n); }
        const cap = key === 'X-File-Capture' ? 16384 : key === 'Last-Event-ID' ? 2048 : 128;
        if (typeof value !== 'string' || !/^[\x20-\x7e]+$/.test(value) || value.length > cap || (key === 'Idempotency-Key' && !/^[A-Za-z0-9._-]{16,128}$/.test(value))) fail('request'); h[key] = value;
      }
      let payload: Buffer | undefined;
      if (bodyType === 'bytes') {
        if (!(body instanceof Uint8Array) || body.byteLength > 8388608) fail('request'); payload = Buffer.from(body); h['Content-Type'] = 'application/octet-stream';
      } else if (bodyType) {
        payload = Buffer.from(stringify(model(bodyType, body, false))); if (payload.length > 65536) fail('request'); h['Content-Type'] = 'application/json';
      }
      if (payload) h['Content-Length'] = String(payload.length);
      const offset = integer(rawOffset), limit = integer(rawLimit);
      if (result === 'binary' && (offset < 0n || offset > 18446744073709551615n || limit < 1n || limit > 32768n)) fail('request');
      response = await this.send(url, method, h, payload, result === 'stream' ? 100 : this.#config.timeout, options);
      if (response.status !== status) {
        let problem: Record<string, unknown> | undefined;
        try { problem = model('ProblemBody', parseJson(await readBody(response, 65536))); } catch { /* Keep HTTP status even if the problem body fails. */ }
        if (problem?.status !== response.status) problem = undefined;
        throw new ClientError('http', response.status, problem?.code as string | undefined, problem?.operation_id as string | undefined);
      }
      if (one(response.headers, 'cache-control') !== 'no-store') fail('protocol');
      if (result === 'empty') { await readBody(response, 0); return; }
      const expected = result === 'binary' ? 'application/octet-stream' : result === 'stream' ? 'text/event-stream' : 'application/json';
      if (one(response.headers, 'content-type').split(';')[0]?.trim() !== expected || response.headers['content-encoding']) fail('protocol');
      if (result === 'stream') { const stream = new EventStream(response); response = undefined; return stream; }
      const bytes = await readBody(response, result === 'binary' ? Number(limit) : 2 * 1024 * 1024);
      return result === 'binary' ? rangeChunk(response.headers, bytes, prefix, offset, limit) : model(result, parseJson(bytes));
    } catch (error) {
      if (error instanceof ClientError) throw error;
      if (response && response.status !== status) throw new ClientError('http', response.status);
      throw new ClientError('transport');
    } finally { response?.close(); }
  }
  private send(url: URL, method: string, headers: Record<string, string>, body: Buffer | undefined, seconds: number, options: RequestOptions): Promise<Response> {
    return new Promise((resolve, reject) => {
      const deadline = performance.now() + seconds * 1000;
      let message: IncomingMessage | undefined;
      let timer: ReturnType<typeof setTimeout> | undefined, connect: ReturnType<typeof setTimeout> | undefined;
      const request = httpsRequest(url, { method, headers, agent: this.#agent, rejectUnauthorized: true, signal: options.signal });
      const close = () => { clearTimeout(timer); clearTimeout(connect); message?.destroy(); request.destroy(); };
      timer = setTimeout(() => { close(); reject(new ClientError('transport')); }, seconds * 1000);
      request.on('error', () => { close(); reject(new ClientError('transport')); });
      request.setTimeout(20000, close);
      request.on('socket', socket => {
        connect = setTimeout(close, 5000);
        socket.once('secureConnect', () => clearTimeout(connect));
      });
      request.on('response', incoming => {
        message = incoming;
        // Attach before yielding so an aborted response cannot emit an unhandled error.
        message.on('error', () => {});
        message.once('end', () => { clearTimeout(timer); clearTimeout(connect); });
        const all: Headers = Object.create(null);
        for (let i = 0; i < message.rawHeaders.length; i += 2) {
          const key = message.rawHeaders[i]!.toLowerCase(); (all[key] ??= []).push(message.rawHeaders[i + 1]!);
        }
        resolve({ message, headers: all, status: message.statusCode ?? 0, deadline, close });
      });
      request.end(body);
    });
  }
}
export async function readBody(response: Response, cap: number): Promise<Buffer> {
  const length = response.headers['content-length'];
  if (length && (length.length !== 1 || !/^[0-9]+$/.test(length[0]!) || BigInt(length[0]!) > BigInt(cap))) fail('protocol');
  const chunks: Buffer[] = []; let size = 0;
  try {
    for await (const chunk of response.message) {
      const bytes = Buffer.from(chunk as Uint8Array); size += bytes.length; if (size > cap) fail('protocol'); chunks.push(bytes);
    }
    if (!response.message.complete) fail('transport'); return Buffer.concat(chunks, size);
  } catch (error) { if (error instanceof ClientError) throw error; throw new ClientError('transport'); }
}
