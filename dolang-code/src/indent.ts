// Where a typed `catch`, `finally` or `else` moves to. VS Code's
// `decreaseIndentPattern` can only move a line one level out from the line
// before it, so the extension and the playground both apply these rules as the
// keyword is typed.

/**
 * A line that moves out to the nearest earlier `header` indented less than
 * it, or else `fallback` levels from where it is
 */
export interface Dedent {
    readonly line: RegExp;
    readonly header: RegExp;
    readonly fallback: number;
}

export const dedents: readonly Dedent[] = [
    // `catch` and `finally` align with their `try`, or a `catch` before them
    {
        line: /^\s*(?:catch|finally)\b/,
        header: /^\s*(?:try|catch)\b|[^=!<>]=\s*try\b/,
        fallback: -1
    },
    // An `else` with no `if` before it belongs to a `let` or `bind`, so stays
    // put
    { line: /^\s*else\b/, header: /^\s*(?:if|else\s+if)\b|[^=!<>]=\s*if\b/, fallback: 0 }
];

/** A line that is a keyword just typed, which moves by its rule */
export const typed = /^\s*(?:catch|finally|else)$/;

/** The rule that moves `text`, if one does */
export function dedentFor(text: string): Dedent | undefined {
    return dedents.find(rule => rule.line.test(text));
}

/** A line of source, and its indentation in columns */
export interface Line {
    readonly text: string;
    readonly indent: number;
}

/**
 * The indentation of a line at `indent` that `rule` moves, given the lines
 * before it, nearest first, and the columns of a level
 */
export function dedent(rule: Dedent, before: Iterable<Line>, indent: number, unit: number): number {
    for (const line of before) {
        if (/\S/.test(line.text) && line.indent < indent && rule.header.test(line.text)) {
            return line.indent;
        }
    }
    return Math.max(0, indent + rule.fallback * unit);
}

/** The columns `text`'s leading whitespace takes, with tabs every `tabSize` */
export function columns(text: string, tabSize: number): number {
    let column = 0;
    for (const char of /^\s*/.exec(text)![0]) {
        column = char === "\t" ? column + tabSize - (column % tabSize) : column + 1;
    }
    return column;
}
