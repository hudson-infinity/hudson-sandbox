import { openSync, closeSync, fstatSync, readSync, constants } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { inspect } from 'node:util';
import { ClientError, parseJson, integer, fail } from './wire.js';
export function privateBytes(path: string): Buffer {
  let fd: number | undefined;
  try {
    if (!process.geteuid || !constants.O_NOFOLLOW) fail('configuration');
    fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    const stat = fstatSync(fd);
    if (!stat.isFile() || stat.uid !== process.geteuid() || (stat.mode & 0o077) !== 0 || stat.size > 65536) fail('configuration');
    const data = Buffer.alloc(65537); let size = 0;
    while (size < data.length) { const n = readSync(fd, data, size, data.length - size, null); if (!n) break; size += n; }
    if (size > 65536) fail('configuration'); return data.subarray(0, size);
  } catch { throw new ClientError('configuration'); } finally { if (fd !== undefined) closeSync(fd); }
}
function record(v: unknown): Record<string, unknown> {
  if (!v || typeof v !== 'object' || Array.isArray(v)) fail('configuration'); return v as Record<string, unknown>;
}
export class Config {
  readonly endpoint: URL; readonly timeout: number; readonly ca?: Buffer; #token: string;
  constructor(path: string) {
    try {
      const data = record(parseJson(privateBytes(path)));
      if (Object.keys(data).some(k => !['version', 'endpoint', 'credential_file', 'ca_file', 'request_timeout_seconds'].includes(k)) || data.version !== 1) fail('configuration');
      const timeout = Object.hasOwn(data, 'request_timeout_seconds') ? data.request_timeout_seconds : 30;
      if (typeof timeout !== 'number' || !Number.isInteger(timeout) || timeout < 1 || timeout > 120) fail('configuration'); this.timeout = timeout;
      if (typeof data.endpoint !== 'string' || !/^https:\/\/[^/?#\\]+\/?$/.test(data.endpoint) || /[\s\x00-\x20\x7f?#\\]/u.test(data.endpoint)) fail('configuration');
      this.endpoint = new URL(data.endpoint);
      if (!data.endpoint.startsWith('https://') || this.endpoint.protocol !== 'https:' || this.endpoint.username || this.endpoint.password || this.endpoint.pathname !== '/' || this.endpoint.search || this.endpoint.hash || data.endpoint.includes('@')) fail('configuration');
      if (typeof data.credential_file !== 'string') fail('configuration');
      const credential = record(parseJson(privateBytes(resolve(dirname(path), data.credential_file))));
      const keys = ['version','project_id','name','token','created_at','expires_at'];
      if (Object.keys(credential).length !== keys.length || keys.some(k => !Object.hasOwn(credential,k)) || credential.version !== 1) fail('configuration');
      if (typeof credential.project_id !== 'string' || !credential.project_id.startsWith('prj_') || typeof credential.name !== 'string' || Buffer.byteLength(credential.name) > 4096) fail('configuration');
      const created = integer(credential.created_at), expires = integer(credential.expires_at);
      if (created < 0n || expires <= created || expires > 9223372036854775807n) fail('configuration');
      if (typeof credential.token !== 'string' || !/^[\x21-\x7e]{1,4096}$/.test(credential.token)) fail('configuration'); this.#token = credential.token;
      if (data.ca_file !== undefined && data.ca_file !== null) {
        if (typeof data.ca_file !== 'string') fail('configuration'); this.ca = privateBytes(resolve(dirname(path), data.ca_file));
      }
    } catch { throw new ClientError('configuration'); }
  }
  authorization(): string { return 'Bearer ' + this.#token; }
  [inspect.custom](): string { return 'Config(...)'; }
}
