// Indentation rules for Do, read from the VS Code extension's language
// configuration so the two editors agree.
import { countColumn, EditorState, type Extension } from '@codemirror/state';
import { indentOnInput, indentService, indentUnit, type IndentContext } from '@codemirror/language';
import configuration from '../../dolang-code/language-configuration.json';

interface EnterRule { beforeText: RegExp; indent: 'indent' | 'outdent' | 'none' }

const enterRules: EnterRule[] = configuration.onEnterRules.map(rule => ({
  beforeText: new RegExp(rule.beforeText),
  indent: rule.action.indent as EnterRule['indent'],
}));
// A line that moves out to the nearest earlier `header` indented less, or else
// to `fallback` levels from where it is
interface Dedent { line: RegExp; header: RegExp; fallback: number }
const dedents: Dedent[] = [
  // `catch` and `finally`, by the extension's rule
  {
    line: new RegExp(configuration.indentationRules.decreaseIndentPattern),
    header: /^\s*(?:try|catch)\b|[^=!<>]=\s*try\b/,
    fallback: -1,
  },
  // `else`, which the extension doesn't dedent. One with no `if` before it
  // belongs to a `let` or `bind`, so stays put.
  { line: /^\s*else\b/, header: /^\s*(?:if|else\s+if)\b|[^=!<>]=\s*if\b/, fallback: 0 },
];

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
  const rule = dedents.find(rule => rule.line.test(line.text));
  return rule ? dedent(cx, rule, line.from, breaking ? indent : cx.lineIndent(line.from, 1)) : indent;
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

// The indentation of a line at `indent`, starting at `from`, that `rule`
// dedents.
function dedent(cx: IndentContext, rule: Dedent, from: number, indent: number): number {
  while (from > 0) {
    const line = cx.lineAt(from - 1, -1);
    const lineIndent = cx.lineIndent(line.from, -1);
    if (!blank(line.text) && lineIndent < indent && rule.header.test(line.text)) return lineIndent;
    from = line.from;
  }
  return Math.max(0, indent + rule.fallback * cx.unit);
}

/** Do's indentation: two spaces, by the extension's rules */
export function doIndentation(): Extension {
  return [
    indentUnit.of('  '),
    EditorState.tabSize.of(2),
    indentService.of(indentation),
    EditorState.languageData.of(() => [{ indentOnInput: /^\s*(?:catch|finally|else)$/ }]),
    indentOnInput(),
  ];
}
