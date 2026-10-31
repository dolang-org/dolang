import * as vscode from "vscode";
import * as path from "path";
import {
    Executable,
    LanguageClient,
    LanguageClientOptions,
    ErrorAction,
    CloseAction,
    ErrorHandlerResult,
    CloseHandlerResult
} from "vscode-languageclient/node";
import { DoScriptRunner, TASK_TYPE } from "./runner";
import { DoInteractiveShell } from "./shell";
import { resolveTool } from "./tools";

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
    const runner = new DoScriptRunner(context);
    const shell = new DoInteractiveShell(context);
    const ignore = (promise: Promise<void>): void => {
        void promise.catch(() => undefined);
    };

    context.subscriptions.push(
        shell,
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
        )
    );

    // Start LSP server
    const clientOptions: LanguageClientOptions = {
        documentSelector: [{ scheme: "file", language: "dolang" }],
        synchronize: {
            fileEvents: vscode.workspace.createFileSystemWatcher("**/*.dol")
        },
        errorHandler: {
            error: (
                _error: Error,
                _message: unknown,
                _count: number | undefined
            ): ErrorHandlerResult => {
                return { action: ErrorAction.Continue };
            },
            closed: (): CloseHandlerResult => {
                client = undefined;
                return { action: CloseAction.Restart };
            }
        }
    };

    try {
        const server = await resolveTool("lsp", { context });
        const serverExecutable: Executable = {
            command: server.path,
            args: [],
            options: { cwd: path.dirname(server.path) }
        };
        client = new LanguageClient(
            "dolang",
            "Do Language Server",
            serverExecutable,
            clientOptions
        );
        context.subscriptions.push(client);
        context.subscriptions.push({
            dispose: async () => {
                await client?.stop();
            }
        });
        await client.start();
    } catch (error) {
        const message =
            error instanceof Error ? error.message : `Failed to start language client: ${error}`;
        vscode.window.showErrorMessage(`Failed to start language client: ${message}`);
        throw error;
    }
}

export async function deactivate(): Promise<void> {
    if (client) {
        await client.stop();
        client = undefined;
    }
}
