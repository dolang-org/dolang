import * as assert from "assert";
import { columns, dedent, dedentFor, typed, type Line } from "../indent";

// The text before the keyword's line, and the line once it's typed at the
// indentation Enter gave it, as the playground's tests type them
const cases: [before: string, typed: string, after: number][] = [
    ["try\n  if x\n    risky()", "    catch", 0],
    ["let r = try\n  risky()", "  finally", 0],
    ["try\n  a\ncatch e\n  b", "  finally", 0],
    ["try\n  try\n    a\n  catch e\n    b", "    finally", 2],
    ["if a\n  if b\n    x", "    else", 2],
    ["let v = if a\n  x", "  else", 0],
    ["if a\n  x\nelse if b\n  y", "  else", 0],
    // Blank lines are passed over
    ["if a\n  x\n\n", "  else", 0],
    // One with no `if` belongs to a `let`, so stays put
    ["def f\n  let k v = pair", "  else", 2],
    // A `catch` with no `try` moves out a level
    ["def f\n  x", "  catch", 0]
];

function linesBefore(text: string): Line[] {
    return text
        .split("\n")
        .reverse()
        .map(line => ({ text: line, indent: columns(line, 2) }));
}

suite("Dedent", () => {
    for (const [before, line, after] of cases) {
        test(`${JSON.stringify(line)} after ${JSON.stringify(before)}`, () => {
            assert.ok(typed.test(line));
            const rule = dedentFor(line);
            assert.ok(rule);
            assert.strictEqual(dedent(rule, linesBefore(before), columns(line, 2), 2), after);
        });
    }

    test("only a keyword typed alone moves", () => {
        for (const line of ["  else if", "  catch e", "  elsewhere", "  finallyx"]) {
            assert.ok(!typed.test(line), line);
        }
    });

    test("tabs count to the next tab stop", () => {
        assert.strictEqual(columns("\t x", 4), 5);
        assert.strictEqual(columns("  \tx", 4), 4);
        assert.strictEqual(columns("x", 4), 0);
    });
});
