import type { Host } from '../pkg/dolang_wasm.js';

/** A problem the compiler or checker found, from `Analyzed.diagnostics` and `check` */
export interface Diagnostic {
  from: number;
  to: number;
  severity: 'error' | 'warning' | 'lint';
  message: string;
  labels: Label[];
  notes: Note[];
  fixes: Fix[];
}
/** A labelled span: the cause of a diagnostic, or context for it */
export interface Label {
  primary: boolean;
  /** Empty for a label that only marks its span */
  message: string;
  /** In the source, or in a bundled module, which has no text here */
  at: { from: number; to: number } | { module: string; line: number; column: number };
}
export interface Note { kind: 'info' | 'help'; message: string }
/** A suggested change: replacing `from` to `to` with `insert` */
export interface Fix { from: number; to: number; insert: string; title: string }
export interface TokenRange { from: number; to: number; kind: string }
/** A name of a declaration; `write` marks the one it binds */
export interface Occurrence { from: number; to: number; write: boolean }
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
  /** Parenthesized where an annotation needs it to be */
  type: string;
  /** The overload a call chose, from 0 */
  overload?: number;
}
export type Declaration =
  | {
    kind: 'local';
    /** Whether its signature says what its checked type would, or its type says all its signature does */
    shape: 'signature' | 'value' | 'other';
    /** The header of the class it's a member of, as `class C[T]` */
    class?: string;
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
    /** Whether it has a body, which a type-only declaration doesn't */
    body: boolean;
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
  error?: RunError;
  canceled: boolean;
}
/** An error a run raised, or its source's failure to compile */
export interface RunError {
  message: string;
  /** The backtrace, innermost frame first */
  frames: RunFrame[];
}
export interface RunFrame {
  module: string;
  receiver: string;
  method?: string;
  /** The code running in the frame: in the source run, in UTF-16 offsets, or elsewhere */
  at?: { from: number; to: number } | { path: string; line: number; column: number };
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
  | { type: 'hover'; id: number; version: number; pos: number }
  /** The names of the declaration that the name at `pos` names */
  | { type: 'occurrences'; id: number; version: number; pos: number }
  /** Where the bundled modules' hover docs are, sent first */
  | { type: 'docs'; url: string };
/** A hover tooltip's range and HTML */
export interface HoverTip { from: number; to: number; html: string }
export type AnalysisReply =
  | Lifecycle
  | { type: 'analysis'; id: number; version: number; value: Analysis }
  | { type: 'checked'; id: number; version: number; diagnostics: Diagnostic[] }
  | { type: 'hover'; id: number; version: number; value?: HoverTip }
  | { type: 'occurrences'; id: number; version: number; spans: Occurrence[] };
export type RunRequest =
  | { type: 'run'; id: number; version: number; source: string }
  | { type: 'cancel'; id: number }
  | { type: 'return'; callId: number; error?: string };
export type RunReply =
  | Lifecycle
  | ({ type: 'call'; id: number; callId: number } & PageCall)
  | { type: 'abortCall'; callId: number }
  | { type: 'result'; id: number; version: number; value: RunResult };
