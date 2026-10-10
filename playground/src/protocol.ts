import type { Host } from '../pkg/dolang_wasm.js';

export interface Diagnostic {
  from: number;
  to: number;
  severity: 'error' | 'warning' | 'info';
  message: string;
}
export interface TokenRange { from: number; to: number; kind: string }
export interface Analysis { tokens: TokenRange[]; diagnostics: Diagnostic[] }
export interface RunResult {
  result?: string;
  error?: string;
  canceled: boolean;
  diagnostics: Diagnostic[];
}
/** Host methods the worker forwards to the page. */
export type PageMethod = 'write';
/** A host method's arguments without its trailing `AbortSignal`. */
export type HostArgs<M extends keyof Host> =
  Host[M] extends (...args: [...infer A, AbortSignal]) => unknown ? A : never;
export type PageCall = { [M in PageMethod]: { method: M; args: HostArgs<M> } }[PageMethod];
type Lifecycle = { type: 'ready' } | { type: 'failure'; message: string };
export type AnalysisRequest = { type: 'analyze'; id: number; version: number; source: string };
export type AnalysisReply =
  | Lifecycle
  | { type: 'analysis'; id: number; version: number; value: Analysis };
export type RunRequest =
  | { type: 'run'; id: number; version: number; source: string }
  | { type: 'cancel'; id: number }
  | { type: 'return'; callId: number; error?: string };
export type RunReply =
  | Lifecycle
  | ({ type: 'call'; id: number; callId: number } & PageCall)
  | { type: 'abortCall'; callId: number }
  | { type: 'result'; id: number; version: number; value: RunResult };
