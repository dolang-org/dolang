// The backtrace of the error a run raised: its frames, innermost first, each
// with the code it was running, those in the source linked to the editor, and
// the code that raised marked there until it changes.
import { ChangeSet, StateEffect, StateField, type Extension, type Text } from '@codemirror/state';
import { Decoration, EditorView, type DecorationSet } from '@codemirror/view';
import type { RunError, RunFrame } from './protocol';
import { snippet } from './problems';

const raisedEffect = StateEffect.define<{ from: number, to: number } | undefined>();
const raisedLine = Decoration.line({ class: 'cm-raised-line' });
const raisedMark = Decoration.mark({ class: 'cm-raised' });
const raised = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(value, transaction) {
    if (transaction.docChanged) value = Decoration.none;
    for (const effect of transaction.effects) {
      if (effect.is(raisedEffect)) {
        const span = effect.value;
        value = span ? Decoration.set([raisedLine.range(transaction.state.doc.lineAt(span.from).from),
          ...span.from < span.to ? [raisedMark.range(span.from, span.to)] : []], true) : Decoration.none;
      }
    }
    return value;
  },
  provide: field => EditorView.decorations.from(field),
});

const element = <K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};

// A frame's function, qualified by its module unless it's in the source
function name(frame: RunFrame): HTMLElement {
  const node = element('code', 'frame-name');
  const here = frame.at && 'from' in frame.at;
  if (!here) node.append(element('span', 'frame-module', frame.module), element('span', 'frame-dot', '.'));
  if (frame.method !== undefined) {
    node.append(frame.receiver, element('span', 'frame-dot', '.'), element('span', 'frame-function', frame.method));
  } else {
    node.append(element('span', 'frame-function', frame.receiver));
  }
  return node;
}

/** The backtrace list and the editor extension marking where it raised */
export function backtrace(list: HTMLElement) {
  let view: EditorView;
  // The source of the latest run, and the edits made to it since it started
  let ran: Text | undefined;
  let edits = ChangeSet.empty(0);
  let frames: RunFrame[] = [];

  const line = (doc: Text, pos: number) => doc.lineAt(pos).number;

  function frame(entry: RunFrame, index: number, width: number, shown: Set<string>): HTMLElement {
    const item = element('li', 'frame');
    item.style.setProperty('--index-width', `${width}ch`);
    const head = element('div', 'frame-head');
    head.append(element('span', 'frame-index', String(index).padStart(width)), name(entry));
    const at = entry.at;
    if (at && 'from' in at && ran) {
      const place = element('button', 'frame-place', `line ${line(ran, at.from)}`);
      place.onclick = () => {
        const anchor = edits.mapPos(at.from, 1);
        view.dispatch({ selection: { anchor, head: Math.max(anchor, edits.mapPos(at.to, -1)) }, scrollIntoView: true });
        view.focus();
      };
      head.append(place);
      item.append(head);
      // Each frame's code once, as the shell shows it
      const key = `${at.from}:${at.to}`;
      if (!shown.has(key)) {
        shown.add(key);
        item.append(snippet(ran, [{ from: at.from, to: at.to, primary: true, message: '' }], 'error'));
      }
    } else {
      if (at && 'path' in at) head.append(element('span', 'frame-place', `${at.path}:${at.line}:${at.column}`));
      item.append(head);
    }
    return item;
  }

  return {
    extension: [
      raised,
      EditorView.updateListener.of(update => { if (update.docChanged) edits = edits.compose(update.changes); }),
    ] as Extension,
    attach(editor: EditorView) { view = editor; },
    /** Notes the source a run starts with, which its frames refer to */
    started() {
      ran = view.state.doc;
      edits = ChangeSet.empty(ran.length);
    },
    /** Shows the backtrace of `error`, raised by the latest run */
    show(error: RunError) {
      frames = error.frames;
      const width = String(Math.max(0, frames.length - 1)).length;
      const shown = new Set<string>();
      list.replaceChildren(...frames.map((entry, index) => frame(entry, index, width, shown)));
      const innermost = frames.find(entry => entry.at && 'from' in entry.at)?.at;
      if (innermost && 'from' in innermost && edits.empty) view.dispatch({ effects: raisedEffect.of(innermost) });
    },
    clear() {
      frames = [];
      list.replaceChildren();
      view?.dispatch({ effects: raisedEffect.of(undefined) });
    },
    /** The backtrace as text, numbered as the list numbers it */
    text(): string {
      const width = String(Math.max(0, frames.length - 1)).length;
      return frames.map((entry, index) => {
        const at = entry.at;
        const here = at && 'from' in at;
        const qualified = `${here ? '' : `${entry.module}.`}${entry.receiver}${entry.method !== undefined ? `.${entry.method}` : ''}`;
        const place = here && ran ? ` at line ${line(ran, at.from)}` : at && 'path' in at ? ` at ${at.path}:${at.line}:${at.column}` : '';
        return `  ${String(index).padStart(width)}: ${qualified}${place}\n`;
      }).join('');
    },
  };
}
