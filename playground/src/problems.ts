// The Problems panel: one card per diagnostic, with its source lines and
// labels drawn as `annotate-snippets` draws them, its notes and its fixes, and
// the editor's side of it: underlines, gutter markers, F8, and the labels of
// the card under the pointer.
import { StateEffect, StateField, type Extension, type Text } from '@codemirror/state';
import { Decoration, EditorView, keymap, type DecorationSet } from '@codemirror/view';
import {
  lintGutter, nextDiagnostic, previousDiagnostic, setDiagnostics, type Action, type Diagnostic as LintDiagnostic,
} from '@codemirror/lint';
import type { Diagnostic, Fix } from './protocol';
import { markup, plain } from './markup';

const lintSeverity = { error: 'error', warning: 'warning', lint: 'info' } as const;

// The spans of the card under the pointer or focus, marked in the editor
const labelEffect = StateEffect.define<{ from: number, to: number, primary: boolean }[]>();
const primaryMark = Decoration.mark({ class: 'cm-problem-span cm-problem-primary' });
const contextMark = Decoration.mark({ class: 'cm-problem-span' });
const labelMarks = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(value, transaction) {
    value = value.map(transaction.changes);
    for (const effect of transaction.effects) {
      if (effect.is(labelEffect)) {
        value = Decoration.set(effect.value.filter(span => span.from < span.to)
          .map(span => (span.primary ? primaryMark : contextMark).range(span.from, span.to)), true);
      }
    }
    return value;
  },
  provide: field => EditorView.decorations.from(field),
});

/** A span to draw under a line of a snippet */
export interface Span { from: number; to: number; primary: boolean; message: string }

const element = <K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};
// The width of `text` in columns, counting each code point as one
const columns = (text: string) => [...text].length;

const marked = <T extends HTMLElement>(node: T, text: string) => { node.append(...markup(text)); return node; };

// A span placed under its line, in columns
interface Placed { start: number; length: number; message: string; className: string }
// A piece of a row under a line: `text` at column `at`
// A message after it, if any, is marked up and hung there if `hung`.
interface Piece { at: number; text: string; className: string; message?: string; hung?: boolean }

// How wide a row may grow before a message is hung below its span instead
const inlineWidth = 72;

// The rows under a line marking `placed`, which don't overlap, in the manner
// of `annotate-snippets`: every underline on one row, the last span's message
// beside it if it fits, and the others' hung below, rightmost first, each
// from a stroke down from its span.
function underlines(placed: Placed[]): Piece[][] {
  const last = placed.at(-1)!;
  const inline = last.message && last.start + last.length + 1 + columns(plain(last.message)) <= inlineWidth ? last : undefined;
  const hung = placed.filter(span => span.message && span !== inline);
  const marks = placed.map(span => ({ at: span.start, text: (span.className.includes('context') ? '─' : '━').repeat(span.length), className: span.className,
    message: span === inline ? ` ${span.message}` : undefined }));
  const stroke = (span: Placed) => ({ at: span.start, text: '│', className: span.className });
  const rows: Piece[][] = [marks];
  if (hung.length) rows.push(hung.map(stroke));
  for (let index = hung.length - 1; index >= 0; index--) {
    const span = hung[index];
    rows.push([...hung.slice(0, index).map(stroke), { at: span.start, text: '╰╴', className: span.className, message: span.message, hung: true }]);
  }
  return rows;
}

// Splits spans into runs that don't overlap, each drawn on its own rows.
function runs(placed: Placed[]): Placed[][] {
  const result: Placed[][] = [];
  for (const span of placed) {
    const run = result.find(run => run.at(-1)!.start + run.at(-1)!.length <= span.start);
    if (run) run.push(span);
    else result.push([span]);
  }
  return result;
}

/**
 * The source lines `spans` start on, each with rows under it marking them, as
 * `annotate-snippets` draws them with Unicode, and a gap marked where lines
 * are skipped. Line numbers are a column of their own.
 */
export function snippet(doc: Text, spans: Span[], severity: string): HTMLElement {
  const grid = element('div', 'snippet');
  const row = (number: string, ...content: (Node | string)[]) => {
    const text = element('code', 'snippet-text');
    text.append(...content);
    grid.append(element('span', 'snippet-number', number), text);
  };
  const lines = [...new Set(spans.map(span => doc.lineAt(span.from).number))].sort((a, b) => a - b);
  let previous: number | undefined;
  for (const number of lines) {
    if (previous !== undefined && number > previous + 1) row('⋮');
    previous = number;
    const line = doc.line(number);
    row(String(number), line.text);
    const placed = spans.filter(span => doc.lineAt(span.from).number === number)
      .map(span => ({
        start: columns(line.text.slice(0, span.from - line.from)),
        length: Math.max(1, columns(line.text.slice(span.from - line.from, Math.min(span.to, line.to) - line.from))),
        message: span.message,
        className: span.primary ? `snippet-mark ${severity}` : 'snippet-mark context',
      }))
      .sort((a, b) => a.start - b.start || b.length - a.length);
    for (const run of runs(placed)) {
      for (const pieces of underlines(run)) {
        let at = 0;
        const content: (Node | string)[] = [];
        for (const piece of pieces) {
          content.push(' '.repeat(Math.max(0, piece.at - at)), element('span', piece.className, piece.text));
          at = Math.max(at, piece.at) + columns(piece.text);
          // A hung message wraps within what's left of the row, at its column
          if (piece.message !== undefined) {
            const message = marked(element('span', piece.hung ? `${piece.className} snippet-hung` : piece.className), piece.message);
            if (piece.hung) message.style.maxWidth = `max(20ch, calc(100% - ${at}ch))`;
            content.push(message);
          }
        }
        row('', ...content);
      }
    }
  }
  return grid;
}

/** The Problems panel's list and the editor extensions it works with */
export function problems(list: HTMLElement, count: HTMLElement, docs: URL) {
  let view: EditorView;
  let shown: { diagnostic: Diagnostic, card: HTMLElement }[] = [];
  let stale = false;

  const apply = (fix: Fix, shift = 0) => {
    view.dispatch({ changes: { from: fix.from + shift, to: fix.to + shift, insert: fix.insert }, userEvent: 'input.fix' });
    view.focus();
  };
  // The spans a card marks in the editor
  const spans = (diagnostic: Diagnostic): Span[] => [
    { from: diagnostic.from, to: diagnostic.to, primary: true, message: '' },
    ...diagnostic.labels.flatMap(label => 'from' in label.at
      ? [{ from: label.at.from, to: label.at.to, primary: label.primary, message: label.message }]
      : []),
  ];

  function card(diagnostic: Diagnostic): HTMLElement {
    const item = element('li', `problem ${diagnostic.severity}`);
    const head = element('button', 'problem-head');
    head.append(element('span', 'problem-severity', diagnostic.severity), marked(element('span', 'problem-message'), diagnostic.message),
      element('span', 'problem-line', `line ${view.state.doc.lineAt(diagnostic.from).number}`));
    head.onclick = () => {
      if (stale) return;
      view.dispatch({ selection: { anchor: diagnostic.from, head: diagnostic.to }, scrollIntoView: true });
      view.focus();
    };
    item.append(head, snippet(view.state.doc, spans(diagnostic), diagnostic.severity));
    const notes = element('ul', 'problem-notes');
    for (const label of diagnostic.labels) {
      if ('from' in label.at || !label.message) continue;
      const note = element('li', 'problem-note elsewhere');
      const link = element('a', undefined, label.at.module);
      link.href = new URL(`${label.at.module}/`, docs).href;
      note.append('in ', link, ': ', ...markup(label.message));
      notes.append(note);
    }
    for (const note of diagnostic.notes) {
      const node = element('li', `problem-note ${note.kind}`);
      node.append(element('span', 'problem-note-kind', `${note.kind}: `), ...markup(note.message));
      notes.append(node);
    }
    if (notes.childElementCount) item.append(notes);
    if (diagnostic.fixes.length) {
      const fixes = element('div', 'problem-fixes');
      for (const fix of diagnostic.fixes) {
        const button = marked(element('button', 'problem-fix', 'Fix: '), fix.title);
        button.onclick = () => { if (!stale) apply(fix); };
        fixes.append(button);
      }
      item.append(fixes);
    }
    const mark = (on: boolean) => view.dispatch({ effects: labelEffect.of(on && !stale ? spans(diagnostic) : []) });
    item.onpointerenter = () => mark(true);
    item.onpointerleave = () => mark(false);
    item.addEventListener('focusin', () => mark(true));
    item.addEventListener('focusout', () => mark(false));
    return item;
  }

  // What the editor's tooltip and gutter show: the message, its notes, and
  // the fixes, applied where the diagnostic has moved to
  function lint(diagnostic: Diagnostic): LintDiagnostic {
    const actions: Action[] = diagnostic.fixes.map(fix => ({
      name: plain(fix.title),
      apply: (_, from, to) => {
        if (to - from === diagnostic.to - diagnostic.from) apply(fix, from - diagnostic.from);
      },
    }));
    return {
      from: diagnostic.from,
      to: diagnostic.to,
      severity: lintSeverity[diagnostic.severity],
      message: diagnostic.message,
      renderMessage() {
        const node = marked(element('div', 'problem-tip'), diagnostic.message);
        for (const note of diagnostic.notes) {
          node.append(marked(element('div', `problem-note ${note.kind}`, `${note.kind}: `), note.message));
        }
        return node;
      },
      actions,
    };
  }

  // Marks the card the cursor is in, scrolling it into the list's view
  function follow() {
    const head = view.state.selection.main.head;
    let current: HTMLElement | undefined;
    for (const { diagnostic, card } of shown) {
      const here = !stale && !current && diagnostic.from <= head && head <= diagnostic.to;
      card.classList.toggle('current', here);
      if (here) current = card;
    }
    const scroller = list.parentElement;
    if (current && scroller && scroller.offsetParent) {
      const top = current.offsetTop - scroller.offsetTop;
      if (top < scroller.scrollTop) scroller.scrollTop = top;
      else if (top + current.offsetHeight > scroller.scrollTop + scroller.clientHeight) {
        scroller.scrollTop = top + current.offsetHeight - scroller.clientHeight;
      }
    }
  }

  return {
    extension: [
      labelMarks,
      lintGutter(),
      keymap.of([{ key: 'F8', run: nextDiagnostic, shift: previousDiagnostic }]),
      EditorView.updateListener.of(update => { if (update.selectionSet) follow(); }),
    ] as Extension,
    attach(editor: EditorView) { view = editor; },
    /** Shows the problems of the current source */
    show(diagnostics: Diagnostic[]) {
      stale = false;
      list.classList.remove('stale');
      shown = diagnostics.map(diagnostic => ({ diagnostic, card: card(diagnostic) }));
      list.replaceChildren(...shown.map(({ card }) => card));
      count.textContent = diagnostics.length ? String(diagnostics.length) : '';
      view.dispatch(setDiagnostics(view.state, diagnostics.map(lint)), { effects: labelEffect.of([]) });
      follow();
    },
    /** Dims the problems shown, which are of an earlier source, until the next show */
    stale() {
      stale = true;
      list.classList.add('stale');
      follow();
    },
  };
}
