export interface Diagnostic {
  from: number;
  to: number;
  severity: 'error' | 'warning' | 'info';
  message: string;
}
export interface TokenRange { from: number; to: number; kind: string }
export interface Analysis { tokens: TokenRange[]; diagnostics: Diagnostic[] }
export interface RunResult {
  output: string;
  error?: string;
  diagnostics: Diagnostic[];
}
export type Request = {
  type: 'analyze' | 'run';
  id: number;
  version: number;
  source: string;
};
export type Reply =
  | { type: 'ready' }
  | { type: 'failure'; message: string }
  | { type: 'analysis'; id: number; version: number; value: Analysis }
  | { type: 'output'; id: number; version: number; chunk: string }
  | { type: 'result'; id: number; version: number; value: RunResult };
