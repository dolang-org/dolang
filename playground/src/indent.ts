// Indentation rules for Do, read from the VS Code extension's language
// configuration and dedent rules so the two editors agree.
import { countColumn, EditorState, type Extension } from '@codemirror/state';
import { indentOnInput, indentService, indentUnit, type IndentContext } from '@codemirror/language';
import configuration from '../../dolang-code/language-configuration.json';
import { dedent, dedentFor, typed, type Line } from '../../dolang-code/src/indent';

interface EnterRule { beforeText: RegExp; indent: 'indent' | 'outdent' | 'none' }

const enterRules: EnterRule[] = configuration.onEnterRules.map(rule => ({
  beforeText: new RegExp(rule.beforeText),
  indent: rule.action.indent as EnterRule['indent'],
}));
const blank = (text: string) => !/\S/.test(text);

// The indentation of the line starting at `pos`. When the editor breaks a
// line, the rules look at the text before the cursor, as VS Code's
// `beforeText` does; otherwise at the whole line before.
function indentation(cx: IndentContext, pos: number): number | null {
  const line = cx.lineAt(pos, 1);
  if (line.from === 0) return 0;
  const breaking = cx.simulatedBreak === pos;
  const indent = byRules(cx, breaking ? cx.lineAt(pos, -1) : cx.lineAt(line.from - 1, -1), breaking);
  // A line being broken off gets the indentation it would have; one being
  // typed keeps the one it has
  const rule = dedentFor(line.text);
  return rule ? dedent(rule, linesBefore(cx, line.from), breaking ? indent : cx.lineIndent(line.from, 1), cx.unit) : indent;
}

// The indentation the rules give a line after `before`.
function byRules(cx: IndentContext, before: { text: string, from: number }, breaking: boolean): number {
  const indent = countColumn(/^\s*/.exec(before.text)![0], cx.state.tabSize);
  // A line left blank keeps its indentation, so a block stays open across it
  if (blank(before.text)) return breaking ? indent : previousIndent(cx, before.from);
  const rule = enterRules.find(rule => rule.beforeText.test(before.text));
  switch (rule?.indent) {
    case 'indent': return indent + cx.unit;
    case 'outdent': return Math.max(0, indent - cx.unit);
    default: return indent;
  }
}

// The indentation of the nearest non-blank line before the one at `from`.
function previousIndent(cx: IndentContext, from: number): number {
  while (from > 0) {
    const line = cx.lineAt(from - 1, -1);
    if (!blank(line.text)) return cx.lineIndent(line.from, -1);
    from = line.from;
  }
  return 0;
}

// The lines before the one at `from`, nearest first
function* linesBefore(cx: IndentContext, from: number): Generator<Line> {
  while (from > 0) {
    const line = cx.lineAt(from - 1, -1);
    yield { text: line.text, indent: cx.lineIndent(line.from, -1) };
    from = line.from;
  }
}

/** Do's indentation: two spaces, by the extension's rules */
export function doIndentation(): Extension {
  return [
    indentUnit.of('  '),
    EditorState.tabSize.of(2),
    indentService.of(indentation),
    EditorState.languageData.of(() => [{ indentOnInput: typed }]),
    indentOnInput(),
  ];
}
