import init, { Analyzed } from '../pkg/dolang_wasm.js';
import wasmUrl from '../pkg/dolang_wasm_bg.wasm?url';
import type { AnalysisReply, AnalysisRequest, Diagnostic, Hover } from './protocol';
import { html, markdown, type BundledKey, type Described } from './hover';
import { Docs, link, type ModuleJson, type Pages } from './docs';

// How long edits must settle before the source is type checked, as in the LSP.
const checkDelayMs = 300;

const reply = (message: AnalysisReply) => self.postMessage(message);

// The latest source, compiled, and its type diagnostics once checked, with the
// request and page version it last answered. It's freed when a different
// source replaces it.
interface Entry { source: string; analyzed: Analyzed; id: number; version: number; diagnostics?: Diagnostic[] }
let current: Entry | undefined;
let checkTimer: ReturnType<typeof setTimeout> | undefined;

// Bundled items' docs and the API pages their links go to, once the page says
// where they are. Until then, bundled items are undescribed.
let docs: Docs | undefined;
let pages: Promise<Pages | undefined> = Promise.resolve(undefined);

async function fetchJson<T>(url: URL): Promise<T | undefined> {
  const response = await fetch(url);
  return response.ok ? response.json() as Promise<T> : undefined;
}

// Nearly every hover needs the prelude, so it's fetched along with the pages
function locateDocs(url: string) {
  pages = fetchJson<Pages>(new URL('pages.json', url)).catch(() => undefined);
  docs = new Docs(module => fetchJson<ModuleJson>(new URL(`modules/${encodeURIComponent(module)}.json`, url)));
  void docs.describe({ module: 'std', item: '' });
}

function check(entry: Entry) {
  clearTimeout(checkTimer);
  const diagnostics: Diagnostic[] = entry.analyzed.check();
  entry.diagnostics = diagnostics;
  reply({ type: 'checked', id: entry.id, version: entry.version, diagnostics });
}

function analyze(id: number, version: number, source: string) {
  clearTimeout(checkTimer);
  if (current?.source !== source) {
    const analyzed = new Analyzed(source);
    current?.analyzed.free();
    current = { source, analyzed, id, version };
  }
  const entry = current;
  entry.id = id;
  entry.version = version;
  const { analyzed } = entry;
  reply({ type: 'analysis', id, version, value: { tokens: analyzed.tokens(), diagnostics: analyzed.diagnostics() } });
  if (entry.diagnostics) {
    reply({ type: 'checked', id, version, diagnostics: entry.diagnostics });
    return;
  }
  checkTimer = setTimeout(() => check(entry), checkDelayMs);
}

// A hover for a replaced version gets no answer; the page has given up on it.
// One while a check is pending checks now, so it shows checked types. Bundled
// docs may have to be fetched, and a later analysis can free `analyzed` by
// then, so it's read first.
async function hover(id: number, version: number, pos: number) {
  const entry = current;
  if (entry?.version !== version) return;
  if (!entry.diagnostics) check(entry);
  const found: Hover | undefined = entry.analyzed.hover(pos, pos);
  if (!found) return reply({ type: 'hover', id, version });
  const keys = found.declarations.filter(declaration => declaration.kind === 'bundled');
  const described = await Promise.all(keys.map(key => docs?.describe(key) ?? []));
  // `markdown` asks for each by the declaration itself
  const bundled = new Map<BundledKey, Described[]>(keys.map((key, i) => [key, described[i]]));
  const text = markdown(found, key => bundled.get(key) ?? []);
  const resolved = await pages;
  const value = text === undefined ? undefined
    : { from: found.from, to: found.to, html: html(text, href => link(href, resolved)) };
  reply({ type: 'hover', id, version, value });
}

// Requests that arrive while loading wait for it; compiling is synchronous, so
// replies keep request order. Each analysis cancels the pending check of the
// one before it.
const loaded = init({ module_or_path: wasmUrl }).then(() => reply({ type: 'ready' }));
loaded.catch(error => reply({ type: 'failure', message: String(error) }));
self.onmessage = (event: MessageEvent<AnalysisRequest>) => {
  const request = event.data;
  loaded.then(() => {
    if (request.type === 'analyze') analyze(request.id, request.version, request.source);
    else if (request.type === 'docs') locateDocs(request.url);
    else return hover(request.id, request.version, request.pos);
  }).catch(error => reply({ type: 'failure', message: String(error) }));
};
