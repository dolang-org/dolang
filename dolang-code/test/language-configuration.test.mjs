import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const config = JSON.parse(
    await readFile(new URL("../language-configuration.json", import.meta.url), "utf8")
);

// VS Code compiles these patterns as JavaScript regular expressions.
const rules = config.onEnterRules.map(rule => ({
    beforeText: new RegExp(rule.beforeText),
    indent: rule.action.indent
}));
const increase = new RegExp(config.indentationRules.increaseIndentPattern);
const decrease = new RegExp(config.indentationRules.decreaseIndentPattern);

// Like VS Code, the first matching rule decides.
function onEnter(line) {
    return rules.find(rule => rule.beforeText.test(line))?.indent ?? "keep";
}

const onEnterCases = [
    // Keyword block openers
    ["if flag", "indent"],
    ['  if func "arg"', "indent"],
    ["else", "indent"],
    ["else  # fallback", "indent"],
    ["else if (x > 1)", "indent"],
    ["while (count < 5)", "indent"],
    ["for k v = items", "indent"],
    ["try", "indent"],
    ["catch error.Type: err", "indent"],
    ["finally", "indent"],
    ["let label = match value", "indent"],
    ["match value", "indent"],
    ["bind args", "indent"],
    ["def greet name", "indent"],
    ["def f x # note", "indent"],
    ["pub def build", "indent"],
    ["class Point", "indent"],
    ["pub @class Shape", "indent"],
    ["if diagnostics.any do |diag| (diag.severity == :ERROR:)", "indent"],
    // Block-valued right-hand sides
    ["let x = if (a > b)", "indent"],
    ["result = try", "indent"],
    // `do` at the end of a line
    ["let f = do |x|", "indent"],
    ["s.each do", "indent"],
    ["do", "indent"],
    ["let f = do |x @ (Int | Str)| -> Int", "indent"],
    ["do -> Array[Str]", "indent"],
    // Here strings
    ["let doc = |", "indent"],
    ["let doc = |-", "indent"],
    ["let raw = r|", "indent"],
    // Vertical data and key items
    ["let items = $", "indent"],
    ["return $", "indent"],
    ["  scores:", "indent"],
    ['  "x-custom":', "indent"],
    ["class Derived[T]:", "indent"],
    ["import math:", "indent"],
    // Block terminators
    ["  return result", "outdent"],
    ["break", "outdent"],
    ["continue", "outdent"],
    ['throw std.ValueError "bad"', "outdent"],
    // Lines that open nothing
    ["@def double x@Int -> Int", "keep"],
    ['else throw std.ValueError "bad"', "keep"],
    ["else return nil", "keep"],
    ["echo :foo:", "keep"],
    ["let t = :ERROR:", "keep"],
    ["let f = do echo hi", "keep"],
    ['0 | 1 do "small"', "keep"],
    ["echo hello", "keep"],
    ["let total = (a + b)", "keep"],
    ["if_ready = (a == b)", "keep"],
    ["default_value", "keep"],
    ["echo $price", "keep"],
    ["# see foo:", "none"],
    ["  # if flag", "none"]
];

for (const [line, expected] of onEnterCases) {
    test(`onEnter: ${JSON.stringify(line)} -> ${expected}`, () => {
        assert.equal(onEnter(line), expected);
    });
}

test("increaseIndentPattern never matches (onEnterRules decide)", () => {
    for (const [line] of onEnterCases) {
        assert.ok(!increase.test(line), line);
    }
});

for (const [line, expected] of [
    ["catch err", true],
    ["  finally", true],
    ["else", false],
    ["catcher", false]
]) {
    test(`decreaseIndent: ${JSON.stringify(line)} -> ${expected}`, () => {
        assert.equal(decrease.test(line), expected);
    });
}

// VS Code's default word pattern, used because the configuration sets none.
const DEFAULT_WORD_REGEXP = /(-?\d*\.\d\w*)|([^`~!@#$%^&*()\-=+[{\]}\\|;:'",.<>/?\s]+)/g;

for (const [text, words] of [
    ["$name", ["name"]],
    [":sym:", ["sym"]],
    ["self.#count", ["self", "count"]],
    ["$person.first_name", ["person", "first_name"]]
]) {
    test(`default word pattern: ${text}`, () => {
        assert.deepEqual(text.match(DEFAULT_WORD_REGEXP), words);
    });
}
