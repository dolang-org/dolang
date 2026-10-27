// Hover tooltips from `Analyzed.hover`, formatted as `dolang-lsp` formats its
// hover markdown (`backend.rs`: `Backend::hover`, `Described`,
// `declaration_label`; `signature.rs`), then rendered to HTML.
import MarkdownIt from 'markdown-it';
import type { Declaration, Hover, Param, Signature, TypeLine } from './protocol';

/** One declaration's docs, as hover shows them */
export interface Described {
  /** Its signature, in its class and noted with its module */
  label: string;
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
  if (param.default) text += ' = ...';
  return text;
}
const alone = (p: Param) => param(p, ' @ ');

// `Param::binding`: the parameter as a `let` binds it, without its default
const paramBinding = (p: Param) => `${binding(`let ${p.head}`, p.type)} = ...`;

// `signature::def`: `prefix`, then the parameters and return type, vertical
// when too wide for one line, and a `...` body when it has a `body`
function def(prefix: string, params: Param[], returns: string | undefined, body: boolean): string {
  const arrow = returns === undefined ? '' : ` -> ${returns}`;
  const line = params.length === 0 ? `${prefix}${arrow}`
    : params.length === 1 ? `${prefix} ${alone(params[0])}${arrow}`
    : `${prefix} ${params.map(p => param(p, '@')).join(' ')}${arrow}`;
  // Decorators are lines of their own
  const text = [...line.slice(line.lastIndexOf('\n') + 1)].length <= width ? line
    : `${prefix}${params.map(p => `\n  ${alone(p)}`).join('')}\ndo${arrow}`;
  return body ? `${text}\n  ...` : text;
}

// `signature::binding`
const binding = (binding: string, type?: string) => type === undefined ? binding : `${binding} @ ${type}`;

/**
 * `signature::member`: `label` in the class whose header is `header`, if it's
 * a member, and noted as in `module` when that's given.
 */
export function member(module: string | undefined, header: string | undefined, label: string): string {
  const noted = module === undefined ? '' : `# module ${module}\n`;
  if (header === undefined) return `${noted}${label}`;
  return `${noted}${header}${label.split('\n').map(line => `\n  ${line}`).join('')}`;
}

/** A declaration's signature, as `declaration_label` writes it. */
export function signature(signature: Signature): string {
  const pub = (is: boolean) => is ? 'pub ' : '';
  const decorators = (items: string[]) => items.map(item => `#[${item}]\n`).join('');
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
      return def(prefix, signature.params, signature.returns, signature.body);
    }
    case 'field':
      return binding(`${pub(signature.pub)}field ${signature.name}`, signature.type);
    case 'let':
      return `${binding(`${pub(signature.pub)}let ${signature.name}`, signature.type)} = ...`;
    case 'param':
      return paramBinding(signature.param);
  }
}

// `Described::render`: a fenced label, then its doc comment. Below a `typed`
// block, a value's label repeats it, so only its doc comment is left, and
// nothing when it has none.
function render(described: Described, typed: boolean): string | undefined {
  if (typed && described.shape === 'value') return described.prose || undefined;
  const fence = described.label.includes('```') ? '````' : '```';
  let markdown = `${fence}dolang\n${described.label}\n${fence}`;
  if (described.prose) markdown += `\n\n${described.prose}`;
  return markdown;
}

// `signature::typed`: a line of what the check concluded. A qualified name
// binds its last part, and is kept in a comment.
function typeLine(line: TypeLine): string {
  const name = line.name === undefined ? '_' : line.name.slice(line.name.lastIndexOf('.') + 1);
  const notes = [];
  if (line.name?.includes('.')) notes.push(line.name);
  if (line.overload !== undefined) notes.push(`overload ${line.overload + 1}`);
  const typed = `let ${name} @ ${line.type} = ...`;
  return notes.length ? `${typed}  # ${notes.join(', ')}` : typed;
}

/**
 * Hover's Markdown, as `Backend::hover` assembles it: the checked types, then
 * each declaration's docs, or `undefined` for nothing to show.
 */
export function markdown(hover: Hover, bundled: Bundled): string | undefined {
  const found = hover.declarations.map(declaration => declaration.kind === 'local'
    ? [{
      label: member(undefined, declaration.class, signature(declaration.signature)),
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
 * someone else's. `link` says where each link goes, or leaves it as text;
 * without it, only web links stay. Links open apart from the playground, so
 * following one doesn't lose the edit.
 */
export function html(markdown: string, link?: (href: string) => string | undefined): string {
  const md = new MarkdownIt({ html: false });
  md.core.ruler.push('doc_links', state => {
    for (const block of state.tokens) {
      if (!block.children) continue;
      let unlinked = false;
      block.children = block.children.filter(token => {
        if (token.type === 'link_close' && unlinked) {
          unlinked = false;
          return false;
        }
        if (token.type !== 'link_open') return true;
        const href = token.attrGet('href') ?? '';
        const target = link ? link(href) : /^(https?|mailto):/i.test(href) ? href : undefined;
        if (target === undefined) {
          unlinked = true;
          return false;
        }
        token.attrSet('href', target);
        token.attrSet('target', '_blank');
        token.attrSet('rel', 'noopener');
        return true;
      });
    }
  });
  return md.render(markdown);
}
