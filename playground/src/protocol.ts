import type { Host } from '../pkg/dolang_wasm.js';

export interface Diagnostic {
  from: number;
  to: number;
  severity: 'error' | 'warning' | 'info';
  message: string;
}
export interface TokenRange { from: number; to: number; kind: string }
export interface Analysis { tokens: TokenRange[]; diagnostics: Diagnostic[] }
/** What hover shows at a position, from `Analyzed.hover`. */
export interface Hover {
  /** The reference under the position, or else the checked node's span */
  from: number;
  to: number;
  /** What the check concluded there, empty before a check */
  types: TypeLine[];
  declarations: Declaration[];
  /**
   * Whether every declaration in the source the check found was described. A
   * bundled key with no docs isn't either.
   */
  complete: boolean;
}
export interface TypeLine {
  role: 'variable' | 'callee' | 'value';
  /** The variable, the member or item as `m.C.member` or `m.item`, or the reference */
  name?: string;
  type: string;
  /** The overload a call chose, from 0 */
  overload?: number;
}
export type Declaration =
  | {
    kind: 'local';
    /** Whether its signature says what its checked type would, or its type says all its signature does */
    shape: 'signature' | 'value' | 'other';
    /** Where it's declared, as "in class `C`" */
    note?: string;
    /** Its doc comment, as Markdown */
    doc: string;
    signature: Signature;
  }
  | {
    /** An item of a bundled module: `item` is empty for the module, and `Class.member` for a member */
    kind: 'bundled';
    module: string;
    item: string;
    /** The overload a call chose, from 0, among the item's */
    overload?: number;
  };
/** A declaration's signature in parts, with types as written */
export type Signature =
  | {
    form: 'class';
    pub: boolean;
    typeOnly: boolean;
    name: string;
    binders: string[];
    decorators: string[];
    supers: string[];
  }
  | {
    form: 'def';
    pub: boolean;
    /** An `@def` overload */
    typeOnly: boolean;
    name: string;
    binders: string[];
    decorators: string[];
    params: Param[];
    returns?: string;
  }
  | { form: 'field' | 'let'; pub: boolean; name: string; type?: string }
  | { form: 'param'; param: Param };
export interface Param {
  /** The parameter up to its annotation, sigils included */
  head: string;
  type?: string;
  default: boolean;
}
export interface RunResult {
  result?: string;
  error?: string;
  canceled: boolean;
}
/** Host methods the worker forwards to the page. */
export type PageMethod = 'write';
/** A host method's arguments without its trailing `AbortSignal`. */
export type HostArgs<M extends keyof Host> =
  Host[M] extends (...args: [...infer A, AbortSignal]) => unknown ? A : never;
export type PageCall = { [M in PageMethod]: { method: M; args: HostArgs<M> } }[PageMethod];
type Lifecycle = { type: 'ready' } | { type: 'failure'; message: string };
export type AnalysisRequest =
  | { type: 'analyze'; id: number; version: number; source: string }
  | { type: 'hover'; id: number; version: number; pos: number };
/** A hover tooltip's range and HTML */
export interface HoverTip { from: number; to: number; html: string }
export type AnalysisReply =
  | Lifecycle
  | { type: 'analysis'; id: number; version: number; value: Analysis }
  | { type: 'checked'; id: number; version: number; diagnostics: Diagnostic[] }
  | { type: 'hover'; id: number; version: number; value?: HoverTip };
export type RunRequest =
  | { type: 'run'; id: number; version: number; source: string }
  | { type: 'cancel'; id: number }
  | { type: 'return'; callId: number; error?: string };
export type RunReply =
  | Lifecycle
  | ({ type: 'call'; id: number; callId: number } & PageCall)
  | { type: 'abortCall'; callId: number }
  | { type: 'result'; id: number; version: number; value: RunResult };
