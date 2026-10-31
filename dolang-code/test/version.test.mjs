import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

// The extension downloads the tool release tagged with its own version, so it
// must match the tools built from the same tree.
test("extension version matches the dolang-lsp crate version", async () => {
    const manifest = JSON.parse(
        await readFile(new URL("../package.json", import.meta.url), "utf8")
    );
    const cargo = await readFile(new URL("../../dolang-lsp/Cargo.toml", import.meta.url), "utf8");
    const crate = cargo.match(/^\[package\][^[]*?^version\s*=\s*"([^"]+)"/m);
    assert.ok(crate, "dolang-lsp/Cargo.toml has no package version");
    assert.equal(manifest.version, crate[1]);
});
