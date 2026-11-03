// Runs the integration tests, in `src/integration/`, against real `dolang`
// and `dolang-lsp` executables named by `DOLANG_CODE_DOLANG` and
// `DOLANG_CODE_LSP`. `dodo vscode-test` builds them and sets both.
import { defineConfig } from "@vscode/test-cli";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";

function tool(variable) {
    const value = process.env[variable];
    if (!value) {
        throw new Error(`Set ${variable} to the absolute path of the executable to test`);
    }
    if (!path.isAbsolute(value)) {
        throw new Error(`${variable} must be an absolute path: ${value}`);
    }
    return value;
}

const dolang = tool("DOLANG_CODE_DOLANG");
const lsp = tool("DOLANG_CODE_LSP");

// A fresh copy of the fixture project, so runs don't see each other's output,
// and a fresh profile, so no state carries over between runs
const root = fs.mkdtempSync(path.join(os.tmpdir(), "dolang-code-integration-"));
process.on("exit", () => fs.rmSync(root, { recursive: true, force: true }));
const workspace = path.join(root, "workspace");
fs.cpSync(path.join(import.meta.dirname, "test", "fixture"), workspace, { recursive: true });
// The checked-in fixture has no `doze.toml`, so that the language server
// doesn't take it for a project of its own while editing this repository
fs.writeFileSync(path.join(workspace, "doze.toml"), '[project]\nname = "fixture"\n');
fs.mkdirSync(path.join(workspace, ".vscode"));
fs.writeFileSync(
    path.join(workspace, ".vscode", "settings.json"),
    JSON.stringify(
        {
            "dolang.path": dolang,
            "dolang.lsp.path": lsp,
            "dolang.keepTerminalOpen": false
        },
        null,
        4
    )
);

export default defineConfig({
    label: "integration",
    files: "out/integration/**/*.test.js",
    workspaceFolder: workspace,
    launchArgs: ["--disable-gpu", `--user-data-dir=${path.join(root, "user-data")}`],
    env: { DOLANG_CODE_FIXTURE: workspace },
    mocha: { timeout: 60000 }
});
