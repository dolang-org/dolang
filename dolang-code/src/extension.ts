import * as vscode from "vscode";
import { DoLanguageServer } from "./lsp";
import { DoScriptRunner, requireTrustedWorkspace, TASK_TYPE } from "./runner";
import { DoInteractiveShell } from "./shell";
import { DoTestController } from "./testing";
import {
    extensionVersion,
    GitHubReleaseToolDownloader,
    resolveExecutionTools,
    resolveTool,
    type ToolName
} from "./tools";

let server: DoLanguageServer | undefined;

/**
 * What `activate` returns, for the integration tests to observe the extension
 * with. It isn't an API for other extensions.
 */
export interface DoExtensionExports {
    readonly languageServer: DoLanguageServer;
    readonly tests: DoTestController;
}

export async function activate(context: vscode.ExtensionContext): Promise<DoExtensionExports> {
    const log = vscode.window.createOutputChannel("Do", { log: true });
    const languageServer = new DoLanguageServer(context, log);
    const runner = new DoScriptRunner(context, log, languageServer);
    const shell = new DoInteractiveShell(context, log, languageServer);
    const tests = new DoTestController({
        projectRoot: uri => languageServer.projectRoot(uri),
        dolang: async () => (await resolveExecutionTools({ context, log })).shell.path,
        log
    });
    server = languageServer;
    const ignore = (promise: Promise<void>): void => {
        void promise.catch(() => undefined);
    };

    context.subscriptions.push(
        log,
        shell,
        tests,
        languageServer,
        vscode.tasks.registerTaskProvider(TASK_TYPE, runner),
        vscode.commands.registerCommand("dolang.runScript", () => ignore(runner.run())),
        vscode.commands.registerCommand("dolang.runScriptFile", (uri?: vscode.Uri) =>
            ignore(runner.run(uri))
        ),
        vscode.commands.registerCommand("dolang.runScriptWithArgs", (uri?: vscode.Uri) =>
            ignore(runner.runWithArgs(uri))
        ),
        vscode.commands.registerCommand("dolang.openInteractiveShell", () => ignore(shell.open())),
        vscode.commands.registerCommand("dolang.sendSelectionToShell", () =>
            ignore(shell.sendSelection())
        ),
        vscode.commands.registerCommand("dolang.restartLanguageServer", () => {
            if (!vscode.workspace.isTrusted) {
                void vscode.window.showInformationMessage(
                    "The Do language server starts once the workspace is trusted"
                );
                return;
            }
            ignore(languageServer.restart());
        }),
        vscode.commands.registerCommand("dolang.redownloadTools", () =>
            ignore(redownloadTools(context, log, languageServer))
        ),
        vscode.workspace.onDidChangeConfiguration(event => {
            if (event.affectsConfiguration("dolang.lsp.path")) {
                log.info("dolang.lsp.path changed; restarting the language server");
                ignore(languageServer.restart());
            }
        }),
        vscode.workspace.onDidGrantWorkspaceTrust(() => ignore(languageServer.start()))
    );

    await languageServer.start();
    return { languageServer, tests };
}

export async function deactivate(): Promise<void> {
    await server?.stop();
    server = undefined;
}

/** Replaces the cached tool bundle for this extension version and restarts the server. */
async function redownloadTools(
    context: vscode.ExtensionContext,
    log: vscode.LogOutputChannel,
    languageServer: DoLanguageServer
): Promise<void> {
    if (!requireTrustedWorkspace()) {
        return;
    }
    const version = extensionVersion(context);
    const downloader = new GitHubReleaseToolDownloader(context, version, log);
    try {
        await downloader.clear();
        await Promise.all([downloader.resolve("shell"), downloader.resolve("lsp")]);
    } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        void vscode.window.showErrorMessage(`Failed to download Do tools: ${message}`);
        return;
    }

    // Configured paths and PATH take precedence over the downloaded bundle
    const overridden: string[] = [];
    for (const tool of ["shell", "lsp"] as ToolName[]) {
        try {
            const resolved = await resolveTool(tool, { context });
            if (resolved.source !== "downloaded") {
                overridden.push(
                    `${resolved.path} (${resolved.source === "path" ? "PATH" : "configured"})`
                );
            }
        } catch {
            // Reported when the tool is next used
        }
    }
    await languageServer.restart();
    if (overridden.length > 0) {
        void vscode.window.showWarningMessage(
            `Downloaded Do tools v${version}, but these are used instead: ${overridden.join(", ")}`
        );
    } else {
        void vscode.window.showInformationMessage(`Downloaded Do tools v${version}`);
    }
}
