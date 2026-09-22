import { inspect } from 'node:util';
import type { Response } from './transport.js';
import type { OutputEvent, EndEvent, StreamProblemEvent } from './models.js';
import { ClientError, fail, model, parseJson } from './wire.js';
export type Event = { event: 'output'; data: OutputEvent; cursor: string } | { event: 'end'; data: EndEvent; cursor: string } | { event: 'gap' | 'error'; data: StreamProblemEvent };
export function decodeEvent(kind: string, cursor: string | undefined, raw: string): Event {
  if (kind === 'output' || kind === 'end') {
    if (!cursor || Buffer.byteLength(cursor) > 2048 || /[\x00-\x1f\x7f-\x9f]/u.test(cursor)) fail('protocol');
  } else if ((kind !== 'gap' && kind !== 'error') || cursor !== undefined) fail('protocol');
  if (kind === 'output') {
    const data = model('OutputEvent', parseJson(raw)) as unknown as OutputEvent;
    const bytes = Buffer.from(data.data_base64, 'base64');
    if (bytes.toString('base64') !== data.data_base64 || bytes.length > 32768 || !['stdout','stderr'].includes(data.stream) || data.offset + BigInt(bytes.length) !== data.next_offset || data.next_offset > data.stored || data.stored > 10485760n || data.stored > data.seen || data.at_end !== (data.next_offset === data.stored) || !data.guest_reported || data.truncated !== (data.seen > data.stored)) fail('protocol');
    return { event: kind, data, cursor: cursor! };
  }
  if (kind === 'end') {
    const data = model('EndEvent', parseJson(raw)) as unknown as EndEvent;
    if (data.reason !== 'complete' || !data.guest_reported || [data.stdout, data.stderr].some(s => s.stored > s.seen || s.stored > 10485760n || s.truncated !== (s.seen > s.stored))) fail('protocol');
    return { event: kind, data, cursor: cursor! };
  }
  return { event: kind as 'gap' | 'error', data: model('StreamProblemEvent', parseJson(raw)) as unknown as StreamProblemEvent };
}
export class Parser {
  #line: number[] = []; #data: string[] = []; #event = ''; #id?: string;
  #bytes = 0; #afterCr = false; #first = true;
  byte(byte: number): Event | undefined {
    if (this.#afterCr && byte === 10) { this.#afterCr = false; return; }
    this.#afterCr = byte === 13; if (++this.#bytes > 65536) fail('protocol');
    if (byte !== 10 && byte !== 13) { this.#line.push(byte); return; }
    let line: string;
    try { line = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(Uint8Array.from(this.#line)); } catch { return fail('protocol'); }
    this.#line = [];
    if (this.#first) { if (line.startsWith('\ufeff')) line = line.slice(1); this.#first = false; }
    if (!line) {
      this.#bytes = 0; const data = this.#data.join('\n'), kind = this.#event, cursor = this.#id;
      this.#data = []; this.#event = ''; this.#id = undefined;
      return data ? decodeEvent(kind, cursor, data) : undefined;
    }
    if (line.startsWith(':')) return;
    const colon = line.indexOf(':'); const field = colon < 0 ? line : line.slice(0, colon);
    let value = colon < 0 ? '' : line.slice(colon + 1); if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'event') this.#event = value;
    else if (field === 'id' && !value.includes('\0')) this.#id = value;
    else if (field === 'data') this.#data.push(value);
  }
}
export class EventStream implements AsyncIterableIterator<Event> {
  #response: Response; #chunks: AsyncIterator<Uint8Array>; #pending = new Uint8Array(); #position = 0; #parser = new Parser(); #ended = false;
  constructor(response: Response) { this.#response = response; this.#chunks = response.message[Symbol.asyncIterator](); }
  [inspect.custom](): string { return 'EventStream(...)'; }
  [Symbol.asyncIterator](): AsyncIterableIterator<Event> { return this; }
  close(): void { this.#ended = true; this.#response.close(); }
  async [Symbol.asyncDispose](): Promise<void> { this.close(); }
  async return(): Promise<IteratorResult<Event>> { this.close(); return { done: true, value: undefined }; }
  async next(): Promise<IteratorResult<Event>> {
    if (this.#ended) return { done: true, value: undefined };
    try {
      while (true) {
        if (performance.now() >= this.#response.deadline) fail('transport');
        while (this.#position < this.#pending.length) {
          const event = this.#parser.byte(this.#pending[this.#position++]!);
          if (event) { if (event.event !== 'output') this.close(); return { done: false, value: event }; }
        }
        const next = await this.#chunks.next();
        if (next.done) { this.close(); return { done: true, value: undefined }; }
        if (next.value.byteLength > 262144) fail('protocol'); this.#pending = new Uint8Array(next.value); this.#position = 0;
      }
    } catch (error) { this.close(); if (error instanceof ClientError) throw error; throw new ClientError('transport'); }
  }
}
