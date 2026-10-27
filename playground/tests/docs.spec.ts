// The doc JSON consumer, without a page: fixtures, and every entity of the
// docs the build ships.
import { expect, test } from '@playwright/test';
import { readFile, readdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import {
  Docs, label, link, removeManualAnchors, removePlaygroundMarkup, type Entity, type ModuleJson, type Pages,
} from '../src/docs';
import { html } from '../src/hover';

const fixture = (modules: ModuleJson[]) =>
  new Docs(async name => modules.find(module => module.module === name));

test('signatures include annotated types', () => {
  const name = (name: string) => ({ kind: 'name', name }) as const;
  const place: Entity = {
    kind: 'function',
    name: 'place',
    binders: [{ kind: 'pos', name: 'T' }, { kind: 'key', name: 'K' }, { kind: 'mixed_rest', name: 'R' }],
    params: [{ kind: 'pos', name: 'point', type: name('Point') }, { kind: 'key', name: 'limit', optional: true }],
    returns: { kind: 'union', members: [name('Int'), name('nil')] },
  };
  expect(label('m', place)).toBe('# module m\ndef place[T, :K, ...R] point@Point :limit = ... -> (Int | nil)\n  ...');
  const box: Entity = { kind: 'class', name: 'Box', binders: [{ kind: 'pos', name: 'T' }] };
  expect(label('m', box)).toBe('# module m\nclass Box[T]');
  // A member is in its class
  expect(label('m', { kind: 'field', name: 'x', type: name('Int') }, box))
    .toBe('# module m\nclass Box[T]\n  field x @ Int');
  expect(label('m', { kind: 'class', name: 'Sym', decorators: ['final', 'mixable'] }))
    .toBe('# module m\n#[final]\n#[mixable]\nclass Sym');
  expect(label('m', { kind: 'module', name: 'm' })).toBe('import m');
  expect(label('m', { kind: 'value', name: 'args' })).toBe('# module m\nlet args = ...');
  // A spread rest's element type keeps its parentheses
  const fork: Entity = {
    kind: 'function',
    name: 'fork',
    params: [{ kind: 'mixed_rest', name: 'thunks', type_spread: true, type: { kind: 'func', params: [], ret: name('Rs') } }],
  };
  expect(label('m', fork)).toBe('# module m\ndef fork ...thunks @ ...(() -> Rs)\n  ...');
});

test('prose drops playground markup and manual anchors', () => {
  expect(removePlaygroundMarkup('Text\n\n```playground\n#> import base64:\n#>   - encode\nencode hi\n```\n'))
    .toBe('Text\n\n```\nencode hi\n```\n');
  expect(removePlaygroundMarkup('- item\n\n  ````playground title=x\n  #> hidden\n  shown\n  ````\n'))
    .toBe('- item\n\n  ```` title=x\n  shown\n  ````\n');
  const kept = '```\n#> kept\n```\n```playgrounds\n#> kept\n```\n#> kept\n';
  expect(removePlaygroundMarkup(kept)).toBe(kept);
  expect(removeManualAnchors('## Busy retry {: #sqlite.Connection.busy-retry }\n\nText\n### Body {: #body}\n'))
    .toBe('## Busy retry\n\nText\n### Body\n');
  const braces = '## Red {.warning}\nUse {: #anchor } in prose.\nIncomplete {: #anchor\n';
  expect(removeManualAnchors(braces)).toBe(braces);
});

test('lookup follows re-exports, members and overloads', async () => {
  const docs = fixture([
    {
      module: 'a',
      doc: 'Module a.',
      entities: [
        { kind: 'import_item', name: 'Thing', pub: true, module: 'b', item: 'Inner' },
        { kind: 'import_item', name: 'Loop', pub: true, module: 'a', item: 'Loop' },
        { kind: 'import_item', name: 'Gone', pub: true, module: 'c', item: 'Gone' },
      ],
    },
    {
      module: 'b',
      entities: [
        {
          kind: 'class',
          name: 'Inner',
          pub: true,
          doc: 'An inner thing.',
          members: [
            { kind: 'method', name: '(init)', pub: true, params: [{ kind: 'pos', name: 'self' }], doc: 'Makes one.' },
            { kind: 'method', name: 'hidden', pub: false },
          ],
        },
        { kind: 'function', name: 'f', pub: true, params: [{ kind: 'pos', name: 'x', type: { kind: 'name', name: 'Int' } }] },
        { kind: 'function', name: 'f', pub: true, params: [{ kind: 'pos', name: 'x', type: { kind: 'name', name: 'Str' } }] },
        { kind: 'function', name: 'f', pub: true, params: [{ kind: 'pos', name: 'x' }], doc: 'Does f.' },
        { kind: 'function', name: 'private', pub: false },
      ],
    },
  ]);
  expect(await docs.describe({ module: 'a', item: '' }))
    .toEqual([{ label: 'import a', prose: 'Module a.', shape: 'other' }]);
  expect(await docs.describe({ module: 'a', item: 'Thing' }))
    .toEqual([{ label: '# module a\nclass Thing', prose: 'An inner thing.', shape: 'signature' }]);
  expect(await docs.describe({ module: 'a', item: 'Thing.(init)' })).toEqual([{
    label: '# module a\nclass Thing\n  def (init) self\n    ...', prose: 'Makes one.', shape: 'signature',
  }]);
  expect(await docs.describe({ module: 'a', item: 'Thing.hidden' })).toEqual([]);
  expect(await docs.describe({ module: 'a', item: 'Loop' })).toEqual([]);
  expect(await docs.describe({ module: 'a', item: 'Gone' })).toEqual([]);
  expect(await docs.describe({ module: 'b', item: 'private' })).toEqual([]);
  expect(await docs.describe({ module: 'missing', item: 'x' })).toEqual([]);
  const labels = async (overload?: number) =>
    (await docs.describe({ module: 'b', item: 'f', overload })).map(described => described.label);
  const f = (param: string) => `# module b\ndef f ${param}\n  ...`;
  expect(await labels()).toEqual([f('x @ Int'), f('x @ Str'), f('x')]);
  expect(await labels(1)).toEqual([f('x @ Str')]);
});

test('links go to API pages beside the playground', () => {
  const pages: Pages = { std: ['Int'], term: ['Text'], 'fs.unix': [] };
  expect(link('std.Int', pages)).toBe('../api/std/Int/#std.Int');
  expect(link('std.Int.(init)', pages)).toBe('../api/std/Int/#std.Int.(init)');
  expect(link('term.Text.formatting', pages)).toBe('../api/term/Text/#term.Text.formatting');
  expect(link('std.echo', pages)).toBe('../api/std/#std.echo');
  expect(link('fs.unix', pages)).toBe('../api/fs.unix/#fs.unix');
  expect(link('fs.unix.Mode', pages)).toBe('../api/fs.unix/#fs.unix.Mode');
  expect(link('../../language/strings.md#formatted-sequences', pages)).toBe('../language/strings/#formatted-sequences');
  expect(link('../sys/index.md', pages)).toBe('../api/sys/');
  expect(link('https://example.com/x', pages)).toBe('https://example.com/x');
  expect(link('nowhere.Thing', pages)).toBeUndefined();
  expect(link('std.Int', undefined)).toBeUndefined();
  expect(link('#local', pages)).toBeUndefined();
  expect(link('notes.txt', pages)).toBeUndefined();
});

test('every shipped entity describes, and its links resolve', async () => {
  const dir = fileURLToPath(new URL('../../target/playground/docs/', import.meta.url));
  const read = async <T>(path: string): Promise<T> => JSON.parse(await readFile(`${dir}${path}`, 'utf8'));
  const pages = await read<Pages>('pages.json');
  const docs = new Docs(module => read<ModuleJson>(`modules/${module}.json`));
  const files = await readdir(`${dir}modules`);
  expect(files).toContain('std.json');
  expect(files).toContain('test.json');
  const unresolved: string[] = [];
  let count = 0;
  for (const file of files) {
    const json = await read<ModuleJson>(`modules/${file}`);
    const items = new Set(['']);
    for (const entity of json.entities ?? []) {
      if (!entity.pub) continue;
      items.add(entity.name);
      for (const member of entity.members ?? []) if (member.pub) items.add(`${entity.name}.${member.name}`);
    }
    for (const item of items) {
      const described = await docs.describe({ module: json.module, item });
      expect(described.length, `${json.module}.${item}`).toBeGreaterThan(0);
      for (const { label, prose } of described) {
        count++;
        expect(label, `${json.module}.${item}`).not.toBe('');
        html(prose, href => link(href, pages) ?? void unresolved.push(`${json.module}.${item}: ${href}`));
      }
    }
  }
  expect(count).toBeGreaterThan(1000);
  expect(unresolved).toEqual([]);
});
