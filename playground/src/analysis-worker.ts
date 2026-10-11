import init, { analyze, check } from '../pkg/dolang_wasm.js';
import wasmUrl from '../pkg/dolang_wasm_bg.wasm?url';
import type { AnalysisReply, AnalysisRequest, Diagnostic } from './protocol';

// How long edits must settle before the source is type checked, as in the LSP.
const checkDelayMs = 300;

const reply = (message: AnalysisReply) => self.postMessage(message);

// The last source checked and its diagnostics, kept while the source is unchanged.
let checked: { source: string; diagnostics: Diagnostic[] } | undefined;
let checkTimer: ReturnType<typeof setTimeout> | undefined;

// Requests that arrive while loading wait for it; `analyze` is synchronous, so
// replies keep request order. Each request cancels the pending check of the
// one before it.
const loaded = init({ module_or_path: wasmUrl }).then(() => reply({ type: 'ready' }));
loaded.catch(error => reply({ type: 'failure', message: String(error) }));
self.onmessage = (event: MessageEvent<AnalysisRequest>) => {
  const { id, version, source } = event.data;
  loaded.then(() => {
    clearTimeout(checkTimer);
    reply({ type: 'analysis', id, version, value: analyze(source) });
    if (checked?.source === source) {
      reply({ type: 'checked', id, version, diagnostics: checked.diagnostics });
      return;
    }
    checkTimer = setTimeout(() => {
      checked = { source, diagnostics: check() };
      reply({ type: 'checked', id, version, diagnostics: checked.diagnostics });
    }, checkDelayMs);
  }).catch(error => reply({ type: 'failure', message: String(error) }));
};
