import init, { Analyzed } from '../pkg/dolang_wasm.js';
import wasmUrl from '../pkg/dolang_wasm_bg.wasm?url';
import type { AnalysisReply, AnalysisRequest, Diagnostic } from './protocol';

// How long edits must settle before the source is type checked, as in the LSP.
const checkDelayMs = 300;

const reply = (message: AnalysisReply) => self.postMessage(message);

// The latest source, compiled, and its type diagnostics once checked. It's
// freed when a different source replaces it.
let current: { source: string; analyzed: Analyzed; diagnostics?: Diagnostic[] } | undefined;
let checkTimer: ReturnType<typeof setTimeout> | undefined;

// Requests that arrive while loading wait for it; compiling is synchronous, so
// replies keep request order. Each request cancels the pending check of the
// one before it.
const loaded = init({ module_or_path: wasmUrl }).then(() => reply({ type: 'ready' }));
loaded.catch(error => reply({ type: 'failure', message: String(error) }));
self.onmessage = (event: MessageEvent<AnalysisRequest>) => {
  const { id, version, source } = event.data;
  loaded.then(() => {
    clearTimeout(checkTimer);
    if (current?.source !== source) {
      const analyzed = new Analyzed(source);
      current?.analyzed.free();
      current = { source, analyzed };
    }
    const entry = current;
    const { analyzed } = entry;
    reply({ type: 'analysis', id, version, value: { tokens: analyzed.tokens(), diagnostics: analyzed.diagnostics() } });
    if (entry.diagnostics) {
      reply({ type: 'checked', id, version, diagnostics: entry.diagnostics });
      return;
    }
    checkTimer = setTimeout(() => {
      const diagnostics: Diagnostic[] = analyzed.check();
      entry.diagnostics = diagnostics;
      reply({ type: 'checked', id, version, diagnostics });
    }, checkDelayMs);
  }).catch(error => reply({ type: 'failure', message: String(error) }));
};
