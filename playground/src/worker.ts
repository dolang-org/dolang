import init, { analyze, run } from '../pkg/dolang_wasm.js';
import wasmUrl from '../pkg/dolang_wasm_bg.wasm?url';
import type { Request, Reply } from './protocol';

const reply = (message: Reply) => self.postMessage(message);
// Serialize requests even when run yields to the browser executor.
let pending = init({ module_or_path: wasmUrl }).then(() => reply({ type: 'ready' }));
pending.catch(error => reply({ type: 'failure', message: String(error) }));
self.onmessage = (event: MessageEvent<Request>) => {
  const request = event.data;
  pending = pending.then(async () => {
    const { id, version, source } = request;
    if (request.type === 'analyze') {
      reply({ type: 'analysis', id, version, value: analyze(source) });
    } else {
      const value = await run(source, (chunk: string) => reply({ type: 'output', id, version, chunk }));
      reply({ type: 'result', id, version, value });
    }
  }).catch(error => reply({ type: 'failure', message: String(error) }));
};
