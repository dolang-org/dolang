import * as assert from "assert";
import * as fs from "fs/promises";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import {
    binaryFileName,
    GitHubReleaseToolDownloader,
    platformTripleFor,
    resolveExecutionTools,
    resolveTool,
    type ToolName
} from "../tools";

suite("Tool Resolution", () => {
    test("configured path wins over PATH and release download", async () => {
        const root = await fs.mkdtemp(path.join(os.tmpdir(), "dolang-code-test-"));
        const configured = await createExecutable(root, "configured", "shell");
        const onPath = await createExecutable(root, "path", "shell");

        const resolved = await resolveTool("shell", {
            context: createContext(root),
            configuration: createConfiguration({
                "path": configured
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
                platformTripleFor(process.platform, process.arch),
                "bundle"
            ),
            "shell-vfs"
        );

        const resolved = await resolveTool("shell-vfs", {
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
    });

    test("downloader computes stable bundle cache paths", () => {
        const root = path.join(os.tmpdir(), "dolang-code-test-static");
        const downloader = new GitHubReleaseToolDownloader(createContext(root));
        const triple = platformTripleFor(process.platform, process.arch);

        assert.strictEqual(downloader.isEnabled(), true);
        assert.strictEqual(
            downloader.downloadedBinaryPath("shell"),
            path.join(
                root,
                "global-storage",
                "tools",
                "releases",
                triple,
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
                triple,
                `dolang-${triple}.tar.gz`
            )
        );
        assert.strictEqual(downloader.releaseAssetName(), `dolang-${triple}.tar.gz`);
    });
});

function createConfiguration(values: Record<string, string> = {}): vscode.WorkspaceConfiguration {
    return {
        get<T>(section: string, defaultValue?: T): T | undefined {
            return (values[section] as T | undefined) ?? defaultValue;
        }
    } as vscode.WorkspaceConfiguration;
}

function createContext(root: string): Pick<vscode.ExtensionContext, "globalStorageUri"> {
    return {
        globalStorageUri: vscode.Uri.file(path.join(root, "global-storage"))
    };
}

async function createExecutable(
    root: string,
    directory: string,
    tool: ToolName
): Promise<string> {
    const dir = path.join(root, directory);
    const executablePath = path.join(dir, binaryFileName(tool));
    await fs.mkdir(dir, { recursive: true });
    await fs.writeFile(executablePath, "#!/bin/sh\nexit 0\n");

    if (process.platform !== "win32") {
        await fs.chmod(executablePath, 0o755);
    }

    return executablePath;
}
