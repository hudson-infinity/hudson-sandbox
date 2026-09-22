import { readFileSync } from 'node:fs';
import { inspect } from 'node:util';
import { Client, ClientError, RangeChunk, EventStream, stringify } from '../typescript/dist/index.js';
import { parseJson } from '../typescript/dist/wire.js';
const testcase = parseJson(readFileSync(process.argv[3]));
const args = testcase.args;
if (args.body_base64 !== undefined) { args.body = Buffer.from(args.body_base64, 'base64'); delete args.body_base64; }
let client;
try {
  client = new Client(process.argv[2]);
  let result = testcase.action === 'wait' ? await client.wait(args.operation_id, args.seconds?.value ?? args.seconds) : await client[testcase.action](args);
  if (result instanceof EventStream) { const events = []; for await (const event of result) events.push(event); result = events; }
  if (result instanceof RangeChunk) result = { data_base64: Buffer.from(result.data).toString('base64'), ...Object.fromEntries(['offset','next_offset','size','eof','simulated','seen','truncated','sha256','guest_reported'].map(k => [k,result[k] ?? null])) };
  console.log(stringify({ ok: result ?? null }));
} catch (error) {
  if (!(error instanceof ClientError)) throw error;
  if ((inspect(error) + String(error)).includes('private-backend-text')) throw new Error('unredacted error');
  console.log(stringify({ error: error.kind, ...(error.kind === 'http' ? { status: error.status, code: error.code ?? null, operation_id: error.operationId ?? null } : {}) }));
} finally { client?.close(); }
