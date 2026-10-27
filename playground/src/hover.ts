// Hover tooltips from `Analyzed.hover`, formatted as `dolang-lsp` formats its
// hover markdown (`backend.rs`: `Backend::hover`, `Described`,
// `declaration_label`; `signature.rs`), then rendered to HTML.
import MarkdownIt from 'markdown-it';
import type { Declaration, Hover, Param, Signature, TypeLine } from './protocol';

/** One declaration's docs, as hover shows them */
export interface Described {
  /** Its signature */
  label: string;
  /** Where it's declared, as "in module `m`" or "in class `m.C`" */
  note?: string;
  /** Its doc comment, as Markdown */
  prose: string;
  shape: 'signature' | 'value' | 'other';
}

/** An item of a bundled module, as `Declaration` names it */
export type BundledKey = Omit<Extract<Declaration, { kind: 'bundled' }>, 'kind'>;

/** Finds a bundled item's docs, each overload's in turn, or none. */
export type Bundled = (key: BundledKey) => Described[];

/** The widest a signature is written on one line (`signature::WIDTH`) */
const width = 80;

// `Param::render`: the parameter with `@` written as `at`
function param(param: Param, at: string): string {
  let text = param.head;
  if (param.type !== undefined) text += at + param.type;
  if (param.default) text += ' = …';
  return text;
}
const alone = (p: Param) => param(p, ' @ ');

// `signature::def`: `prefix`, then the parameters and return type, vertical
// when too wide for one line
function def(prefix: string, params: Param[], returns?: string): string {
  const arrow = returns === undefined ? '' : ` -> ${returns}`;
  const line = params.length === 0 ? `${prefix}${arrow}`
    : params.length === 1 ? `${prefix} ${alone(params[0])}${arrow}`
    : `${prefix} ${params.map(p => param(p, '@')).join(' ')}${arrow}`;
  if ([...line].length <= width) return line;
  return `${prefix}${params.map(p => `\n  ${alone(p)}`).join('')}\ndo${arrow}`;
}

// `signature::binding`
const binding = (binding: string, type?: string) => type === undefined ? binding : `${binding} @ ${type}`;

/** A declaration's signature, as `declaration_label` writes it. */
export function signature(signature: Signature): string {
  const pub = (is: boolean) => is ? 'pub ' : '';
  const decorators = (items: string[]) => items.map(item => `#[${item}] `).join('');
  const named = (name: string, binders: string[]) => binders.length ? `${name}[${binders.join(', ')}]` : name;
  switch (signature.form) {
    case 'class': {
      const supers = signature.supers.length ? `: ${signature.supers.join(' ')}` : '';
      const at = signature.typeOnly ? '@' : '';
      return `${decorators(signature.decorators)}${pub(signature.pub)}${at}class ${named(signature.name, signature.binders)}${supers}`;
    }
    case 'def': {
      const at = signature.typeOnly ? '@' : '';
      const prefix = `${decorators(signature.decorators)}${pub(signature.pub)}${at}def ${named(signature.name, signature.binders)}`;
      return def(prefix, signature.params, signature.returns);
    }
    case 'field':
    case 'let':
      return binding(`${pub(signature.pub)}${signature.form} ${signature.name}`, signature.type);
    case 'param':
      return alone(signature.param);
  }
}

// `Described::render`: a fenced label, where it's declared, then its doc
// comment. Below a `typed` block, a value's label repeats it, so only its doc
// comment is left, and nothing when it has none.
function render(described: Described, typed: boolean): string | undefined {
  if (typed && described.shape === 'value') return described.prose || undefined;
  const fence = described.label.includes('```') ? '````' : '```';
  let markdown = `${fence}dolang\n${described.label}\n${fence}`;
  if (described.note) markdown += `\n\n*${described.note}*`;
  if (described.prose) markdown += `\n\n${described.prose}`;
  return markdown;
}

// `label`'s lines: a name and its type, and the overload a call chose
function typeLine(line: TypeLine): string {
  const typed = line.name === undefined ? line.type : `${line.name} @ ${line.type}`;
  return line.overload === undefined ? typed : `${typed}  # overload ${line.overload + 1}`;
}

/**
 * Hover's Markdown, as `Backend::hover` assembles it: the checked types, then
 * each declaration's docs, or `undefined` for nothing to show.
 */
export function markdown(hover: Hover, bundled: Bundled): string | undefined {
  const found = hover.declarations.map(declaration => declaration.kind === 'local'
    ? [{
      label: signature(declaration.signature),
      note: declaration.note,
      prose: declaration.doc,
      shape: declaration.shape,
    }]
    : bundled(declaration));
  const docs = found.flat();
  // An undescribed bundled item leaves its checked type the only description
  const complete = hover.complete && found.every(described => described.length > 0);
  // Signatures say what the checked type would
  const signatures = complete && docs.length > 0 && docs.every(doc => doc.shape === 'signature');
  const rendered = (typed: boolean) => {
    const parts = docs.map(doc => render(doc, typed)).filter(part => part !== undefined);
    return parts.length ? parts.join('\n\n---\n\n') : undefined;
  };
  if (hover.types.length === 0) return rendered(false);
  const typed = `\`\`\`dolang\n${hover.types.map(typeLine).join('\n')}\n\`\`\``;
  const docsMarkdown = rendered(!signatures);
  if (docsMarkdown === undefined) return typed;
  return signatures ? docsMarkdown : `${typed}\n\n---\n\n${docsMarkdown}`;
}

/**
 * Renders hover Markdown to HTML. Raw HTML in doc comments is escaped and
 * unsafe link schemes are refused, since a shared link's doc comments are
 * someone else's.
 */
export function html(markdown: string): string {
  return new MarkdownIt({ html: false }).render(markdown);
}
