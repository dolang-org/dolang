import init, { analyze } from '../pkg/dolang_wasm.js';
import wasmUrl from '../pkg/dolang_wasm_bg.wasm?url';
import type { AnalysisReply, AnalysisRequest } from './protocol';

const reply = (message: AnalysisReply) => self.postMessage(message);

// Requests that arrive while loading wait for it; `analyze` is synchronous, so
// replies keep request order.
const loaded = init({ module_or_path: wasmUrl }).then(() => reply({ type: 'ready' }));
loaded.catch(error => reply({ type: 'failure', message: String(error) }));
self.onmessage = (event: MessageEvent<AnalysisRequest>) => {
  const { id, version, source } = event.data;
  loaded.then(() => reply({ type: 'analysis', id, version, value: analyze(source) }))
    .catch(error => reply({ type: 'failure', message: String(error) }));
};
