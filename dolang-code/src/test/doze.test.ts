import * as assert from "assert";
import * as fs from "fs/promises";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import {
    buildDozeTask,
    dozeTaskPaths,
    DozeRunner,
    readReport,
    reportPath,
    spanRange,
    type DozeDependencies
} from "../doze";

suite("Doze Tasks", () => {
    test("check runs doze on the project with its report", () => {
        const task = buildDozeTask({
            dolang: "/bin/dolang",
            root: "/p",
            command: "check",
            args: ["--strict"],
            report: "/s/r.json",
            keepOpen: true
        });
        const execution = task.execution;
        assert.ok(execution instanceof vscode.ProcessExecution);
        assert.strictEqual(execution.process, "/bin/dolang");
        assert.deepStrictEqual(execution.args, [
            "-m",
            "doze",
            "--project",
            "/p",
            "check",
            "--strict",
            "--json",
            "/s/r.json"
        ]);
        assert.strictEqual(execution.options?.cwd, "/p");
        assert.strictEqual(task.group, vscode.TaskGroup.Test);
        assert.strictEqual(task.presentationOptions.close, false);
        assert.deepStrictEqual(task.definition, {
            type: "doze",
            command: "check",
            args: ["--strict"]
        });
        assert.deepStrictEqual(dozeTaskPaths(task), { root: "/p", report: "/s/r.json" });
    });

    test("bundle is a build task", () => {
        const task = buildDozeTask({
            dolang: "dolang",
            root: "/p",
            command: "bundle",
            report: "/r.json",
            keepOpen: false
        });
        const execution = task.execution;
        assert.ok(execution instanceof vscode.ProcessExecution);
        assert.deepStrictEqual(execution.args, [
            "-m",
            "doze",
            "--project",
            "/p",
            "bundle",
            "--json",
            "/r.json"
        ]);
        assert.strictEqual(task.group, vscode.TaskGroup.Build);
        assert.strictEqual(task.presentationOptions.close, true);
    });

    test("the report path is stable and per project", () => {
        const storage = vscode.Uri.file("/storage");
        assert.strictEqual(reportPath(storage, "/a"), reportPath(storage, "/a"));
        assert.notStrictEqual(reportPath(storage, "/a"), reportPath(storage, "/b"));
        assert.strictEqual(path.dirname(reportPath(storage, "/a")), path.join("/storage", "doze"));
    });
});

suite("Doze Spans", () => {
    const span = (line: number, column: number, len: number) => ({ start: { line, column }, len });
    const range = (r: vscode.Range) => [
        r.start.line,
        r.start.character,
        r.end.line,
        r.end.character
    ];

    test("columns count characters, ranges UTF-16 units", () => {
        assert.deepStrictEqual(range(spanRange("a😀bc\n", span(0, 2, 2))), [0, 3, 0, 5]);
    });

    test("a span runs onto later lines", () => {
        assert.deepStrictEqual(range(spanRange("ab\ncd\nef", span(0, 1, 4))), [0, 1, 1, 2]);
        assert.deepStrictEqual(range(spanRange("ab\r\ncd", span(0, 1, 4))), [0, 1, 1, 1]);
    });

    test("a span past the end of the text ends there", () => {
        assert.deepStrictEqual(range(spanRange("ab\ncd", span(1, 1, 10))), [1, 1, 1, 2]);
    });
});

suite("Doze Reports", () => {
    let root: string;

    setup(async () => {
        root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-doze-"));
        await fs.mkdir(path.join(root, "lib"));
        await fs.writeFile(path.join(root, "main.dol"), "import util\n\nutil.go 1\n");
        await fs.writeFile(path.join(root, "lib", "util.dol"), "pub def go x@Str\n  x\n");
    });

    teardown(async () => {
        await fs.rm(root, { recursive: true, force: true });
    });

    async function writeReport(diagnostics: unknown[]): Promise<string> {
        const file = path.join(root, "report.json");
        await fs.writeFile(
            file,
            JSON.stringify({
                sources: 2,
                checked: true,
                errors: 1,
                warnings: 1,
                passed: false,
                diagnostics
            })
        );
        return file;
    }

    const main = () => path.join(root, "main.dol");
    const util = () => path.join(root, "lib", "util.dol");

    const REPORT = () => [
        {
            path: main(),
            severity: "error",
            message: "expected Str",
            span: { start: { line: 2, column: 8 }, len: 1 },
            annotations: [
                {
                    kind: "context",
                    message: "parameter declared here",
                    path: util(),
                    span: { start: { line: 0, column: 11 }, len: 5 }
                },
                {
                    kind: "context",
                    message: "from the prelude",
                    path: "std.dol",
                    span: { start: { line: 0, column: 0 }, len: 1 }
                }
            ],
            notes: [{ kind: "help", message: "pass a string" }],
            patches: []
        },
        {
            path: util(),
            severity: "warning",
            message: "unused",
            span: { start: { line: 1, column: 2 }, len: 1 },
            annotations: [],
            notes: [],
            patches: []
        }
    ];

    test("diagnostics are grouped by source, with annotations as related information", async () => {
        const results = await readReport(await writeReport(REPORT()), root);
        assert.ok(results);
        const [error] = results.get(main()) ?? [];
        assert.strictEqual(error.severity, vscode.DiagnosticSeverity.Error);
        assert.strictEqual(error.message, "expected Str\nhelp: pass a string");
        assert.strictEqual(error.source, "doze");
        assert.deepStrictEqual(
            [error.range.start.line, error.range.start.character, error.range.end.character],
            [2, 8, 9]
        );
        // The bundled typelib's annotation has no file to point into
        assert.strictEqual(error.relatedInformation?.length, 1);
        const [related] = error.relatedInformation;
        assert.strictEqual(related.location.uri.fsPath, util());
        assert.strictEqual(related.location.range.start.character, 11);
        assert.strictEqual(related.message, "parameter declared here");

        const [warning] = results.get(util()) ?? [];
        assert.strictEqual(warning.severity, vscode.DiagnosticSeverity.Warning);
    });

    test("a missing report reads as none", async () => {
        assert.strictEqual(await readReport(path.join(root, "missing.json"), root), undefined);
    });

    suite("Runner", () => {
        let storage: string;
        let errors: string[];
        let executed: vscode.Task[];
        let runner: DozeRunner;

        function createRunner(overrides: Partial<DozeDependencies> = {}): DozeRunner {
            return new DozeRunner({
                projectRoot: async () => root,
                dolang: async () => "/bin/dolang",
                storage: vscode.Uri.file(storage),
                log: { info: () => undefined, warn: () => undefined, error: () => undefined },
                trusted: () => true,
                execute: async task => {
                    executed.push(task);
                },
                showError: message => errors.push(message),
                ...overrides
            });
        }

        setup(async () => {
            storage = path.join(root, "storage");
            errors = [];
            executed = [];
            runner = createRunner();
        });

        teardown(() => runner.dispose());

        test("a command runs doze in the server's project root", async () => {
            const report = reportPath(vscode.Uri.file(storage), root);
            await fs.mkdir(path.dirname(report), { recursive: true });
            await fs.writeFile(report, "stale");
            await runner.run("check", vscode.Uri.file(main()));
            assert.deepStrictEqual(errors, []);
            assert.strictEqual(executed.length, 1);
            assert.deepStrictEqual(dozeTaskPaths(executed[0]), { root, report });
            const execution = executed[0].execution as vscode.ProcessExecution;
            assert.strictEqual(execution.options?.cwd, root);
            await assert.rejects(fs.access(report), "the stale report is deleted");
        });

        test("an untrusted workspace refuses", async () => {
            runner.dispose();
            runner = createRunner({ trusted: () => false });
            await runner.run("bundle", vscode.Uri.file(main()));
            assert.deepStrictEqual(executed, []);
        });

        test("no language server is an error", async () => {
            runner.dispose();
            runner = createRunner({ projectRoot: async () => undefined });
            await runner.run("bundle", vscode.Uri.file(main()));
            assert.deepStrictEqual(executed, []);
            assert.strictEqual(errors.length, 1);
            assert.match(errors[0], /language server/);
        });

        test("a finished task's report fills the Problems panel", async () => {
            await runner.run("check", vscode.Uri.file(main()));
            const [task] = executed;
            const paths = dozeTaskPaths(task);
            assert.ok(paths);
            await fs.writeFile(paths.report, JSON.stringify({ diagnostics: REPORT() }));
            await runner.taskEnded(task);
            assert.strictEqual(runner.diagnostics.get(vscode.Uri.file(main()))?.length, 1);
            assert.strictEqual(runner.diagnostics.get(vscode.Uri.file(util()))?.length, 1);
            await assert.rejects(fs.access(paths.report), "the report is consumed");

            // No report this time: the project's entries are cleared
            await runner.taskEnded(task);
            assert.strictEqual(runner.diagnostics.get(vscode.Uri.file(main()))?.length ?? 0, 0);
            assert.strictEqual(runner.diagnostics.get(vscode.Uri.file(util()))?.length ?? 0, 0);
        });

        test("an open document is left to the language server", async () => {
            await runner.run("check", vscode.Uri.file(main()));
            const [task] = executed;
            const paths = dozeTaskPaths(task);
            assert.ok(paths);
            await fs.writeFile(paths.report, JSON.stringify({ diagnostics: REPORT() }));
            await runner.taskEnded(task);
            assert.strictEqual(runner.diagnostics.get(vscode.Uri.file(util()))?.length, 1);

            // Opening drops the entries reported earlier
            await vscode.workspace.openTextDocument(vscode.Uri.file(util()));
            assert.strictEqual(runner.diagnostics.get(vscode.Uri.file(util()))?.length ?? 0, 0);

            // And a later report skips it
            await fs.writeFile(paths.report, JSON.stringify({ diagnostics: REPORT() }));
            await runner.taskEnded(task);
            assert.strictEqual(runner.diagnostics.get(vscode.Uri.file(util()))?.length ?? 0, 0);
            assert.strictEqual(runner.diagnostics.get(vscode.Uri.file(main()))?.length, 1);
        });
    });
});
