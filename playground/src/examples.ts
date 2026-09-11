// Files are named with a numeric prefix (01-, 02-, ...) to fix their display
// order; the prefix itself carries no meaning beyond sort order. Each file's
// first line is a `# title: ...` comment naming the example, stripped below
// along with the file's trailing newline.
const files = import.meta.glob('./examples/*.dol', { eager: true, query: '?raw', import: 'default' }) as Record<string, string>;

const titleLine = /^# title: (.+)\n/;

function parse(path: string, source: string): [title: string, body: string] {
  const match = titleLine.exec(source);
  if (!match) throw new Error(`${path}: missing leading "# title: ..." comment`);
  return [match[1], source.slice(match[0].length).replace(/\n$/, '')];
}

export const examples: Record<string, string> = Object.fromEntries(
  Object.keys(files).sort().map(path => parse(path, files[path])),
);
