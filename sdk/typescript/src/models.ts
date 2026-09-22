// Generated from api/openapi.json. Do not edit.
import type { JsonValue } from "./wire.js";
export type Input<T> = T extends bigint ? bigint | number : T extends Array<infer U> ? Input<U>[] : T extends object ? { [K in keyof T]: Input<T[K]> } : T;

export interface RequestedResources {
  vcpu: number;
  memory_mib: bigint;
  disk_mib: bigint;
}
export interface CreateRequest {
  image_digest: string;
  name?: string | null;
  resources: RequestedResources;
  correlation_id?: string | null;
}
export interface CommandInput {
  argv: Array<string>;
  env?: Record<string, string>;
  cwd?: string;
  deadline_unix_ms: bigint;
  output_limit?: bigint;
}
export interface DestroyRequest {
  correlation_id?: string | null;
}
export interface CancelRequest {
}
export interface AdmittedResponse {
  sandbox_id: string;
  operation_id: string;
  status: string;
  status_url: string;
}
export interface ProblemBody {
  operation_id?: string;
  title: string;
  status: number;
  code: string;
}
export interface OperationBody {
  response_expired?: boolean;
  operation_id: string;
  sandbox_id: string;
  target_operation_id?: string;
  kind: string;
  status: string;
  phase?: string;
  output_status?: string;
  result?: JsonValue;
  error?: JsonValue;
  created_at: string;
  completed_at?: string;
}
export interface SandboxBody {
  observation_simulated?: boolean;
  sandbox_id: string;
  name?: string;
  desired_state: string;
  observed_state: string;
  observed_at?: string;
  image_digest: string;
  resources: JsonValue;
  generation: bigint;
  active_operation_id?: string;
  created_at: string;
}
export interface SandboxList {
  items: Array<SandboxBody>;
  next_cursor: string | null;
}
export interface OperationList {
  items: Array<OperationBody>;
  next_cursor: string | null;
}
export interface FileCaptureRequest {
  path: string;
}
export interface FileCaptureResponse {
  capture: string;
  size: bigint;
  sha256: string;
  expires_unix_ms: bigint;
  chunk_size: number;
  simulated: boolean;
  guest_reported: boolean;
}
export interface OutputStats {
  seen: bigint;
  stored: bigint;
  truncated: boolean;
}
export interface OutputEvent {
  stream: string;
  offset: bigint;
  next_offset: bigint;
  data_base64: string;
  at_end: boolean;
  complete: boolean;
  seen: bigint;
  stored: bigint;
  truncated: boolean;
  simulated: boolean;
  guest_reported: boolean;
}
export interface EndEvent {
  reason: string;
  stdout: OutputStats;
  stderr: OutputStats;
  simulated: boolean;
  guest_reported: boolean;
}
export interface StreamProblemEvent {
  code: string;
}
