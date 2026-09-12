import { EditorState, StateEffect, StateField } from '@codemirror/state';
import { Decoration, EditorView, keymap, lineNumbers, type DecorationSet } from '@codemirror/view';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { setDiagnostics } from '@codemirror/lint';
import type { Diagnostic, PageCall, Reply, Request, TokenRange } from './protocol';
import { examples } from './examples';
import { decodeSource, encodeSource } from './share';
import './style.css';

const element = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const runButton = element<HTMLButtonElement>('run');
const stopButton = element<HTMLButtonElement>('stop');
const shareButton = element<HTMLButtonElement>('share');
const select = element<HTMLSelectElement>('example');
const status = element('status');
const tokenEffect = StateEffect.define<TokenRange[]>();
const highlights = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(value, transaction) {
    value = value.map(transaction.changes);
    for (const effect of transaction.effects) {
      if (effect.is(tokenEffect)) {
        value = Decoration.set(effect.value.map(token =>
          Decoration.mark({ class: `token-${token.kind}` }).range(token.from, token.to)), true);
      }
    }
    return value;
  },
  provide: field => EditorView.decorations.from(field),
});

// How long a canceled run may take to unwind before its worker is replaced.
const stopGraceMs = 1000;

let version = 0;
let requestId = 0;
let latestAnalysis = 0;
let activeRun: number | undefined;
let stopping = false;
let stopTimer: ReturnType<typeof setTimeout> | undefined;
let outputVersion: number | undefined;
let worker: Worker;
let generation = 0;
let ready = false;
let timer: ReturnType<typeof setTimeout>;
let view: EditorView;

const blankLabel = 'Blank';

async function initialSource(): Promise<{ doc: string, label: string }> {
  const shared = new URLSearchParams(location.search).get('src');
  if (shared) {
    try {
      return { doc: await decodeSource(shared), label: blankLabel };
    } catch {
      console.warn('Could not decode shared source from URL; loading default example instead.');
    }
  }
  return { doc: examples['Hello, Do'], label: 'Hello, Do' };
}

function controls() {
  runButton.disabled = !ready || activeRun !== undefined;
  stopButton.disabled = activeRun === undefined || stopping;
}
function showSnapshot() {
  element('snapshot').textContent = outputVersion !== undefined && outputVersion !== version ? '(earlier source)' : '';
}
function post(message: Request) {
  worker.postMessage(message);
}
function send(type: 'analyze' | 'run'): number {
  const id = ++requestId;
  post({ type, id, version, source: view.state.doc.toString() });
  return id;
}
function scheduleAnalysis() {
  clearTimeout(timer);
  timer = setTimeout(() => {
    if (ready && activeRun === undefined) latestAnalysis = send('analyze');
  }, 200);
}
function setError(message: string) {
  element('error').textContent = message;
  element('error-section').hidden = !message;
}
function showDiagnostics(diagnostics: Diagnostic[]) {
  const list = element('diagnostics');
  list.replaceChildren();
  for (const diagnostic of diagnostics) {
    const item = document.createElement('li');
    const button = document.createElement('button');
    const line = view.state.doc.lineAt(diagnostic.from).number;
    button.textContent = `${diagnostic.severity} · line ${line}: ${diagnostic.message}`;
    button.onclick = () => {
      view.dispatch({ selection: { anchor: diagnostic.from, head: diagnostic.to }, scrollIntoView: true });
      view.focus();
    };
    item.append(button);
    list.append(item);
  }
  view.dispatch(setDiagnostics(view.state, diagnostics));
  element('diagnostics-section').hidden = diagnostics.length === 0;
}
// Performs a host call forwarded by the worker for run `id`.
function hostCall(id: number, call: PageCall) {
  switch (call.method) {
    case 'echo':
      if (id === activeRun) element('output').append(call.args[0]);
      break;
  }
}
function endStop() {
  clearTimeout(stopTimer);
  stopping = false;
}
function replaceWorker(message = 'Loading…') {
  clearTimeout(timer);
  endStop();
  worker?.terminate();
  const currentGeneration = ++generation;
  ready = false;
  activeRun = undefined;
  status.textContent = message;
  controls();
  worker = new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' });
  worker.onmessage = (event: MessageEvent<Reply>) => {
    if (currentGeneration !== generation) return;
    const reply = event.data;
    if (reply.type === 'ready') {
      ready = true;
      status.textContent = 'Ready';
      controls();
      scheduleAnalysis();
    } else if (reply.type === 'failure') {
      failWorker(reply.message);
    } else if (reply.type === 'analysis') {
      if (reply.version !== version || reply.id !== latestAnalysis || activeRun !== undefined) return;
      view.dispatch({ effects: tokenEffect.of(reply.value.tokens) });
      showDiagnostics(reply.value.diagnostics);
    } else if (reply.type === 'call') {
      try {
        hostCall(reply.id, reply);
        post({ type: 'return', callId: reply.callId });
      } catch (error) {
        post({ type: 'return', callId: reply.callId, error: String(error) });
      }
    } else if (reply.type === 'abortCall') {
      // Page calls complete synchronously, so there is nothing to abort.
    } else if (reply.id === activeRun) {
      endStop();
      activeRun = undefined;
      outputVersion = reply.version;
      setError(reply.value.error ?? '');
      status.textContent = reply.value.canceled ? 'Stopped' : reply.value.error ? 'Failed' : 'Finished';
      showSnapshot();
      if (reply.version === version) showDiagnostics(reply.value.diagnostics);
      controls();
      scheduleAnalysis();
    }
  };
  worker.onerror = event => {
    if (currentGeneration !== generation) return;
    event.preventDefault();
    failWorker(event.message);
  };
}
function failWorker(message: string) {
  setError(message);
  if (ready) {
    replaceWorker('Restarting…');
  } else {
    worker.terminate();
    generation++;
    ready = false;
    activeRun = undefined;
    endStop();
    status.textContent = 'Could not load playground. Reload to retry.';
    controls();
  }
}
function startRun() {
  if (!ready || activeRun !== undefined) return;
  clearTimeout(timer);
  element('output').textContent = '';
  setError('');
  outputVersion = version;
  showSnapshot();
  activeRun = send('run');
  status.textContent = 'Running…';
  controls();
}
runButton.onclick = startRun;
stopButton.onclick = () => {
  if (activeRun === undefined || stopping) return;
  stopping = true;
  post({ type: 'cancel', id: activeRun });
  status.textContent = 'Stopping…';
  controls();
  // Code that never suspends cannot observe cancellation (#679).
  stopTimer = setTimeout(() => {
    setError('Stopped. The run did not respond to cancellation.');
    replaceWorker('Restarting…');
  }, stopGraceMs);
};
shareButton.onclick = async () => {
  try {
    const param = await encodeSource(view.state.doc.toString());
    const url = `${location.origin}${location.pathname}?src=${param}`;
    window.history.replaceState(null, '', url);
    await navigator.clipboard.writeText(url);
    const original = shareButton.textContent;
    shareButton.textContent = 'Copied!';
    setTimeout(() => { shareButton.textContent = original; }, 1500);
  } catch (err) {
    setError(`Could not create share link: ${err instanceof Error ? err.message : String(err)}`);
  }
};
select.add(new Option(blankLabel, blankLabel));
for (const name of Object.keys(examples)) select.add(new Option(name, name));
select.onchange = () => view.dispatch({
  changes: { from: 0, to: view.state.doc.length, insert: select.value === blankLabel ? '' : examples[select.value] },
});

async function init() {
  const { doc, label } = await initialSource();
  view = new EditorView({
    parent: element('editor'),
    state: EditorState.create({
      doc,
      extensions: [lineNumbers(), history(), highlights, EditorView.lineWrapping,
        EditorView.theme({
          '.cm-gutters': { backgroundColor: '#1d202d', color: '#7f88a4', border: 'none' },
          '.cm-content': { padding: '1rem 0' },
        }, { dark: true }),
        EditorView.contentAttributes.of({ 'aria-label': 'Do source', spellcheck: 'false' }),
        keymap.of([{ key: 'Mod-Enter', run: () => { startRun(); return true; } }, ...defaultKeymap, ...historyKeymap, indentWithTab]),
        EditorView.updateListener.of(update => {
          if (update.docChanged) {
            version++;
            element('diagnostics').replaceChildren();
            element('diagnostics-section').hidden = true;
            showSnapshot();
            scheduleAnalysis();
          }
        }),
      ],
    }),
  });
  select.value = label;
  shareButton.disabled = false;
  replaceWorker();
}
init();
