import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import test from "node:test";
import textmate from "vscode-textmate";
import oniguruma from "vscode-oniguruma";

const require = createRequire(import.meta.url);
const fixturesPath = new URL("../../test/syntax/fixtures.json", import.meta.url);
const fixtures = JSON.parse(await readFile(fixturesPath, "utf8"));
await oniguruma.loadWASM(await readFile(require.resolve("vscode-oniguruma/release/onig.wasm")));
const registry = new textmate.Registry({
    onigLib: Promise.resolve({
        createOnigScanner: patterns => new oniguruma.OnigScanner(patterns),
        createOnigString: value => new oniguruma.OnigString(value)
    }),
    loadGrammar: async () =>
        textmate.parseRawGrammar(
            await readFile(new URL("../syntaxes/dolang.tmLanguage.json", import.meta.url), "utf8"),
            "dolang.tmLanguage.json"
        )
});
const grammar = await registry.loadGrammar("source.dol");

for (const fixture of fixtures) {
    test(`TextMate: ${fixture.name}`, () => {
        let state = textmate.INITIAL;
        const tokens = fixture.lines.map(line => {
            const result = grammar.tokenizeLine(line, state);
            state = result.ruleStack;
            return result.tokens;
        });
        for (const check of fixture.checks) {
            const line = fixture.lines[check.line - 1];
            const start = line.indexOf(check.text, check.from ?? 0);
            assert.ok(start >= 0, `missing fixture text: ${check.text}`);
            // Check every character, catching partial numeric and escape matches.
            for (let column = start; column < start + check.text.length; column++) {
                const scopes =
                    tokens[check.line - 1].find(
                        token => token.startIndex <= column && token.endIndex > column
                    )?.scopes ?? [];
                for (const scope of check.scopes ?? []) {
                    assert.ok(
                        scopes.includes(scope),
                        `${check.line}:${column + 1} ${check.text}: expected ${scope}, got ${scopes}`
                    );
                }
                for (const scope of check.notScopes ?? []) {
                    assert.ok(
                        !scopes.some(value => value.startsWith(scope)),
                        `${check.line}:${column + 1} ${check.text}: unexpected ${scope}, got ${scopes}`
                    );
                }
            }
        }
    });
}
