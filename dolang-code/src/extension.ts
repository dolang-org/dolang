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
import { DoShellRunner } from "./shell";
import { resolveTool } from "./tools";

let client: LanguageClient | undefined;
let shellRunner: DoShellRunner | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
    // Initialize shell runner
    shellRunner = new DoShellRunner(context);

    // Register shell commands
    const runScriptCommand = vscode.commands.registerCommand("dolang.runScript", () => {
        void shellRunner?.runScript().catch(() => undefined);
    });

    const runScriptFileCommand = vscode.commands.registerCommand(
        "dolang.runScriptFile",
        (uri: vscode.Uri) => {
            if (uri) {
                void shellRunner?.runScript(uri.fsPath).catch(() => undefined);
            } else {
                void shellRunner?.runScript().catch(() => undefined);
            }
        }
    );

    const openInteractiveShellCommand = vscode.commands.registerCommand(
        "dolang.openInteractiveShell",
        () => {
            void shellRunner?.openInteractiveShell().catch(() => undefined);
        }
    );

    context.subscriptions.push(runScriptCommand, runScriptFileCommand, openInteractiveShellCommand);

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
    if (shellRunner) {
        shellRunner.dispose();
    }
}
