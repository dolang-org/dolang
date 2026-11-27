import * as vscode from "vscode";
import { columns, dedent, dedentFor, typed, type Line } from "./indent";

/** The characters that end a keyword `dedents` moves */
export const TRIGGERS = ["e", "h", "y"] as const;

/**
 * Moves a `catch`, `finally` or `else` out to its header as its last letter
 * is typed
 */
export const dedentOnType: vscode.OnTypeFormattingEditProvider = {
    provideOnTypeFormattingEdits(document, position, _ch, options) {
        const line = document.lineAt(position.line);
        const rule = typed.test(line.text) ? dedentFor(line.text) : undefined;
        if (!rule) {
            return [];
        }
        const tabSize = options.tabSize;
        const lines = function* (): Generator<Line> {
            for (let at = position.line - 1; at >= 0; at--) {
                const text = document.lineAt(at).text;
                yield { text, indent: columns(text, tabSize) };
            }
        };
        const indent = columns(line.text, tabSize);
        const target = dedent(rule, lines(), indent, tabSize);
        if (target === indent) {
            return [];
        }
        const whitespace = options.insertSpaces
            ? " ".repeat(target)
            : "\t".repeat(Math.floor(target / tabSize)) + " ".repeat(target % tabSize);
        const leading = line.firstNonWhitespaceCharacterIndex;
        const range = new vscode.Range(position.line, 0, position.line, leading);
        return [vscode.TextEdit.replace(range, whitespace)];
    }
};
