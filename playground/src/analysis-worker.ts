import init, { Analyzed } from '../pkg/dolang_wasm.js';
import wasmUrl from '../pkg/dolang_wasm_bg.wasm?url';
import type { AnalysisReply, AnalysisRequest, Diagnostic, Hover } from './protocol';
import { html, markdown, type Bundled } from './hover';

// How long edits must settle before the source is type checked, as in the LSP.
const checkDelayMs = 300;

const reply = (message: AnalysisReply) => self.postMessage(message);

// The latest source, compiled, and its type diagnostics once checked, with the
// request and page version it last answered. It's freed when a different
// source replaces it.
interface Entry { source: string; analyzed: Analyzed; id: number; version: number; diagnostics?: Diagnostic[] }
let current: Entry | undefined;
let checkTimer: ReturnType<typeof setTimeout> | undefined;

// Bundled items aren't described yet (#1434).
const bundled: Bundled = () => [];

function check(entry: Entry) {
  clearTimeout(checkTimer);
  const diagnostics: Diagnostic[] = entry.analyzed.check();
  entry.diagnostics = diagnostics;
  reply({ type: 'checked', id: entry.id, version: entry.version, diagnostics });
}

function analyze(id: number, version: number, source: string) {
  clearTimeout(checkTimer);
  if (current?.source !== source) {
    const analyzed = new Analyzed(source);
    current?.analyzed.free();
    current = { source, analyzed, id, version };
  }
  const entry = current;
  entry.id = id;
  entry.version = version;
  const { analyzed } = entry;
  reply({ type: 'analysis', id, version, value: { tokens: analyzed.tokens(), diagnostics: analyzed.diagnostics() } });
  if (entry.diagnostics) {
    reply({ type: 'checked', id, version, diagnostics: entry.diagnostics });
    return;
  }
  checkTimer = setTimeout(() => check(entry), checkDelayMs);
}

// A hover for a replaced version gets no answer; the page has given up on it.
// One while a check is pending checks now, so it shows checked types.
function hover(id: number, version: number, pos: number) {
  const entry = current;
  if (entry?.version !== version) return;
  if (!entry.diagnostics) check(entry);
  const found: Hover | undefined = entry.analyzed.hover(pos, pos);
  const text = found && markdown(found, bundled);
  const value = found && text !== undefined
    ? { from: found.from, to: found.to, html: html(text) }
    : undefined;
  reply({ type: 'hover', id, version, value });
}

// Requests that arrive while loading wait for it; compiling is synchronous, so
// replies keep request order. Each analysis cancels the pending check of the
// one before it.
const loaded = init({ module_or_path: wasmUrl }).then(() => reply({ type: 'ready' }));
loaded.catch(error => reply({ type: 'failure', message: String(error) }));
self.onmessage = (event: MessageEvent<AnalysisRequest>) => {
  const request = event.data;
  loaded.then(() => {
    if (request.type === 'analyze') analyze(request.id, request.version, request.source);
    else hover(request.id, request.version, request.pos);
  }).catch(error => reply({ type: 'failure', message: String(error) }));
};
