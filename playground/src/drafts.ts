// What the page keeps in `localStorage`: the editor buffer, so a reload or an
// accidental back-navigation doesn't lose it, and the layout. Storage may be
// unavailable or full; everything kept is a convenience, so failures are
// ignored.

const prefix = 'dolang-playground:';

/** The value kept under `key`, if there is a readable one */
export function stored(key: string): unknown {
  try {
    return JSON.parse(localStorage.getItem(prefix + key) ?? 'null') ?? undefined;
  } catch {
    return undefined;
  }
}

/** Keeps `value` under `key` */
export function store(key: string, value: unknown) {
  try {
    localStorage.setItem(prefix + key, JSON.stringify(value));
  } catch {
    // Nothing to do
  }
}

/** A saved buffer and the example label it was last loaded from */
export interface Draft { doc: string; label: string }

/** The saved draft, if there is one */
export function loadDraft(): Draft | undefined {
  const value = stored('draft') as Partial<Draft> | undefined;
  if (typeof value?.doc === 'string' && typeof value.label === 'string') return { doc: value.doc, label: value.label };
  return undefined;
}

export const saveDraft = (draft: Draft) => store('draft', draft);
