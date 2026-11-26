// The Help dialog: keyboard shortcuts, what the playground provides, and an
// inventory of the bundled modules, each summarized from its doc.
import type { ModuleJson } from './docs';
import { markup } from './markup';

const mac = /Mac|iPhone|iPad/.test(navigator.platform);

// The first paragraph of a module's doc, with its links reduced to their text
function summary(doc: string): string {
  const paragraph = doc.split(/\n\s*\n/)[0].replace(/\s+/g, ' ').trim();
  return paragraph.replace(/\[([^\]]+)\]\([^)]*\)/g, '$1');
}

/** Opens `dialog` from `button`, summarizing its modules from the docs at `docs` the first time */
export function help(button: HTMLButtonElement, dialog: HTMLDialogElement, docs: URL) {
  dialog.querySelectorAll<HTMLElement>('kbd[data-mod]').forEach(key => { key.textContent = mac ? '⌘' : 'Ctrl'; });
  let summarized = false;
  button.onclick = () => {
    dialog.showModal();
    if (summarized) return;
    summarized = true;
    dialog.querySelectorAll<HTMLElement>('[data-module]').forEach(item => {
      const module = item.dataset.module!;
      fetch(new URL(`modules/${encodeURIComponent(module)}.json`, docs))
        .then(response => response.ok ? response.json() as Promise<ModuleJson> : undefined)
        .then(json => {
          if (!json?.doc) return;
          // Before any note on what the playground leaves out
          item.querySelector('.module-summary')!.prepend(...markup(summary(json.doc)));
        })
        .catch(() => {
          // A module without its summary is still listed
        });
    });
  };
  // A click outside the dialog's box closes it
  dialog.onclick = event => { if (event.target === dialog) dialog.close(); };
}
