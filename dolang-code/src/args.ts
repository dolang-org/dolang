/**
 * Splits a user-typed argument string into an argument vector.
 *
 * The rules are shell-like, but no shell ever sees the result: whitespace
 * separates arguments, `'…'` is literal, `"…"` honors `\"` and `\\`, and a
 * backslash outside quotes escapes the next character.
 */
export function splitArgs(text: string): string[] {
    const args: string[] = [];
    let current = "";
    let inArg = false;
    let i = 0;

    while (i < text.length) {
        const c = text[i];
        if (/\s/.test(c)) {
            if (inArg) {
                args.push(current);
                current = "";
                inArg = false;
            }
            i++;
        } else if (c === "'") {
            const end = text.indexOf("'", i + 1);
            if (end < 0) {
                throw new Error("Unterminated single quote in arguments");
            }
            current += text.slice(i + 1, end);
            inArg = true;
            i = end + 1;
        } else if (c === '"') {
            i++;
            let closed = false;
            while (i < text.length) {
                const d = text[i];
                if (d === '"') {
                    closed = true;
                    i++;
                    break;
                }
                if (
                    d === "\\" &&
                    i + 1 < text.length &&
                    (text[i + 1] === '"' || text[i + 1] === "\\")
                ) {
                    current += text[i + 1];
                    i += 2;
                } else {
                    current += d;
                    i++;
                }
            }
            if (!closed) {
                throw new Error("Unterminated double quote in arguments");
            }
            inArg = true;
        } else if (c === "\\" && i + 1 < text.length) {
            current += text[i + 1];
            inArg = true;
            i += 2;
        } else {
            current += c;
            inArg = true;
            i++;
        }
    }

    if (inArg) {
        args.push(current);
    }
    return args;
}

/**
 * Builds the text to type into the Do REPL for `text`, without the final
 * Enter, or `undefined` when there is nothing to send.
 *
 * The text is dedented to its common indentation and stripped of leading and
 * trailing blank lines. Several lines go in as one bracketed paste, so the REPL
 * reads them as a single turn: it discards input typed ahead of its next
 * prompt, which rules out sending them line by line. When `bracketedPaste` is
 * false (Windows consoles), several lines are instead sent through the REPL's
 * multi-line mode: an empty line starts it and a blank line, supplied by the
 * final Enter, ends it, so blank lines inside the block are dropped.
 */
export function replPayload(text: string, bracketedPaste: boolean): string | undefined {
    const lines = text
        .replace(/\r\n?/g, "\n")
        .split("\n")
        .map(line => line.trimEnd());
    while (lines.length > 0 && lines[0] === "") {
        lines.shift();
    }
    while (lines.length > 0 && lines[lines.length - 1] === "") {
        lines.pop();
    }
    if (lines.length === 0) {
        return undefined;
    }
    const indent = Math.min(
        ...lines.filter(line => line !== "").map(line => line.length - line.trimStart().length)
    );
    const dedented = lines.map(line => line.slice(indent));
    if (dedented.length === 1) {
        return dedented[0];
    }
    if (bracketedPaste) {
        return `\x1b[200~${dedented.join("\n")}\x1b[201~`;
    }
    return ["", ...dedented.filter(line => line !== ""), ""].join("\r");
}
