import init, { analyze, run, type Host } from '../pkg/dolang_wasm.js';
import wasmUrl from '../pkg/dolang_wasm_bg.wasm?url';
import type { PageCall, Reply, Request } from './protocol';

const reply = (message: Reply) => self.postMessage(message);
const runs = new Map<number, AbortController>();
const calls = new Map<number, { resolve: () => void; reject: (error: Error) => void }>();
let nextCall = 0;

// Forwards a host call to the page; the page answers with `return`.
function forward(id: number, call: PageCall, signal: AbortSignal): Promise<void> {
  const callId = ++nextCall;
  return new Promise((resolve, reject) => {
    calls.set(callId, { resolve, reject });
    signal.addEventListener('abort', () => {
      calls.delete(callId);
      reply({ type: 'abortCall', callId });
    }, { once: true });
    reply({ type: 'call', id, callId, ...call });
  });
}

// Capabilities available in the worker are implemented here directly;
// the rest are forwarded to the page.
const host = (id: number): Host => ({
  write: (data, signal) => forward(id, { method: 'write', args: [data] }, signal),
});

// Serialize requests even when run yields to the browser executor.
let pending = init({ module_or_path: wasmUrl }).then(() => reply({ type: 'ready' }));
pending.catch(error => reply({ type: 'failure', message: String(error) }));
self.onmessage = (event: MessageEvent<Request>) => {
  const request = event.data;
  // Cancellation and call results bypass the queue, since the run they
  // affect is still pending in it.
  if (request.type === 'cancel') {
    runs.get(request.id)?.abort();
    return;
  }
  if (request.type === 'return') {
    const call = calls.get(request.callId);
    calls.delete(request.callId);
    if (request.error === undefined) call?.resolve();
    else call?.reject(new Error(request.error));
    return;
  }
  const { type, id, version, source } = request;
  const controller = new AbortController();
  if (type === 'run') runs.set(id, controller);
  pending = pending.then(async () => {
    if (type === 'analyze') {
      reply({ type: 'analysis', id, version, value: analyze(source) });
    } else {
      try {
        const value = await run(source, host(id), controller.signal);
        reply({ type: 'result', id, version, value });
      } finally {
        runs.delete(id);
      }
    }
  }).catch(error => reply({ type: 'failure', message: String(error) }));
};
