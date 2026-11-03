// Tests the extension against real `dolang` and `dolang-lsp` executables, in
// the fixture project `.vscode-test.integration.mjs` opens as the workspace.
import * as assert from "assert";
import * as fs from "fs/promises";
import * as path from "path";
import * as vscode from "vscode";
import type { DoExtensionExports } from "../extension";
import type { DoTestRun } from "../testing";

const FIXTURE = process.env.DOLANG_CODE_FIXTURE!;

function fixture(...parts: string[]): vscode.Uri {
    return vscode.Uri.file(path.join(FIXTURE, ...parts));
}

/** Polls `check` until it returns a value other than `undefined`. */
async function waitFor<T>(
    what: string,
    check: () => T | undefined | Promise<T | undefined>,
    timeout = 30000
): Promise<T> {
    const deadline = Date.now() + timeout;
    for (;;) {
        const value = await check();
        if (value !== undefined) {
            return value;
        }
        if (Date.now() > deadline) {
            throw new Error(`Timed out waiting for ${what}`);
        }
        await new Promise(resolve => setTimeout(resolve, 50));
    }
}

/** Waits for `promise`, failing after `timeout` milliseconds. */
async function within<T>(what: string, promise: Promise<T>, timeout = 30000): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const expired = new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error(`Timed out waiting for ${what}`)), timeout);
    });
    try {
        return await Promise.race([promise, expired]);
    } finally {
        clearTimeout(timer);
    }
}

function child(parent: vscode.TestItem, label: string): vscode.TestItem {
    const found = [...parent.children].map(([, item]) => item).find(i => i.label === label);
    assert.ok(found, `${parent.label} holds ${label}`);
    return found;
}

async function activate(): Promise<DoExtensionExports> {
    const extension = vscode.extensions.all.find(
        candidate => candidate.packageJSON.name === "dolang"
    );
    assert.ok(extension, "the extension is installed");
    return (await extension.activate()) as DoExtensionExports;
}

/** A test run that records what happens to each item, by test ID. */
function createRun(): DoTestRun & {
    log: string[];
    messages: vscode.TestMessage[];
    ended: boolean;
} {
    const log: string[] = [];
    const messages: vscode.TestMessage[] = [];
    const name = (item: vscode.TestItem) => item.id.slice(item.id.indexOf("::") + 2);
    const record = (what: string) => (item: vscode.TestItem, message?: unknown) => {
        log.push(`${what} ${name(item)}`);
        if (message instanceof vscode.TestMessage) {
            messages.push(message);
        }
    };
    const run = {
        log,
        messages,
        ended: false,
        enqueued: record("enqueued"),
        started: record("started"),
        passed: record("passed"),
        failed: record("failed"),
        skipped: record("skipped"),
        errored: record("errored"),
        appendOutput: () => undefined,
        end: () => {
            run.ended = true;
        }
    };
    return run;
}

/** The outcomes `run` recorded, without the enqueued and started events. */
function outcomes(run: { log: string[] }): string[] {
    return run.log.filter(line => !/^(enqueued|started) /.test(line)).sort();
}

suite("Integration", () => {
    let api: DoExtensionExports;
    let root: string;

    suiteSetup(async () => {
        api = await activate();
        root = await fs.realpath(FIXTURE);
    });

    test("the language server finds the project and its module path", async () => {
        const main = fixture("main.dol");
        const projectRoot = await api.languageServer.projectRoot(main);
        assert.ok(projectRoot, "the server answers");
        assert.strictEqual(await fs.realpath(projectRoot), root);

        const args = await api.languageServer.modulePathArgs(main);
        const lib = args.findIndex(
            (arg, i) => i > 0 && args[i - 1] === "--module-path" && arg.endsWith("lib")
        );
        assert.ok(lib > 0, `the module path holds lib/: ${JSON.stringify(args)}`);
        assert.strictEqual(await fs.realpath(args[lib]), path.join(root, "lib"));
    });

    test("the language server reports diagnostics", async () => {
        const uri = fixture("broken.dol");
        await fs.writeFile(uri.fsPath, "# Uses a name that isn't bound.\n\necho $missing\n");
        await vscode.window.showTextDocument(uri);
        const diagnostics = await waitFor("diagnostics", () => {
            const found = vscode.languages.getDiagnostics(uri);
            return found.length > 0 ? found : undefined;
        });
        const unbound = diagnostics.find(d => /unbound/.test(d.message));
        assert.ok(unbound, `an unbound name is reported: ${JSON.stringify(diagnostics)}`);
        assert.strictEqual(unbound.severity, vscode.DiagnosticSeverity.Error);
        assert.strictEqual(unbound.range.start.line, 2);
        await vscode.commands.executeCommand("workbench.action.closeActiveEditor");
        await fs.rm(uri.fsPath);
    });

    test("a script runs with the project's module path", async () => {
        const output = fixture("greeting.txt");
        await fs.rm(output.fsPath, { force: true });
        const ended = new Promise<number | undefined>(resolve => {
            const listener = vscode.tasks.onDidEndTaskProcess(event => {
                if (event.execution.task.definition.type === "dolang") {
                    listener.dispose();
                    resolve(event.exitCode);
                }
            });
        });
        await vscode.commands.executeCommand("dolang.runScriptFile", fixture("main.dol"));
        assert.strictEqual(await within("the script to finish", ended), 0);
        assert.strictEqual(await fs.readFile(output.fsPath, "utf8"), "Hello, world!");
    });

    suite("Projects", () => {
        /** Runs `command` on the fixture project, returning the root it reports on. */
        async function runOnProject(command: string): Promise<string> {
            const read = new Promise<string>(resolve => {
                const listener = api.doze.onDidReadReport(reported => {
                    listener.dispose();
                    resolve(reported);
                });
            });
            await vscode.commands.executeCommand(command, fixture("main.dol"));
            return within("the report to be read", read);
        }

        test("a check's report fills the Problems panel", async () => {
            const uri = fixture("lib", "broken.dol");
            await fs.writeFile(uri.fsPath, "# Uses a name that isn't bound.\n\necho $missing\n");
            try {
                const reported = await runOnProject("dolang.checkProject");
                assert.strictEqual(await fs.realpath(reported), root);
                const diagnostics = api.doze.diagnostics.get(uri) ?? [];
                const unbound = diagnostics.find(d => /unbound/.test(d.message));
                assert.ok(unbound, `an unbound name is reported: ${JSON.stringify(diagnostics)}`);
                assert.strictEqual(unbound.severity, vscode.DiagnosticSeverity.Error);
                assert.strictEqual(unbound.source, "doze");
                assert.strictEqual(unbound.range.start.line, 2);
            } finally {
                await fs.rm(uri.fsPath);
            }
        });

        test("a build bundles the project and clears its entries", async () => {
            const bundle = fixture("build", "fixture.dolz");
            await fs.rm(bundle.fsPath, { force: true });
            await runOnProject("dolang.buildProject");
            await fs.access(bundle.fsPath);
            let entries = 0;
            api.doze.diagnostics.forEach(() => entries++);
            assert.strictEqual(entries, 0);
        });
    });

    suite("Test Explorer", () => {
        let file: vscode.TestItem;

        suiteSetup(async () => {
            await api.tests.discover();
            const items = [...api.tests.controller.items].map(([, item]) => item);
            const found = items.find(item => item.label === "test/greet.dol");
            assert.ok(found, `discovered test/greet.dol: ${items.map(i => i.label)}`);
            file = found;
        });

        test("discovers the tests with their locations", () => {
            const tests = [...file.children].map(([, item]) => item);
            assert.deepStrictEqual(
                tests.map(item => [item.label, item.range?.start.line]),
                [
                    ["greets", 8],
                    ["fails", 12]
                ]
            );
            assert.strictEqual(file.error, undefined);
        });

        test("runs all tests", async () => {
            const run = createRun();
            await api.tests.run(
                new vscode.TestRunRequest(),
                new vscode.CancellationTokenSource().token,
                run
            );
            assert.ok(run.ended);
            assert.deepStrictEqual(outcomes(run), [
                "failed test/greet/fails",
                "passed test/greet/greets"
            ]);
        });

        test("runs a file", async () => {
            const run = createRun();
            await api.tests.run(
                new vscode.TestRunRequest([file]),
                new vscode.CancellationTokenSource().token,
                run
            );
            assert.deepStrictEqual(outcomes(run), [
                "failed test/greet/fails",
                "passed test/greet/greets"
            ]);
        });

        test("runs a single test", async () => {
            const run = createRun();
            const item = child(file, "greets");
            await api.tests.run(
                new vscode.TestRunRequest([item]),
                new vscode.CancellationTokenSource().token,
                run
            );
            assert.deepStrictEqual(outcomes(run), ["passed test/greet/greets"]);
        });

        test("reports a failing assertion's message and location", async () => {
            const run = createRun();
            const item = child(file, "fails");
            await api.tests.run(
                new vscode.TestRunRequest([item]),
                new vscode.CancellationTokenSource().token,
                run
            );
            assert.deepStrictEqual(outcomes(run), ["failed test/greet/fails"]);
            const [message] = run.messages;
            assert.match(String(message.message), /assertion failed/);
            assert.strictEqual(message.expectedOutput, '"Goodbye, Ada!"');
            assert.strictEqual(message.actualOutput, '"Hello, Ada!"');
            assert.strictEqual(message.location?.uri.fsPath, item.uri?.fsPath);
            assert.strictEqual(message.location?.range.start.line, 13);
        });
    });
});
