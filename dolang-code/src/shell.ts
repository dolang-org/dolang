import * as vscode from "vscode";
import { replPayload } from "./args";
import { resolveExecutionTools } from "./tools";

/** Owns the interactive Do shell terminal that send-to-shell targets. */
export class DoInteractiveShell implements vscode.Disposable {
    private terminal: vscode.Terminal | undefined;
    private readonly closeListener: vscode.Disposable;

    private readonly context: vscode.ExtensionContext;

    constructor(context: vscode.ExtensionContext) {
        this.context = context;
        this.closeListener = vscode.window.onDidCloseTerminal(closed => {
            if (closed === this.terminal) {
                this.terminal = undefined;
            }
        });
    }

    /** Opens a new interactive shell, which becomes the send-to-shell target. */
    public async open(): Promise<void> {
        const terminal = await this.create();
        terminal.show();
    }

    /** Sends the selection, or the current line, to the interactive shell. */
    public async sendSelection(): Promise<void> {
        const editor = vscode.window.activeTextEditor;
        if (!editor) {
            vscode.window.showErrorMessage("No active editor found");
            return;
        }
        const selection = editor.selection;
        const text = selection.isEmpty
            ? editor.document.lineAt(selection.active.line).text
            : editor.document.getText(selection);
        const payload = replPayload(text, process.platform !== "win32");

        if (payload !== undefined) {
            const terminal = this.terminal ?? (await this.create());
            terminal.show(true);
            terminal.sendText(payload, true);
        }

        if (selection.isEmpty) {
            const next = Math.min(selection.active.line + 1, editor.document.lineCount - 1);
            const position = new vscode.Position(next, 0);
            editor.selection = new vscode.Selection(position, position);
            editor.revealRange(new vscode.Range(position, position));
        }
    }

    public dispose(): void {
        this.closeListener.dispose();
    }

    private async create(): Promise<vscode.Terminal> {
        let shellPath: string;
        try {
            shellPath = (await resolveExecutionTools({ context: this.context })).shell.path;
        } catch (error) {
            const message =
                error instanceof Error ? error.message : `Failed to resolve Do tools: ${error}`;
            vscode.window.showErrorMessage(message);
            throw error;
        }
        const folders = vscode.workspace.workspaceFolders;
        this.terminal = vscode.window.createTerminal({
            name: "Do Interactive Shell",
            shellPath,
            cwd: folders && folders.length > 0 ? folders[0].uri.fsPath : undefined,
            message: "--- Do Interactive Shell ---\nType Do commands or use Ctrl+D to exit\n"
        });
        return this.terminal;
    }
}
