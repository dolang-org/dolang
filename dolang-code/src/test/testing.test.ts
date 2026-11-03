import * as assert from "assert";
import * as fs from "fs/promises";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import {
    charToUtf16Column,
    DoTestController,
    type DoTestRun,
    type DozeResult,
    type DozeSpawner
} from "../testing";

// Columns are in characters: `adds` starts at character 4 of line 2, and the
// failing call at character 4 of line 3, after an astral character.
const SOURCE = [
    "# Tests",
    "#[test]",
    "def adds()",
    "  😀assert_eq 1 2",
    "#[test]",
    "def connects backend"
].join("\n");

function location(line: number, column: number, len: number) {
    return { file: "test/foo.dol", line, column, len };
}

function testRecord(name: string, base: string, params: string[][], line: number) {
    return {
        id: `test/foo/${name}`,
        module: "test/foo",
        name,
        base,
        params,
        tags: name === "adds" ? ["fast"] : [],
        location: location(line, 4, base.length)
    };
}

const TESTS = [
    testRecord("adds", "adds", [], 2),
    testRecord("connects/backend=sqlite", "connects", [["backend", "sqlite"]], 5),
    testRecord("connects/backend=pg", "connects", [["backend", "pg"]], 5)
];

const LISTING = {
    version: 1,
    modules: [
        { path: "test/foo.dol", module: "test/foo", status: "loaded", tests: TESTS },
        {
            path: "test/bar.dol",
            module: "test/bar",
            status: "failed",
            error: {
                kind: "compile",
                message: "failed to compile test/bar.dol",
                diagnostics: [
                    {
                        severity: "error",
                        message: "unbound identifier",
                        location: { file: "test/bar.dol", line: 0, column: 2, len: 1 }
                    }
                ]
            }
        }
    ]
};

interface Spawned {
    readonly args: readonly string[];
    readonly cwd: string;
}

/** A project directory holding `test/foo.dol`. */
async function createProject(): Promise<string> {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));
    await fs.mkdir(path.join(root, "test"));
    await fs.writeFile(path.join(root, "test", "foo.dol"), SOURCE);
    return root;
}

/**
 * A spawner answering `--list` with `listing`, and a run with `events`,
 * recording each call.
 */
function createSpawner(
    calls: Spawned[],
    events: readonly object[] = [],
    listing: object | null = LISTING,
    result: DozeResult = { code: 0, stderr: "" }
): DozeSpawner {
    return (_dolang, args, cwd) => {
        calls.push({ args, cwd });
        const lines = args.includes("--list")
            ? listing === null
                ? []
                : [JSON.stringify(listing)]
            : events.map(event => JSON.stringify(event));
        return {
            lines: (async function* () {
                yield* lines;
            })(),
            done: Promise.resolve(
                args.includes("--list") && listing === null
                    ? { code: 1, stderr: "no test directory in /p\n" }
                    : result
            )
        };
    };
}

function createController(spawn: DozeSpawner): DoTestController {
    return new DoTestController({
        projectRoot: async () => undefined,
        dolang: async () => "/bin/dolang",
        log: { info: () => undefined, warn: () => undefined, error: () => undefined },
        spawn
    });
}

/** A test run that records what happens to each item, by label path. */
function createRun(): DoTestRun & { log: string[]; messages: vscode.TestMessage[] } {
    const log: string[] = [];
    const messages: vscode.TestMessage[] = [];
    const name = (item: vscode.TestItem) => item.id.slice(item.id.indexOf("::") + 2);
    const record = (what: string) => (item: vscode.TestItem, message?: unknown) => {
        log.push(`${what} ${name(item)}`);
        if (message instanceof vscode.TestMessage) {
            messages.push(message);
        }
    };
    return {
        log,
        messages,
        enqueued: record("enqueued"),
        started: record("started"),
        passed: record("passed"),
        failed: record("failed"),
        skipped: record("skipped"),
        errored: record("errored"),
        appendOutput: () => undefined,
        end: () => log.push("end")
    };
}

function children(collection: vscode.TestItemCollection): vscode.TestItem[] {
    const items: vscode.TestItem[] = [];
    collection.forEach(item => items.push(item));
    return items;
}

function find(collection: vscode.TestItemCollection, label: string): vscode.TestItem {
    const item = children(collection).find(i => i.label === label);
    assert.ok(item, `no item ${label}`);
    return item;
}

suite("Test Explorer", () => {
    test("character columns convert to UTF-16", () => {
        assert.strictEqual(charToUtf16Column("abc", 2), 2);
        assert.strictEqual(charToUtf16Column("😀a", 1), 2);
        assert.strictEqual(charToUtf16Column("😀a", 2), 3);
        assert.strictEqual(charToUtf16Column("ab", 5), 2);
    });

    test("the listing becomes files, tests and parameter groups", async () => {
        const root = await createProject();
        const calls: Spawned[] = [];
        const tests = createController(createSpawner(calls));
        try {
            await tests.load([root]);
            assert.deepStrictEqual(calls, [
                {
                    args: ["-m", "doze", "--project", root, "test", "--list", "--format", "json"],
                    cwd: root
                }
            ]);
            const items = tests.controller.items;
            assert.deepStrictEqual(
                children(items).map(i => i.label),
                ["test/foo.dol", "test/bar.dol"]
            );
            const foo = find(items, "test/foo.dol");
            assert.strictEqual(foo.uri?.fsPath, path.join(root, "test", "foo.dol"));
            const adds = find(foo.children, "adds");
            assert.strictEqual(adds.uri?.fsPath, path.join(root, "test", "foo.dol"));
            assert.deepStrictEqual(adds.range, new vscode.Range(2, 4, 2, 8));
            assert.deepStrictEqual(
                adds.tags.map(t => t.id),
                ["fast"]
            );
            const connects = find(foo.children, "connects");
            assert.deepStrictEqual(
                children(connects.children).map(i => i.label),
                ["backend=sqlite", "backend=pg"]
            );
            const bar = find(items, "test/bar.dol");
            assert.strictEqual(
                bar.error,
                "failed to compile test/bar.dol\ntest/bar.dol:1:3: error: unbound identifier"
            );
        } finally {
            tests.dispose();
        }
    });

    test("a failed listing shows its error", async () => {
        const root = await createProject();
        const tests = createController(createSpawner([], [], null));
        try {
            await tests.load([root]);
            const items = children(tests.controller.items);
            assert.strictEqual(items.length, 1);
            assert.strictEqual(items[0].error, "no test directory in /p");
        } finally {
            tests.dispose();
        }
    });

    test("several projects each get an item", async () => {
        const first = await createProject();
        const second = await createProject();
        const tests = createController(createSpawner([]));
        try {
            await tests.load([first, second]);
            const items = children(tests.controller.items);
            assert.deepStrictEqual(items.map(i => i.id).sort(), [first, second].sort());
            assert.strictEqual(children(items[0].children).length, 2);
        } finally {
            tests.dispose();
        }
    });

    test("runs select tests by ID", async () => {
        const root = await createProject();
        const calls: Spawned[] = [];
        const tests = createController(createSpawner(calls));
        const token = new vscode.CancellationTokenSource().token;
        const idArgs = () => {
            const args = calls[calls.length - 1].args;
            return args.slice(args.indexOf("jsonl") + 1);
        };
        try {
            await tests.load([root]);
            const foo = find(tests.controller.items, "test/foo.dol");
            const connects = find(foo.children, "connects");

            await tests.run(new vscode.TestRunRequest(), token, createRun());
            assert.deepStrictEqual(calls[1], {
                args: ["-m", "doze", "--project", root, "test", "--format", "jsonl"],
                cwd: root
            });

            await tests.run(new vscode.TestRunRequest([foo]), token, createRun());
            assert.deepStrictEqual(idArgs(), [
                "--id",
                "test/foo/adds",
                "--id",
                "test/foo/connects/backend=sqlite",
                "--id",
                "test/foo/connects/backend=pg"
            ]);

            const excluded = find(connects.children, "backend=pg");
            await tests.run(new vscode.TestRunRequest([connects], [excluded]), token, createRun());
            assert.deepStrictEqual(idArgs(), ["--id", "test/foo/connects/backend=sqlite"]);

            await tests.run(new vscode.TestRunRequest(undefined, [connects]), token, createRun());
            assert.deepStrictEqual(idArgs(), ["--id", "test/foo/adds"]);
        } finally {
            tests.dispose();
        }
    });

    test("events set each test's state", async () => {
        const root = await createProject();
        const events = [
            { event: "run-start", version: 1, modules: 2 },
            { event: "module-start", path: "test/foo.dol", module: "test/foo" },
            { ...LISTING.modules[0], event: "module-end" },
            { ...TESTS[0], event: "test-start" },
            {
                event: "test-end",
                id: "test/foo/adds",
                outcome: "failed",
                duration: 0.002,
                message: "assertion failed: 1 == 2",
                failure_location: location(3, 3, 9),
                error: { type: "test.Assert", rendered: "boom", expected: 2, actual: 1 }
            },
            {
                event: "test-end",
                id: "test/foo/connects/backend=sqlite",
                outcome: "passed",
                duration: 0.001
            },
            {
                event: "test-end",
                id: "test/foo/connects/backend=pg",
                outcome: "ignored",
                duration: 0,
                message: null
            },
            { ...LISTING.modules[1], event: "module-end" },
            { event: "run-end", passed: 1, failed: 1, skipped: 0, ignored: 1, status: 1 }
        ];
        const tests = createController(createSpawner([], events));
        const run = createRun();
        try {
            await tests.load([root]);
            await tests.run(
                new vscode.TestRunRequest(),
                new vscode.CancellationTokenSource().token,
                run
            );
            assert.deepStrictEqual(run.log, [
                "enqueued test/foo/adds",
                "enqueued test/foo/connects/backend=sqlite",
                "enqueued test/foo/connects/backend=pg",
                "started test/foo/adds",
                "failed test/foo/adds",
                "passed test/foo/connects/backend=sqlite",
                "skipped test/foo/connects/backend=pg",
                "errored file:test/bar.dol",
                "end"
            ]);
            const [failure, moduleError] = run.messages;
            assert.strictEqual(failure.message, "assertion failed: 1 == 2");
            assert.strictEqual(failure.expectedOutput, "2");
            assert.strictEqual(failure.actualOutput, "1");
            assert.strictEqual(failure.location?.uri.fsPath, path.join(root, "test", "foo.dol"));
            // Character 3 follows the astral character, two UTF-16 units wide
            assert.deepStrictEqual(failure.location?.range, new vscode.Range(3, 4, 3, 13));
            assert.match(String(moduleError.message), /^failed to compile test\/bar\.dol/);
        } finally {
            tests.dispose();
        }
    });

    test("tests that never finish are errored", async () => {
        const root = await createProject();
        const events = [
            { event: "run-start", version: 1, modules: 1 },
            { event: "test-end", id: "test/foo/adds", outcome: "passed", duration: 0 }
        ];
        const result = { code: 1, stderr: "doze: crashed\n" };
        const tests = createController(createSpawner([], events, LISTING, result));
        const run = createRun();
        try {
            await tests.load([root]);
            const adds = find(find(tests.controller.items, "test/foo.dol").children, "adds");
            const connects = find(
                find(tests.controller.items, "test/foo.dol").children,
                "connects"
            );
            await tests.run(
                new vscode.TestRunRequest([adds, connects]),
                new vscode.CancellationTokenSource().token,
                run
            );
            assert.deepStrictEqual(run.log.slice(3), [
                "passed test/foo/adds",
                "errored test/foo/connects/backend=sqlite",
                "errored test/foo/connects/backend=pg",
                "end"
            ]);
            assert.strictEqual(run.messages[0].message, "doze: crashed");
        } finally {
            tests.dispose();
        }
    });
});
