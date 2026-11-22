// Each example lives in a group directory under `examples/`, labeled from
// `groupLabels`, whose order is the display order of the groups. Within a
// group, files are sorted by name. Each file's first line is a
// `# title: ...` comment naming the example, stripped below along with the
// file's trailing newline. Titles are unique across groups, since they also
// name the selected example.
const files = import.meta.glob('./examples/*/*.dol', { eager: true, query: '?raw', import: 'default' }) as Record<string, string>;

const groupLabels: Record<string, string> = {
  language: 'Language',
  libraries: 'Libraries',
  types: 'Type Checking',
};

const titleLine = /^# title: (.+)\n/;
const groupDir = /^\.\/examples\/([^/]+)\//;

function parse(path: string, source: string): [title: string, body: string] {
  const match = titleLine.exec(source);
  if (!match) throw new Error(`${path}: missing leading "# title: ..." comment`);
  return [match[1], source.slice(match[0].length).replace(/\n$/, '')];
}

/** Example sources by title */
export const examples: Record<string, string> = {};
/** Example titles in display order, by group label */
export const exampleGroups = new Map<string, string[]>(Object.values(groupLabels).map(label => [label, []]));

for (const path of Object.keys(files).sort()) {
  const dir = groupDir.exec(path)![1];
  const label = groupLabels[dir];
  if (!label) throw new Error(`${path}: no label for example group "${dir}"`);
  const [title, body] = parse(path, files[path]);
  if (title in examples) throw new Error(`${path}: duplicate example title "${title}"`);
  examples[title] = body;
  exampleGroups.get(label)!.push(title);
}
