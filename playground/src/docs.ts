// Bundled items' hover docs, from the per-module doc JSON `dodo` ships with
// the page (`extract_playground_docs` in tools/docgen.dol). This mirrors the
// LSP's consumers of the same JSON: `dolang-lsp/build.rs` (the index and type
// rendering), `doc_markdown.rs` (prose cleanup), `src/doc_index.rs`
// (`signature`), and `src/backend.rs` (`Described::entry`, `bundled_hover`).
// Links follow `docs/hooks/inline_autorefs_hook.py` and the mkdocstrings
// handler's page layout.
import { signature, type BundledKey, type Described } from './hover';
import type { Param } from './protocol';

/** A module's doc projection, as `dolang -m compile extract --doc` writes it */
export interface ModuleJson {
  module: string;
  doc?: string | null;
  entities?: Entity[];
}
export interface Entity {
  kind: string;
  name: string;
  pub?: boolean;
  doc?: string | null;
  binders?: BinderJson[];
  /** A class's `final` and `mixable` decorators */
  decorators?: string[];
  params?: ParamJson[];
  members?: Entity[];
  /** A re-export's source module and item */
  module?: string;
  item?: string;
  /** A field's or alias's type */
  type?: TypeJson | null;
  /** A function or method's return type */
  returns?: TypeJson | null;
}
export interface ParamJson {
  type_spread?: boolean;
  kind: string;
  /** The name, or the key of a keyword parameter; absent for an anonymous rest */
  name?: string | null;
  optional?: boolean;
  type?: TypeJson | null;
}
export interface BinderJson {
  kind: string;
  name: string;
  bound?: TypeJson | null;
  default?: TypeJson | null;
}
export type TypeJson =
  | { kind: 'name'; name: string }
  | { kind: 'const'; text: string }
  | { kind: 'app'; base: TypeJson; args: TypeArgJson[] }
  | { kind: 'schema'; params: TypeParamJson[] }
  /** A parenthesized tuple or record form */
  | { kind: 'parens'; params: TypeParamJson[] }
  /** The `[T]` form */
  | { kind: 'array'; elem: TypeJson }
  | { kind: 'union'; members: TypeJson[] }
  | { kind: 'func'; params: TypeParamJson[]; input?: TypeJson | null; output?: TypeJson | null; ret: TypeJson };
export interface TypeArgJson {
  kind: string;
  /** The name of a keyword argument */
  name?: string | null;
  type: TypeJson;
}
export interface TypeParamJson {
  kind: string;
  /** How many of the element the item admits, absent for exactly one */
  quant?: string | null;
  key?: string | null;
  /** The key type of a schema key given as a type */
  key_type?: TypeJson | null;
  type?: TypeJson | null;
}

/** Each public module with the classes that get their own API page */
export type Pages = Record<string, string[]>;

// The sigil that declares a parameter or binder of the given kind
function sigil(kind: string): string {
  switch (kind) {
    case 'key': return ':';
    case 'mixed_rest': return '...';
    case 'pos_rest': return '*';
    case 'key_rest': return '**';
    case 'input': return '<';
    case 'output': return '>';
    default: return '';
  }
}

// The sigil a schema item's quantifier is written with
function quantSigil(quant?: string | null): string {
  switch (quant) {
    case 'opt': return '?';
    case 'star': return '*';
    case 'include': return '...';
    default: return '';
  }
}

// How tightly a type form binds, so that it is parenthesized where it would
// have to be in source
const Binding = { Func: 0, Union: 1, Compact: 2 } as const;
type Binding = (typeof Binding)[keyof typeof Binding];

/** A type as written in a position binding as tightly as `context` (`TypeJson::render`). */
export function typeText(type: TypeJson, context: Binding = Binding.Compact): string {
  let text: string;
  let binding: Binding;
  switch (type.kind) {
    case 'name': return type.name;
    case 'const': return type.text;
    case 'app':
      text = `${typeText(type.base)}[${argsText(type.args)}]`;
      binding = Binding.Compact;
      break;
    case 'schema':
      text = `{${paramsText(type.params)}}`;
      binding = Binding.Compact;
      break;
    case 'parens': {
      // A lone item without a quantifier would only group without its comma
      const [first, ...rest] = type.params;
      const lone = first !== undefined && rest.length === 0 && first.kind === 'pos' && !first.quant;
      text = `(${paramsText(type.params)}${lone ? ',' : ''})`;
      binding = Binding.Compact;
      break;
    }
    case 'array':
      text = `[${typeText(type.elem, Binding.Func)}]`;
      binding = Binding.Compact;
      break;
    case 'union':
      text = type.members.map(member => typeText(member)).join(' | ');
      binding = Binding.Union;
      break;
    case 'func':
      text = `(${listText(type.params, type.input, type.output)}) -> ${typeText(type.ret, Binding.Func)}`;
      binding = Binding.Func;
      break;
  }
  return binding < context ? `(${text})` : text;
}

function argsText(args: TypeArgJson[]): string {
  return args.map(arg => {
    const type = typeText(arg.type, Binding.Func);
    if (arg.kind === 'expand') return `...${type}`;
    if (arg.kind === 'key' && arg.name) return `${arg.name}: ${type}`;
    return type;
  }).join(', ');
}

// A function type's parameters, with its implicits after them
function listText(params: TypeParamJson[], input?: TypeJson | null, output?: TypeJson | null): string {
  const implicits = ([['<', input], ['>', output]] as const)
    .filter(([, type]) => type)
    .map(([sigil, type]) => `${sigil}${typeText(type!, Binding.Func)}`);
  return [...params.map(paramText), ...implicits].join(', ');
}

const paramsText = (params: TypeParamJson[]) => params.map(paramText).join(', ');

function paramText(param: TypeParamJson): string {
  const quant = quantSigil(param.quant);
  if (!param.type) {
    // An element standing alone
    return param.kind === 'open' ? '...' : param.kind === 'any_pos' ? '*' : '**';
  }
  const type = typeText(param.type, Binding.Func);
  if (param.kind === 'any_key') return `**${type}`;
  if (param.kind === 'key') {
    // A name must be parenthesized to not be taken as a symbol key
    if (param.key_type?.kind === 'name') return `${quant}(${typeText(param.key_type)}): ${type}`;
    if (param.key_type) return `${quant}${typeText(param.key_type)}: ${type}`;
    if (param.key) return `${quant}${param.key}: ${type}`;
  }
  return `${quant}${type}`;
}

// `BinderJson::render`: the binder as its declaration writes it
function binderText(binder: BinderJson): string {
  let text = `${sigil(binder.kind)}${binder.name}`;
  if (binder.bound) text += ` @ ${typeText(binder.bound)}`;
  if (binder.default) text += ` = ${typeText(binder.default)}`;
  return text;
}

// A parameter as `build.rs` writes it into the index. An implicit is written
// as its sigil and type with no `@` between, so it carries its whole spelling
// as the head.
function param(param: ParamJson): Param {
  const head = `${sigil(param.kind)}${param.name ?? ''}`;
  const type = param.type && `${param.type_spread ? '...' : ''}${typeText(param.type)}`;
  if (param.kind === 'input' || param.kind === 'output') {
    return { head: `${head}${type ?? ''}`, default: !!param.optional };
  }
  return { head, type: type || undefined, default: !!param.optional };
}

/** An entity's signature, as `doc_index::signature` writes it. */
export function label(module: string, entity: Entity): string {
  const binders = (entity.binders ?? []).map(binderText);
  const common = { pub: false, typeOnly: false, name: entity.name, binders };
  const type = entity.returns ?? entity.type;
  const typed = type ? typeText(type) : undefined;
  switch (entity.kind) {
    case 'module': return `module ${module}`;
    case 'class':
      return signature({ form: 'class', ...common, decorators: entity.decorators ?? [], supers: [] });
    case 'field': return signature({ form: 'field', pub: false, name: entity.name, type: typed });
    case 'alias': {
      const name = binders.length ? `${entity.name}[${binders.join(', ')}]` : entity.name;
      return `@let ${name} = ${typed ?? '...'}`;
    }
    case 'function':
    case 'method':
      return signature({
        form: 'def', ...common, decorators: [], params: (entity.params ?? []).map(param), returns: typed,
      });
    default: return signature({ form: 'let', pub: false, name: entity.name });
  }
}

/** Removes mkdocs attribute-list anchors from heading lines (`remove_manual_anchors`). */
export function removeManualAnchors(markdown: string): string {
  return markdown.replace(/^(.*?)[ \t]*\{:[ \t]*#([^\s}]+)[ \t]*\}[ \t]*$/gm, '$1');
}

/**
 * Removes playground markup from fenced code blocks (`remove_playground_markup`):
 * the `playground` tag goes, and so do the hidden `#>` lines.
 */
export function removePlaygroundMarkup(markdown: string): string {
  const out: string[] = [];
  // Indentation and marker of the open playground fence, if any
  let fence: { indent: string; marker: string } | undefined;
  for (const line of markdown.split(/(?<=\n)/)) {
    const content = line.replace(/[\r\n]+$/, '');
    const newline = line.slice(content.length);
    const rest = content.trimStart();
    const indent = content.slice(0, content.length - rest.length);
    if (fence) {
      const close = rest.trimEnd();
      if (close.length >= fence.marker.length && [...close].every(c => c === fence!.marker[0])) {
        fence = undefined;
      } else if (content.startsWith(fence.indent) && content.slice(fence.indent.length).startsWith('#>')) {
        continue;
      }
      out.push(line);
      continue;
    }
    const opening = /^(`{3,}|~{3,})\s*playground(?=\s|$)(.*)$/.exec(rest);
    if (opening) {
      const [, marker, after] = opening;
      out.push(`${indent}${marker}${after}${newline}`);
      fence = { indent, marker };
    } else {
      out.push(line);
    }
  }
  return out.join('');
}

// `hover_doc`: extracted doc Markdown, written for the mkdocs site, for hover
const prose = (doc?: string | null) => removePlaygroundMarkup(removeManualAnchors(doc ?? ''));

// `Described::entry`
function described(module: string, item: string, entity: Entity): Described {
  const dot = item.indexOf('.');
  const note = entity.kind === 'module' ? undefined
    : dot >= 0 ? `in class \`${module}.${item.slice(0, dot)}\``
    : `in module \`${module}\``;
  const shape = ['function', 'method', 'class'].includes(entity.kind) ? 'signature'
    : entity.kind === 'value' ? 'value'
    : 'other';
  return { label: label(module, entity), note, prose: prose(entity.doc), shape };
}

/** Bundled modules' docs, loaded a module at a time as hovers need them. */
export class Docs {
  readonly #modules = new Map<string, Promise<ModuleJson | undefined>>();

  constructor(readonly load: (module: string) => Promise<ModuleJson | undefined>) {}

  #module(name: string): Promise<ModuleJson | undefined> {
    let module = this.#modules.get(name);
    if (!module) {
      module = this.load(name).catch(() => undefined);
      this.#modules.set(name, module);
    }
    return module;
  }

  // `resolve_entity`: a re-export as what it names, under its own name. A
  // cycle or a missing source is left undescribed.
  async #resolve(module: string, entity: Entity, chain: string[] = []): Promise<Entity | undefined> {
    if (entity.kind === 'import_module') return { ...entity, kind: 'value' };
    if (entity.kind !== 'import_item') return entity;
    const key = `${module}.${entity.name}`;
    if (chain.includes(key) || !entity.module || !entity.item) return undefined;
    const source = await this.#module(entity.module);
    const target = source?.entities?.find(candidate => candidate.name === entity.item);
    if (!target) return undefined;
    const resolved = await this.#resolve(entity.module, target, [...chain, key]);
    return resolved && { ...resolved, name: entity.name, pub: entity.pub };
  }

  // The public entities `item` names in `module`, each overload in turn: the
  // module's own for an empty `item`, and a class's members for `Class.member`
  async #entities(module: string, item: string): Promise<Entity[]> {
    const json = await this.#module(module);
    if (!json) return [];
    if (item === '') return [{ kind: 'module', name: module, doc: json.doc }];
    const dot = item.indexOf('.');
    const name = dot < 0 ? item : item.slice(0, dot);
    const found = (json.entities ?? []).filter(entity => entity.pub && entity.name === name);
    const resolved = (await Promise.all(found.map(entity => this.#resolve(module, entity))))
      .filter(entity => entity !== undefined);
    if (dot < 0) return resolved;
    const member = item.slice(dot + 1);
    return resolved.flatMap(owner => (owner.members ?? []).filter(m => m.pub && m.name === member));
  }

  /**
   * A bundled item's docs, as `bundled_hover` finds them: the overload a call
   * chose, or each in turn.
   */
  async describe(key: BundledKey): Promise<Described[]> {
    const entities = await this.#entities(key.module, key.item);
    const chosen = entities.length > 1 && key.overload !== undefined ? entities[key.overload] : undefined;
    return (chosen ? [chosen] : entities).map(entity => described(key.module, key.item, entity));
  }
}

// `_looks_like_identifier` in the autorefs hook: what isn't a URL, a path or
// an in-page anchor
const scheme = /^[a-zA-Z][a-zA-Z0-9+.-]*:/;
function identifier(href: string): boolean {
  if (!href || './#'.includes(href[0]) || scheme.test(href)) return false;
  return !/\.(md|html)$/.test(href.split('#', 1)[0]);
}

/**
 * Where a doc link goes, relative to the playground page, which the site
 * serves from `playground/` beside `api/`; `undefined` to leave it as text.
 *
 * - An autoref identifier names the longest dotted prefix that is a module, as
 *   the mkdocstrings handler finds it, and is an anchor on that module's page
 *   or on the page of the class after it.
 * - A relative `.md` link is written from a module's API page, and all of
 *   those are siblings under `api/`; mkdocs serves `x.md` as `x/`.
 * - An absolute web link stays; anything else would be relative to the wrong
 *   page.
 */
export function link(href: string, pages?: Pages): string | undefined {
  if (/^(https?|mailto):/i.test(href)) return href;
  if (identifier(href)) {
    if (!pages) return undefined;
    const parts = href.split('.');
    for (let end = parts.length; end > 0; end--) {
      const module = parts.slice(0, end).join('.');
      const classes = Object.hasOwn(pages, module) ? pages[module] : undefined;
      if (!classes) continue;
      const page = end < parts.length && classes.includes(parts[end]) ? `${parts[end]}/` : '';
      return `../api/${module}/${page}#${href}`;
    }
    return undefined;
  }
  if (scheme.test(href) || href.startsWith('#') || href.startsWith('/')) return undefined;
  const url = new URL(href, 'https://site.invalid/api/_/index.md');
  if (!/\.(md|html)$/.test(url.pathname)) return undefined;
  const path = url.pathname.replace(/(^|\/)index\.md$/, '$1').replace(/\.md$/, '/');
  return `..${path}${url.hash}`;
}
