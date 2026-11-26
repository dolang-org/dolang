import { EditorState, StateEffect, StateField } from '@codemirror/state';
import {
  Decoration, EditorView, highlightActiveLine, highlightActiveLineGutter, hoverTooltip, keymap, lineNumbers,
  type DecorationSet, type Tooltip,
} from '@codemirror/view';
import { defaultKeymap, history, historyKeymap, indentWithTab, isolateHistory } from '@codemirror/commands';
import { bracketMatching } from '@codemirror/language';
import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import { highlightSelectionMatches, search, searchKeymap } from '@codemirror/search';
import { AnsiUp } from 'ansi_up';
import type { AnalysisReply, AnalysisRequest, Diagnostic, HoverTip, PageCall, RunReply, RunRequest, Occurrence, TokenRange } from './protocol';
import { exampleGroups, examples } from './examples';
import { decodeSource, encodeSource } from './share';
import { loadDraft, saveDraft } from './drafts';
import { doIndentation } from './indent';
import { layout, tabs } from './layout';
import { problems } from './problems';
import { help } from './help';
import { backtrace } from './trace';
import './style.css';

const element = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const runButton = element<HTMLButtonElement>('run');
const stopButton = element<HTMLButtonElement>('stop');
const shareButton = element<HTMLButtonElement>('share');
const copyButton = element<HTMLButtonElement>('copy-output');
const revertButton = element<HTMLButtonElement>('revert');
const summary = element('run-summary');
const showResults = layout(element('workspace'), element('divider'), element<HTMLButtonElement>('toggle-output'));
const selectTab = tabs(element('execution').querySelector('[role=tablist]')!,
  id => { element('output-actions').hidden = id !== 'output-tab'; });
const problemList = problems(element('problems'), element('problems-count'), new URL('../api/', document.baseURI));
const trace = backtrace(element('backtrace'));
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

// The names of the declaration under the cursor
const occurrenceEffect = StateEffect.define<Occurrence[]>();
const occurrenceMark = Decoration.mark({ class: 'cm-occurrence' });
const writeMark = Decoration.mark({ class: 'cm-occurrence cm-occurrence-write' });
const occurrences = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(value, transaction) {
    if (transaction.docChanged) value = Decoration.none;
    for (const effect of transaction.effects) {
      if (effect.is(occurrenceEffect)) {
        value = Decoration.set(effect.value.map(span => (span.write ? writeMark : occurrenceMark).range(span.from, span.to)), true);
      }
    }
    return value;
  },
  provide: field => EditorView.decorations.from(field),
});
// How long the cursor must rest before its name's occurrences are marked.
// Longer than the analysis delay, so typing doesn't analyze early.
const occurrenceDelayMs = 250;

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
// Hover requests awaiting the analysis worker, by request id.
const hovers = new Map<number, (tip: HoverTip | undefined) => void>();
let activeRun: number | undefined;
// When the active run started, by `performance.now()`
let runStart = 0;
let stopping = false;
let stopTimer: ReturnType<typeof setTimeout> | undefined;
let outputVersion: number | undefined;
let timer: ReturnType<typeof setTimeout>;
let occurrenceTimer: ReturnType<typeof setTimeout> | undefined;
let latestOccurrences = 0;
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
// How long edits must settle before the draft is saved.
const draftDelayMs = 500;
// The text last loaded into the editor and the example label it came with;
// replacing anything else asks first.
let loadedDoc = '';
let label = blankLabel;
// Whether the buffer changed since the draft was last saved. A shared link
// leaves the draft alone until it's edited.
let unsaved = false;
let draftTimer: ReturnType<typeof setTimeout> | undefined;

function flushDraft() {
  clearTimeout(draftTimer);
  if (!unsaved) return;
  unsaved = false;
  saveDraft({ doc: view.state.doc.toString(), label });
}

const exampleText = (label: string) => label === blankLabel ? '' : examples[label];

// The editor's first text, its example label, and the text that counts as
// unedited. A restored draft counts as edited unless it matches its example.
async function initialSource(): Promise<{ doc: string, label: string, loaded: string }> {
  const shared = new URLSearchParams(location.search).get('src');
  if (shared) {
    try {
      const doc = await decodeSource(shared);
      return { doc, label: blankLabel, loaded: doc };
    } catch {
      console.warn('Could not decode shared source from URL; loading default example instead.');
    }
  }
  const draft = loadDraft();
  if (draft) {
    const label = Object.hasOwn(examples, draft.label) ? draft.label : blankLabel;
    return { doc: draft.doc, label, loaded: exampleText(label) };
  }
  return { doc: examples['Hello, Do'], label: 'Hello, Do', loaded: examples['Hello, Do'] };
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
// Marks the names of the declaration at the cursor once it rests, analyzing
// the current version first if needed. A selection marks none.
function scheduleOccurrences() {
  clearTimeout(occurrenceTimer);
  latestOccurrences = 0;
  const { main } = view.state.selection;
  if (!main.empty) {
    view.dispatch({ effects: occurrenceEffect.of([]) });
    return;
  }
  occurrenceTimer = setTimeout(() => {
    if (!analysis.ready || version === failedAnalysis) return;
    if (analysisVersion !== version) requestAnalysis();
    latestOccurrences = ++requestId;
    postAnalysis({ type: 'occurrences', id: latestOccurrences, version, pos: view.state.selection.main.head });
  }, occurrenceDelayMs);
}
// Gives up on pending hovers, whose answers would be for replaced text.
function dropHovers() {
  for (const resolve of hovers.values()) resolve(undefined);
  hovers.clear();
}
// Asks the analysis worker what hover shows at `pos`, analyzing the current
// version first if its analysis is still waiting for edits to settle.
function hover(pos: number): Promise<Tooltip | null> {
  if (!analysis.ready || version === failedAnalysis) return Promise.resolve(null);
  if (analysisVersion !== version) requestAnalysis();
  const id = ++requestId;
  const tip = new Promise<HoverTip | undefined>(resolve => hovers.set(id, resolve));
  postAnalysis({ type: 'hover', id, version, pos });
  return tip.then(tip => tip ? {
    pos: tip.from,
    end: tip.to,
    create() {
      const dom = document.createElement('div');
      dom.className = 'hover';
      dom.innerHTML = tip.html;
      return { dom };
    },
  } : null);
}
function setError(message: string) {
  element('error').textContent = message;
  element('error-section').hidden = !message;
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
// Says below the output how the run ended and how long it took.
function summarize(outcome: 'finished' | 'raised' | 'cancelled', note = '') {
  const ms = performance.now() - runStart;
  const took = ms < 1000 ? `${Math.round(ms)} ms` : `${(ms / 1000).toFixed(2)} s`;
  const text = { finished: `Finished in ${took}`, raised: `Raised an error after ${took}`, cancelled: `Cancelled after ${took}` };
  summary.textContent = text[outcome] + note;
  summary.className = outcome;
}
// Shows the latest flash of `label` on `button`, as for a copy.
function flash(button: HTMLButtonElement, label: string) {
  const original = button.dataset.label ?? button.textContent ?? '';
  button.dataset.label = original;
  button.textContent = label;
  setTimeout(() => { button.textContent = original; delete button.dataset.label; }, 1500);
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
  dropHovers();
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
  dropHovers();
  start<AnalysisReply>(analysis, new Worker(new URL('./analysis-worker.ts', import.meta.url), { type: 'module' }), {
    ready: requestAnalysis,
    crash(message) {
      setError(message);
      // Wait for an edit rather than failing on the same source again.
      failedAnalysis = analysisVersion;
      startAnalysisWorker();
    },
    reply(reply) {
      if (reply.type === 'hover') {
        hovers.get(reply.id)?.(reply.version === version ? reply.value : undefined);
        hovers.delete(reply.id);
        return;
      }
      if (reply.type === 'occurrences') {
        if (reply.id === latestOccurrences && reply.version === version) {
          view.dispatch({ effects: occurrenceEffect.of(reply.spans) });
        }
        return;
      }
      if (reply.type !== 'analysis' && reply.type !== 'checked') return;
      if (reply.version !== version || reply.id !== latestAnalysis) return;
      if (reply.type === 'analysis') {
        view.dispatch({ effects: tokenEffect.of(reply.value.tokens) });
        compileDiagnostics = reply.value.diagnostics;
        problemList.show(compileDiagnostics);
      } else {
        problemList.show([...compileDiagnostics, ...reply.diagnostics]);
        element('problems').dataset.checked = '';
      }
    },
  });
  postAnalysis({ type: 'docs', url: new URL('docs/', document.baseURI).href });
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
        const error = reply.value.error;
        setError(error?.message ?? '');
        if (error) trace.show(error);
        status.textContent = reply.value.canceled ? 'Stopped' : reply.value.error ? 'Failed' : 'Finished';
        summarize(reply.value.canceled ? 'cancelled' : reply.value.error ? 'raised' : 'finished');
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
  summary.textContent = '';
  decoder = new TextDecoder();
  ansi = newAnsi();
  setError('');
  trace.clear();
  trace.started();
  outputVersion = version;
  showSnapshot();
  showResults();
  selectTab('output-tab');
  activeRun = ++requestId;
  runStart = performance.now();
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
    summarize('cancelled', ', restarting');
    replaceRunWorker('Restarting…');
  }, stopGraceMs);
};
shareButton.onclick = async () => {
  try {
    const param = await encodeSource(view.state.doc.toString());
    const url = `${location.origin}${location.pathname}?src=${param}`;
    window.history.replaceState(null, '', url);
    await navigator.clipboard.writeText(url);
    flash(shareButton, 'Copied!');
  } catch (err) {
    setError(`Could not create share link: ${err instanceof Error ? err.message : String(err)}`);
  }
};
element('clear-output').onclick = () => {
  element('output').textContent = '';
  summary.textContent = '';
  setError('');
  trace.clear();
  outputVersion = undefined;
  showSnapshot();
};
copyButton.onclick = async () => {
  const error = element('error').textContent;
  try {
    await navigator.clipboard.writeText(element('output').textContent + (error ? `${error}\n${trace.text()}` : ''));
    flash(copyButton, 'Copied!');
  } catch (err) {
    setError(`Could not copy output: ${err instanceof Error ? err.message : String(err)}`);
  }
};
help(element<HTMLButtonElement>('help'), element<HTMLDialogElement>('help-dialog'), new URL('docs/', document.baseURI));
select.add(new Option(blankLabel, blankLabel));
for (const [label, names] of exampleGroups) {
  const group = select.appendChild(Object.assign(document.createElement('optgroup'), { label }));
  for (const name of names) group.appendChild(new Option(name, name));
}
select.onchange = () => {
  if (view.state.doc.toString() !== loadedDoc && !confirm('Replace your edits with this example?')) {
    select.value = label;
    return;
  }
  label = select.value;
  loadedDoc = exampleText(label);
  view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: loadedDoc } });
};
// Reverting restores the text last loaded, unless that was nothing
function updateRevert() {
  revertButton.disabled = !loadedDoc || view.state.doc.toString() === loadedDoc;
}
revertButton.onclick = () => {
  if (!confirm('Discard your edits and restore the source as it was loaded?')) return;
  // Undo brings the edits back
  view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: loadedDoc }, annotations: isolateHistory.of('full') });
  view.focus();
};
addEventListener('pagehide', () => { if (view) flushDraft(); });

async function init() {
  const initial = await initialSource();
  const { doc } = initial;
  label = initial.label;
  loadedDoc = initial.loaded;
  view = new EditorView({
    parent: element('editor'),
    state: EditorState.create({
      doc,
      extensions: [lineNumbers(), history(), highlights, occurrences, EditorView.lineWrapping, doIndentation(),
        highlightActiveLine(), highlightActiveLineGutter(), highlightSelectionMatches(), bracketMatching(),
        closeBrackets(), EditorState.languageData.of(() => [{ closeBrackets: { brackets: ['(', '[', '{', '"'] } }]),
        search({ top: true }),
        hoverTooltip((_, pos) => hover(pos), { hideOnChange: true }), problemList.extension, trace.extension,
        EditorView.theme({
          '.cm-gutters': { backgroundColor: '#1d202d', color: '#7f88a4', border: 'none' },
          '.cm-content': { padding: '1rem 0' },
        }, { dark: true }),
        EditorView.contentAttributes.of({ 'aria-label': 'Do source', spellcheck: 'false' }),
        keymap.of([{ key: 'Mod-Enter', run: () => { startRun(); return true; } }, ...closeBracketsKeymap, ...searchKeymap,
          ...defaultKeymap, ...historyKeymap, indentWithTab]),
        EditorView.updateListener.of(update => {
          if (update.docChanged) {
            version++;
            dropHovers();
            problemList.stale();
            delete element('problems').dataset.checked;
            showSnapshot();
            scheduleAnalysis();
            unsaved = true;
            clearTimeout(draftTimer);
            draftTimer = setTimeout(flushDraft, draftDelayMs);
            updateRevert();
          }
          if (update.docChanged || update.selectionSet) scheduleOccurrences();
        }),
      ],
    }),
  });
  // Lets tests edit through CodeMirror; DOM selection races its focus handling.
  problemList.attach(view);
  trace.attach(view);
  updateRevert();
  (window as { playground?: { view: EditorView } }).playground = { view };
  select.value = label;
  shareButton.disabled = false;
  startAnalysisWorker();
  replaceRunWorker();
}
init();
