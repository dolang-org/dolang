import * as assert from "assert";
import * as fs from "fs/promises";
import * as os from "os";
import * as path from "path";
import * as tar from "tar";
import * as vscode from "vscode";
import { modulePathArgs, replPayload, splitArgs } from "../args";
import { buildScriptTask } from "../runner";
import {
    binaryFileName,
    GitHubReleaseToolDownloader,
    platformKeyFor,
    resolveExecutionTools,
    resolveTool,
    type ToolContext,
    type ToolLog,
    type ToolName
} from "../tools";

const VERSION = "9.8.7";

suite("Tool Resolution", () => {
    test("configured path wins over PATH and release download", async () => {
        const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));
        const configured = await createExecutable(root, "configured", "shell");
        const onPath = await createExecutable(root, "path", "shell");

        const resolved = await resolveTool("shell", {
            context: createContext(root),
            configuration: createConfiguration({
                path: configured
            }),
            pathEnv: path.dirname(onPath)
        });

        assert.strictEqual(resolved.source, "configured");
        assert.strictEqual(resolved.path, configured);
    });

    test("PATH fallback is used when no explicit path is configured", async () => {
        const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));
        const onPath = await createExecutable(root, "path", "lsp");

        const resolved = await resolveTool("lsp", {
            context: createContext(root),
            configuration: createConfiguration(),
            pathEnv: path.dirname(onPath)
        });

        assert.strictEqual(resolved.source, "path");
        assert.strictEqual(resolved.path, onPath);
    });

    test("downloaded tool is used after PATH misses", async function () {
        if (process.platform === "win32") {
            this.skip();
        }

        const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));
        const downloaded = await createExecutable(
            root,
            path.join(
                "global-storage",
                "tools",
                "releases",
                VERSION,
                platformKeyFor(process.platform, process.arch),
                "bundle"
            ),
            "vfs"
        );

        const resolved = await resolveTool("vfs", {
            context: createContext(root),
            configuration: createConfiguration(),
            pathEnv: path.join(root, "missing")
        });

        assert.strictEqual(resolved.source, "downloaded");
        assert.strictEqual(resolved.path, downloaded);
    });

    test("shell execution resolves the shell", async function () {
        if (process.platform === "win32") {
            this.skip();
        }

        const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));
        const shell = await createExecutable(root, "path", "shell");

        const resolved = await resolveExecutionTools({
            context: createContext(root),
            configuration: createConfiguration(),
            pathEnv: path.dirname(shell)
        });

        assert.strictEqual(resolved.shell.path, shell);
    });

    test("missing tool error reports attempted sources", async () => {
        const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));

        const originalResolve = GitHubReleaseToolDownloader.prototype.resolve;
        GitHubReleaseToolDownloader.prototype.resolve = async () => undefined;
        try {
            await assert.rejects(
                resolveTool("lsp", {
                    context: createContext(root),
                    configuration: createConfiguration(),
                    pathEnv: path.join(root, "missing")
                }),
                (error: unknown) => {
                    assert.ok(error instanceof Error);
                    assert.match(error.message, /Unable to find dolang-lsp/);
                    assert.match(error.message, /PATH/);
                    assert.match(error.message, /downloaded release/);
                    return true;
                }
            );
        } finally {
            GitHubReleaseToolDownloader.prototype.resolve = originalResolve;
        }
    });

    test("downloader computes stable bundle cache paths", () => {
        const root = path.join(os.tmpdir(), "dolang-code-test-static");
        const downloader = new GitHubReleaseToolDownloader(createContext(root), VERSION);
        const key = platformKeyFor(process.platform, process.arch);

        assert.strictEqual(downloader.isEnabled(), true);
        assert.strictEqual(
            downloader.downloadedBinaryPath("shell"),
            path.join(
                root,
                "global-storage",
                "tools",
                "releases",
                VERSION,
                key,
                "bundle",
                binaryFileName("shell")
            )
        );
        assert.strictEqual(
            downloader.downloadedArchivePath(),
            path.join(
                root,
                "global-storage",
                "tools",
                "releases",
                VERSION,
                key,
                `dolang-${VERSION}-${key}.tar.gz`
            )
        );
        assert.strictEqual(downloader.releaseAssetName(), `dolang-${VERSION}-${key}.tar.gz`);
        assert.strictEqual(
            downloader.releaseUrl(),
            `https://api.github.com/repos/dolang-org/dolang/releases/tags/v${VERSION}`
        );
    });

    test("resolution logs where the tool came from", async () => {
        const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));
        const onPath = await createExecutable(root, "path", "lsp");
        const log = createLog();

        await resolveTool("lsp", {
            context: createContext(root),
            configuration: createConfiguration(),
            pathEnv: path.dirname(onPath),
            log
        });

        assert.deepStrictEqual(log.lines, [`info: Resolved dolang-lsp from PATH: ${onPath}`]);
    });
});

suite("Release Downloads", () => {
    test("a missing release names the pinned version", async () => {
        const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));
        const downloader = new GitHubReleaseToolDownloader(createContext(root), VERSION);
        const fetches: string[] = [];

        await withFetch(
            async url => {
                fetches.push(url);
                return new Response("not found", { status: 404 });
            },
            () =>
                assert.rejects(downloader.resolve("shell"), (error: unknown) => {
                    assert.ok(error instanceof Error);
                    assert.match(error.message, new RegExp(`No Do release v${VERSION}`));
                    return true;
                })
        );
        assert.deepStrictEqual(fetches, [downloader.releaseUrl()]);
    });

    test("concurrent resolves share one download and prune other versions", async function () {
        if (process.platform === "win32") {
            this.skip();
        }

        const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));
        const context = createContext(root);
        const releases = path.join(context.globalStorageUri.fsPath, "tools", "releases");
        const key = platformKeyFor(process.platform, process.arch);
        // An older version and the old unversioned layout
        await fs.mkdir(path.join(releases, "0.0.1", key, "bundle"), { recursive: true });
        await fs.mkdir(path.join(releases, key, "bundle"), { recursive: true });

        const archive = await createBundle(root);
        const downloader = new GitHubReleaseToolDownloader(context, VERSION);
        const fetches: string[] = [];
        const resolved = await withFetch(
            async url => {
                fetches.push(url);
                if (url === downloader.releaseUrl()) {
                    return Response.json({
                        tag_name: `v${VERSION}`,
                        assets: [
                            {
                                name: downloader.releaseAssetName(),
                                browser_download_url: "https://example.invalid/bundle"
                            }
                        ]
                    });
                }
                return new Response(await fs.readFile(archive));
            },
            () => Promise.all([downloader.resolve("shell"), downloader.resolve("lsp")])
        );

        assert.strictEqual(fetches.length, 2);
        assert.deepStrictEqual(
            resolved.map(tool => tool?.path),
            [downloader.downloadedBinaryPath("shell"), downloader.downloadedBinaryPath("lsp")]
        );
        assert.deepStrictEqual(await fs.readdir(releases), [VERSION]);
        assert.deepStrictEqual(await fs.readdir(path.join(releases, VERSION, key)), ["bundle"]);
    });

    test("clear removes this version's bundle", async () => {
        const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));
        const downloader = new GitHubReleaseToolDownloader(createContext(root), VERSION);
        const shell = downloader.downloadedBinaryPath("shell");
        await fs.mkdir(path.dirname(shell), { recursive: true });
        await fs.writeFile(shell, "");

        await downloader.clear();

        await assert.rejects(fs.access(shell));
    });
});

suite("Extension Manifest", () => {
    const manifest = vscode.extensions.all.find(
        extension => extension.packageJSON.name === "dolang"
    )?.packageJSON;

    test("declares the trace setting", () => {
        assert.ok(manifest);
        assert.deepStrictEqual(
            manifest.contributes.configuration.properties["dolang.trace.server"].enum,
            ["off", "messages", "verbose"]
        );
    });

    test("restricts tool paths in untrusted workspaces", () => {
        assert.ok(manifest);
        assert.strictEqual(manifest.capabilities.untrustedWorkspaces.supported, "limited");
        assert.deepStrictEqual(manifest.capabilities.untrustedWorkspaces.restrictedConfigurations, [
            "dolang.path",
            "dolang.lsp.path"
        ]);
        assert.strictEqual(manifest.capabilities.virtualWorkspaces, false);
    });

    test("recognizes extensionless scripts by their shebang", async () => {
        const dir = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-shebang-"));
        const languageOf = async (name: string, shebang: string): Promise<string> => {
            const file = path.join(dir, name);
            await fs.writeFile(file, `${shebang}\necho hello\n`);
            return (await vscode.workspace.openTextDocument(vscode.Uri.file(file))).languageId;
        };
        try {
            assert.strictEqual(await languageOf("env", "#!/usr/bin/env dolang"), "dolang");
            assert.strictEqual(await languageOf("direct", "#!/usr/local/bin/dolang"), "dolang");
            assert.strictEqual(
                await languageOf("args", "#!/usr/bin/env -S dolang -m test"),
                "dolang"
            );
            assert.notStrictEqual(await languageOf("sh", "#!/bin/sh"), "dolang");
            assert.notStrictEqual(await languageOf("other", "#!/usr/bin/env dolang-lsp"), "dolang");
        } finally {
            await fs.rm(dir, { recursive: true, force: true });
        }
    });
});

suite("Argument Splitting", () => {
    test("whitespace separates words", () => {
        assert.deepStrictEqual(splitArgs("  a  b\tc\n"), ["a", "b", "c"]);
        assert.deepStrictEqual(splitArgs(""), []);
        assert.deepStrictEqual(splitArgs("   "), []);
    });

    test("quotes group and concatenate", () => {
        assert.deepStrictEqual(splitArgs(`'a b' "c d" e'f'"g"`), ["a b", "c d", "efg"]);
        assert.deepStrictEqual(splitArgs(`'' ""`), ["", ""]);
    });

    test("escapes", () => {
        assert.deepStrictEqual(splitArgs(String.raw`"a\"b" "c\\d" "e\nf"`), [
            `a"b`,
            String.raw`c\d`,
            String.raw`e\nf`
        ]);
        assert.deepStrictEqual(splitArgs(String.raw`a\ b 'c\d'`), ["a b", String.raw`c\d`]);
    });

    test("shell metacharacters are preserved verbatim", () => {
        assert.deepStrictEqual(splitArgs("$HOME `id` ; | & > *"), [
            "$HOME",
            "`id`",
            ";",
            "|",
            "&",
            ">",
            "*"
        ]);
    });

    test("unterminated quotes are errors", () => {
        assert.throws(() => splitArgs("'abc"), /single quote/);
        assert.throws(() => splitArgs('"abc'), /double quote/);
    });
});

suite("REPL Payload", () => {
    test("a single line is sent without its indentation", () => {
        assert.strictEqual(replPayload("    echo hi  ", true), "echo hi");
        assert.strictEqual(replPayload("\n    echo hi\n\n", false), "echo hi");
        assert.strictEqual(replPayload("\n  \n", true), undefined);
    });

    test("several lines become one dedented bracketed paste", () => {
        assert.strictEqual(
            replPayload("\n  if x\r\n\n    echo a\n  else\n    echo b\n", true),
            "\x1b[200~if x\n\n  echo a\nelse\n  echo b\x1b[201~"
        );
    });

    test("without bracketed paste, several lines use REPL multi-line mode", () => {
        assert.strictEqual(replPayload("  if x\n\n    echo a\n", false), "\rif x\r  echo a\r");
    });
});

suite("Script Tasks", () => {
    test("dolang runs directly with an argument vector", () => {
        const script = path.join(os.tmpdir(), `odd "dir" $x \`y\``, "script name.dol");
        const task = buildScriptTask({
            dolang: "/opt/dolang/bin/dolang",
            script,
            args: ["a b", "$HOME", `"q"`],
            cwd: "/work",
            keepOpen: true
        });

        const execution = task.execution;
        assert.ok(execution instanceof vscode.ProcessExecution);
        assert.strictEqual(execution.process, "/opt/dolang/bin/dolang");
        assert.deepStrictEqual(execution.args, [script, "a b", "$HOME", `"q"`]);
        assert.strictEqual(execution.options?.cwd, "/work");
        assert.strictEqual(task.presentationOptions.close, false);
        assert.strictEqual(task.definition.type, "dolang");
    });

    test("keepOpen off closes the terminal", () => {
        const task = buildScriptTask({
            dolang: "dolang",
            script: "/s.dol",
            args: [],
            cwd: "/",
            keepOpen: false
        });
        assert.strictEqual(task.presentationOptions.close, true);
    });

    test("the module path goes before the script", () => {
        const task = buildScriptTask({
            dolang: "dolang",
            options: modulePathArgs(["/p/lib", "/p/vendor/a"]),
            script: "/p/s.dol",
            args: ["x"],
            cwd: "/",
            keepOpen: true
        });
        const execution = task.execution;
        assert.ok(execution instanceof vscode.ProcessExecution);
        assert.deepStrictEqual(execution.args, [
            "--module-path",
            "/p/lib",
            "--module-path",
            "/p/vendor/a",
            "/p/s.dol",
            "x"
        ]);
    });

    test("an empty module path adds no options", () => {
        assert.deepStrictEqual(modulePathArgs([]), []);
    });
});

function createConfiguration(values: Record<string, string> = {}): vscode.WorkspaceConfiguration {
    return {
        get<T>(section: string, defaultValue?: T): T | undefined {
            return (values[section] as T | undefined) ?? defaultValue;
        }
    } as vscode.WorkspaceConfiguration;
}

function createContext(root: string): ToolContext {
    return {
        globalStorageUri: vscode.Uri.file(path.join(root, "global-storage")),
        extension: { packageJSON: { version: VERSION } }
    };
}

function createLog(): ToolLog & { lines: string[] } {
    const lines: string[] = [];
    return {
        lines,
        info: (message: string) => lines.push(`info: ${message}`),
        warn: (message: string) => lines.push(`warn: ${message}`),
        error: (message: string | Error) => lines.push(`error: ${message}`)
    };
}

async function withFetch<T>(
    handler: (_url: string) => Promise<Response>,
    body: () => Promise<T>
): Promise<T> {
    const original = globalThis.fetch;
    globalThis.fetch = (input: string | URL | Request) =>
        handler(input instanceof Request ? input.url : input.toString());
    try {
        return await body();
    } finally {
        globalThis.fetch = original;
    }
}

/** Builds a release archive holding stub executables for every tool. */
async function createBundle(root: string): Promise<string> {
    const tools: ToolName[] = ["shell", "vfs", "lsp"];
    const dir = path.join(root, "bundle-source");
    await Promise.all(tools.map(tool => createExecutable(root, "bundle-source", tool)));
    const archive = path.join(root, "bundle.tar.gz");
    await tar.c({ gzip: true, file: archive, cwd: dir }, tools.map(binaryFileName));
    return archive;
}

async function createExecutable(root: string, directory: string, tool: ToolName): Promise<string> {
    const dir = path.join(root, directory);
    const executablePath = path.join(dir, binaryFileName(tool));
    await fs.mkdir(dir, { recursive: true });
    await fs.writeFile(executablePath, "#!/bin/sh\nexit 0\n");

    if (process.platform !== "win32") {
        await fs.chmod(executablePath, 0o755);
    }

    return executablePath;
}
