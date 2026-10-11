import { EditorState, StateEffect, StateField } from '@codemirror/state';
import { Decoration, EditorView, keymap, lineNumbers, type DecorationSet } from '@codemirror/view';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { setDiagnostics } from '@codemirror/lint';
import { AnsiUp } from 'ansi_up';
import type { AnalysisReply, AnalysisRequest, Diagnostic, PageCall, RunReply, RunRequest, TokenRange } from './protocol';
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

// A worker and its generation; replies from replaced workers are ignored.
interface Slot { worker?: Worker; generation: number; ready: boolean }
const analysis: Slot = { generation: 0, ready: false };
const runner: Slot = { generation: 0, ready: false };

let version = 0;
let requestId = 0;
let latestAnalysis = 0;
let analysisVersion: number | undefined;
// The compiler diagnostics of the latest analysis, which its type diagnostics join.
let compileDiagnostics: Diagnostic[] = [];
// The source version whose analysis made the analysis worker fail.
let failedAnalysis: number | undefined;
let activeRun: number | undefined;
let stopping = false;
let stopTimer: ReturnType<typeof setTimeout> | undefined;
let outputVersion: number | undefined;
let timer: ReturnType<typeof setTimeout>;
let view: EditorView;
// Output decoders for the active run. Both keep state across writes, which may
// end partway through a character or escape sequence.
let decoder = new TextDecoder();
let ansi = newAnsi();

function newAnsi(): AnsiUp {
  const ansi = new AnsiUp();
  ansi.use_classes = true;
  return ansi;
}

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
  runButton.disabled = !runner.ready || activeRun !== undefined;
  stopButton.disabled = activeRun === undefined || stopping;
}
function showSnapshot() {
  element('snapshot').textContent = outputVersion !== undefined && outputVersion !== version ? '(earlier source)' : '';
}
function postAnalysis(message: AnalysisRequest) {
  analysis.worker?.postMessage(message);
}
function postRun(message: RunRequest) {
  runner.worker?.postMessage(message);
}
function requestAnalysis() {
  clearTimeout(timer);
  if (!analysis.ready || version === failedAnalysis) return;
  latestAnalysis = ++requestId;
  analysisVersion = version;
  postAnalysis({ type: 'analyze', id: latestAnalysis, version, source: view.state.doc.toString() });
}
function scheduleAnalysis() {
  clearTimeout(timer);
  timer = setTimeout(requestAnalysis, 200);
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
// Renders console output; ansi_up escapes HTML and turns SGR styling into spans.
function appendOutput(text: string) {
  if (text) element('output').insertAdjacentHTML('beforeend', ansi.ansi_to_html(text));
}
// Performs a host call forwarded by the worker for run `id`.
function hostCall(id: number, call: PageCall) {
  switch (call.method) {
    case 'write':
      if (id === activeRun) appendOutput(decoder.decode(call.args[0], { stream: true }));
      break;
  }
}
function endStop() {
  clearTimeout(stopTimer);
  stopping = false;
}
// Replaces `slot`'s worker with `worker`. `crash` handles a failure after the
// worker is ready; a failure while loading stops the playground.
function start<R extends AnalysisReply | RunReply>(slot: Slot, worker: Worker, handlers: {
  ready: () => void;
  crash: (message: string) => void;
  reply: (reply: R) => void;
}) {
  slot.worker?.terminate();
  const generation = ++slot.generation;
  slot.ready = false;
  slot.worker = worker;
  const fail = (message: string) => {
    if (slot.ready) handlers.crash(message);
    else failLoad(message);
  };
  worker.onmessage = (event: MessageEvent<R>) => {
    if (generation !== slot.generation) return;
    const reply = event.data;
    if (reply.type === 'ready') {
      slot.ready = true;
      handlers.ready();
    } else if (reply.type === 'failure') {
      fail(reply.message);
    } else {
      handlers.reply(reply);
    }
  };
  worker.onerror = event => {
    if (generation !== slot.generation) return;
    event.preventDefault();
    fail(event.message);
  };
}
function failLoad(message: string) {
  setError(message);
  clearTimeout(timer);
  for (const slot of [analysis, runner]) {
    slot.worker?.terminate();
    slot.generation++;
    slot.ready = false;
  }
  activeRun = undefined;
  endStop();
  status.textContent = 'Could not load playground. Reload to retry.';
  controls();
}
function startAnalysisWorker() {
  start<AnalysisReply>(analysis, new Worker(new URL('./analysis-worker.ts', import.meta.url), { type: 'module' }), {
    ready: requestAnalysis,
    crash(message) {
      setError(message);
      // Wait for an edit rather than failing on the same source again.
      failedAnalysis = analysisVersion;
      startAnalysisWorker();
    },
    reply(reply) {
      if (reply.type !== 'analysis' && reply.type !== 'checked') return;
      if (reply.version !== version || reply.id !== latestAnalysis) return;
      if (reply.type === 'analysis') {
        view.dispatch({ effects: tokenEffect.of(reply.value.tokens) });
        compileDiagnostics = reply.value.diagnostics;
        showDiagnostics(compileDiagnostics);
      } else {
        showDiagnostics([...compileDiagnostics, ...reply.diagnostics]);
        element('diagnostics-section').dataset.checked = '';
      }
    },
  });
}
function replaceRunWorker(message = 'Loading…') {
  endStop();
  activeRun = undefined;
  status.textContent = message;
  start<RunReply>(runner, new Worker(new URL('./run-worker.ts', import.meta.url), { type: 'module' }), {
    ready() {
      status.textContent = 'Ready';
      controls();
    },
    crash(message) {
      setError(message);
      replaceRunWorker('Restarting…');
    },
    reply(reply) {
      if (reply.type === 'call') {
        try {
          hostCall(reply.id, reply);
          postRun({ type: 'return', callId: reply.callId });
        } catch (error) {
          postRun({ type: 'return', callId: reply.callId, error: String(error) });
        }
      } else if (reply.type === 'abortCall') {
        // Page calls complete synchronously, so there is nothing to abort.
      } else if (reply.type === 'result' && reply.id === activeRun) {
        appendOutput(decoder.decode());
        endStop();
        activeRun = undefined;
        outputVersion = reply.version;
        setError(reply.value.error ?? '');
        status.textContent = reply.value.canceled ? 'Stopped' : reply.value.error ? 'Failed' : 'Finished';
        showSnapshot();
        controls();
      }
    },
  });
  controls();
}
function startRun() {
  if (!runner.ready || activeRun !== undefined) return;
  element('output').textContent = '';
  decoder = new TextDecoder();
  ansi = newAnsi();
  setError('');
  outputVersion = version;
  showSnapshot();
  activeRun = ++requestId;
  postRun({ type: 'run', id: activeRun, version, source: view.state.doc.toString() });
  status.textContent = 'Running…';
  controls();
}
runButton.onclick = startRun;
stopButton.onclick = () => {
  if (activeRun === undefined || stopping) return;
  stopping = true;
  postRun({ type: 'cancel', id: activeRun });
  status.textContent = 'Stopping…';
  controls();
  // Code that never suspends cannot observe cancellation (#679).
  stopTimer = setTimeout(() => {
    setError('Stopped. The run did not respond to cancellation.');
    replaceRunWorker('Restarting…');
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
            delete element('diagnostics-section').dataset.checked;
            showSnapshot();
            scheduleAnalysis();
          }
        }),
      ],
    }),
  });
  // Lets tests edit through CodeMirror; DOM selection races its focus handling.
  (window as { playground?: { view: EditorView } }).playground = { view };
  select.value = label;
  shareButton.disabled = false;
  startAnalysisWorker();
  replaceRunWorker();
}
init();
