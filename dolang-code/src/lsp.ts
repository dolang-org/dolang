import * as path from "path";
import * as vscode from "vscode";
import { Executable, LanguageClient, LanguageClientOptions } from "vscode-languageclient/node";
import { extensionVersion, resolveTool, type ToolSource } from "./tools";

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}

/** Starts, stops and restarts the Do language server. */
export class DoLanguageServer implements vscode.Disposable {
    private client: LanguageClient | undefined;
    private queue: Promise<void> = Promise.resolve();
    private warnedVersion = false;

    private readonly context: vscode.ExtensionContext;
    private readonly log: vscode.LogOutputChannel;
    // Kept across restarts, so server output stays in one place
    private readonly serverOutput: vscode.LogOutputChannel;

    constructor(context: vscode.ExtensionContext, log: vscode.LogOutputChannel) {
        this.context = context;
        this.log = log;
        this.serverOutput = vscode.window.createOutputChannel("Do Language Server", { log: true });
    }

    /** Starts the server unless it is running or the workspace is untrusted. */
    public start(): Promise<void> {
        return this.enqueue(() => this.startNow());
    }

    public stop(): Promise<void> {
        return this.enqueue(() => this.stopNow());
    }

    public restart(): Promise<void> {
        return this.enqueue(async () => {
            await this.stopNow();
            await this.startNow();
        });
    }

    public dispose(): void {
        void this.stop();
        this.serverOutput.dispose();
    }

    private enqueue(operation: () => Promise<void>): Promise<void> {
        this.queue = this.queue.then(operation, operation);
        return this.queue;
    }

    private async startNow(): Promise<void> {
        if (this.client) {
            return;
        }
        if (!vscode.workspace.isTrusted) {
            this.log.info("Not starting the language server in an untrusted workspace");
            return;
        }

        let client: LanguageClient | undefined;
        let source: ToolSource;
        try {
            const server = await resolveTool("lsp", { context: this.context, log: this.log });
            source = server.source;
            const executable: Executable = {
                command: server.path,
                args: [],
                options: { cwd: path.dirname(server.path) }
            };
            const clientOptions: LanguageClientOptions = {
                documentSelector: [{ scheme: "file", language: "dolang" }],
                synchronize: {
                    fileEvents: vscode.workspace.createFileSystemWatcher("**/*.dol")
                },
                outputChannel: this.serverOutput
            };
            client = new LanguageClient("dolang", "Do Language Server", executable, clientOptions);
            await client.start();
        } catch (error) {
            const message = `Failed to start the Do language server: ${errorMessage(error)}`;
            this.log.error(message);
            await client?.dispose().catch(() => undefined);
            void vscode.window
                .showErrorMessage(message, "Show Output", "Open Settings")
                .then(choice => {
                    if (choice === "Show Output") {
                        this.log.show();
                    } else if (choice === "Open Settings") {
                        void vscode.commands.executeCommand(
                            "workbench.action.openSettings",
                            "dolang.lsp.path"
                        );
                    }
                });
            return;
        }

        this.client = client;
        this.checkVersion(client, source);
    }

    private async stopNow(): Promise<void> {
        const client = this.client;
        this.client = undefined;
        if (!client) {
            return;
        }
        try {
            await client.stop();
        } catch (error) {
            this.log.warn(`Stopping the language server failed: ${errorMessage(error)}`);
        }
        await client.dispose().catch(() => undefined);
    }

    private checkVersion(client: LanguageClient, source: ToolSource): void {
        const expected = extensionVersion(this.context);
        const actual = client.initializeResult?.serverInfo?.version;
        if (actual === undefined) {
            this.log.info("The language server did not report its version");
            return;
        }
        if (actual === expected) {
            this.log.info(`Language server v${actual} started`);
            return;
        }
        const message = `dolang-lsp v${actual} does not match the Do extension v${expected}`;
        this.log.warn(message);
        if (source !== "downloaded" && !this.warnedVersion) {
            this.warnedVersion = true;
            void vscode.window.showWarningMessage(`${message}; some features may not work`);
        }
    }
}
