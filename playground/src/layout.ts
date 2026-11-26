// The workspace layout: a divider that sets how much of the width the editor
// takes, and a toggle that gives it all of it, both kept across visits; and
// the tabs of the results beside it.
import { store, stored } from './drafts';

const minSplit = 20;
const maxSplit = 80;
const defaultSplit = 60;
const keyStep = 5;

const clamp = (split: number) => Math.min(maxSplit, Math.max(minSplit, split));

/** Sets up the layout controls; the returned function shows the results again. */
export function layout(workspace: HTMLElement, divider: HTMLElement, toggle: HTMLButtonElement): () => void {
  let split = typeof stored('split') === 'number' ? clamp(stored('split') as number) : defaultSplit;
  const setSplit = (value: number) => {
    split = clamp(value);
    workspace.style.setProperty('--split', `${split}%`);
    divider.setAttribute('aria-valuenow', String(Math.round(split)));
  };
  divider.setAttribute('aria-valuemin', String(minSplit));
  divider.setAttribute('aria-valuemax', String(maxSplit));
  setSplit(split);

  divider.onpointerdown = event => {
    if (event.button !== 0) return;
    event.preventDefault();
    divider.setPointerCapture(event.pointerId);
    workspace.classList.add('resizing');
  };
  divider.onpointermove = event => {
    if (!divider.hasPointerCapture(event.pointerId)) return;
    const box = workspace.getBoundingClientRect();
    setSplit((event.clientX - box.left) / box.width * 100);
  };
  divider.onlostpointercapture = () => {
    workspace.classList.remove('resizing');
    store('split', split);
  };
  divider.onkeydown = event => {
    const step = { ArrowLeft: -keyStep, ArrowRight: keyStep }[event.key];
    const to = { Home: minSplit, End: maxSplit }[event.key];
    if (step === undefined && to === undefined) return;
    event.preventDefault();
    setSplit(to ?? split + step!);
    store('split', split);
  };
  divider.ondblclick = () => {
    setSplit(defaultSplit);
    store('split', split);
  };

  const setEditorOnly = (editorOnly: boolean) => {
    workspace.classList.toggle('editor-only', editorOnly);
    toggle.textContent = editorOnly ? 'Show results' : 'Hide results';
    toggle.setAttribute('aria-expanded', String(!editorOnly));
    store('editor-only', editorOnly);
  };
  setEditorOnly(stored('editor-only') === true);
  toggle.onclick = () => setEditorOnly(!workspace.classList.contains('editor-only'));
  return () => setEditorOnly(false);
}

/**
 * Sets up the tabs of `tablist`, each a `role="tab"` button controlling its
 * panel; the returned function selects the one with id `id`.
 */
export function tabs(tablist: HTMLElement, onSelect: (id: string) => void = () => {}): (id: string) => void {
  const all = Array.from(tablist.querySelectorAll<HTMLButtonElement>('[role=tab]'));
  const select = (id: string) => {
    for (const tab of all) {
      const selected = tab.id === id;
      tab.setAttribute('aria-selected', String(selected));
      tab.tabIndex = selected ? 0 : -1;
      document.getElementById(tab.getAttribute('aria-controls')!)!.hidden = !selected;
    }
    onSelect(id);
  };
  for (const [index, tab] of all.entries()) {
    tab.onclick = () => select(tab.id);
    tab.onkeydown = event => {
      const to = { ArrowLeft: index - 1, ArrowRight: index + 1, Home: 0, End: all.length - 1 }[event.key];
      if (to === undefined) return;
      event.preventDefault();
      const next = all[(to + all.length) % all.length];
      select(next.id);
      next.focus();
    };
  }
  return select;
}
