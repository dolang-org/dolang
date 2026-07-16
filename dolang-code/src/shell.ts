import * as vscode from "vscode";
import * as path from "path";
import { resolveExecutionTools } from "./tools";

export class DoShellRunner {
    private extensionContext: vscode.ExtensionContext;

    constructor(context: vscode.ExtensionContext) {
        this.extensionContext = context;
    }

    private getKeepTerminalCommand(): string {
        const config = vscode.workspace.getConfiguration("dolang");
        const shouldKeepOpen = config.get<boolean>("keepTerminalOpen", true);

        if (!shouldKeepOpen) {
            return "";
        }

        const isWindows = process.platform === "win32";
        return isWindows
            ? "; echo Press any key to continue... && pause >nul"
            : '; echo "Press any key to continue..." && read -n 1';
    }

    public async runScript(filePath?: string): Promise<void> {
        const config = vscode.workspace.getConfiguration("dolang");

        let scriptPath: string;
        if (filePath) {
            scriptPath = filePath;
        } else {
            const editor = vscode.window.activeTextEditor;
            if (!editor) {
                vscode.window.showErrorMessage("No active editor found");
                return;
            }

            const document = editor.document;
            if (document.languageId !== "dolang") {
                vscode.window.showErrorMessage("Active file is not a Do language file (.dol)");
                return;
            }

            const autoSave = config.get<boolean>("autoSaveBeforeRun", true);
            if (!document.isUntitled && document.isDirty && autoSave) {
                const saveAction = await vscode.window.showWarningMessage(
                    "The file has unsaved changes. Do you want to save it before running?",
                    "Save",
                    "Run without saving",
                    "Cancel"
                );

                if (saveAction === "Save") {
                    await document.save();
                } else if (saveAction === "Cancel" || !saveAction) {
                    return;
                }
            }

            scriptPath = document.fileName;
        }

        const { shell } = await this.resolveExecutionTools();

        // Create new terminal for each execution
        const terminal = this.createScriptTerminal(shell.path, scriptPath);
        terminal.show();
    }

    private async resolveExecutionTools() {
        try {
            return await resolveExecutionTools({ context: this.extensionContext });
        } catch (error) {
            const message =
                error instanceof Error ? error.message : `Failed to resolve Do tools: ${error}`;
            vscode.window.showErrorMessage(message);
            throw error;
        }
    }

    private createScriptTerminal(
        shellPath: string,
        scriptPath: string
    ): vscode.Terminal {
        const timestamp = new Date().toLocaleTimeString();
        const separator = `--- Running ${path.basename(scriptPath)} at ${timestamp} ---`;
        const keepTerminalCmd = this.getKeepTerminalCommand();
        if (keepTerminalCmd) {
            // Use shell command to keep terminal open
            const isWindows = process.platform === "win32";
            const shellPath_wrapper = isWindows ? "cmd" : "bash";
            const shellArgs_wrapper = isWindows
                ? ["/c", `"${shellPath}" "${scriptPath}"${keepTerminalCmd}`]
                : ["-c", `"${shellPath}" "${scriptPath}"${keepTerminalCmd}`];

            return vscode.window.createTerminal({
                name: "Do Script Terminal",
                shellPath: shellPath_wrapper,
                shellArgs: shellArgs_wrapper,
                cwd: path.dirname(scriptPath),
                message: separator
            });
        } else {
            // Direct execution
            return vscode.window.createTerminal({
                name: "Do Script Terminal",
                shellPath,
                shellArgs: [scriptPath],
                cwd: path.dirname(scriptPath),
                message: separator
            });
        }
    }

    public async openInteractiveShell(): Promise<void> {
        const { shell } = await this.resolveExecutionTools();

        const workspaceFolders = vscode.workspace.workspaceFolders;
        const cwd =
            workspaceFolders && workspaceFolders.length > 0
                ? workspaceFolders[0].uri.fsPath
                : undefined;

        const terminal = this.createShellTerminal(shell.path, cwd);
        terminal.show();
    }

    private createShellTerminal(shellPath: string, cwd?: string): vscode.Terminal {
        return vscode.window.createTerminal({
            name: "Do Interactive Shell",
            shellPath,
            cwd,
            message: "--- Do Interactive Shell ---\nType Do commands or use Ctrl+D to exit\n"
        });
    }

    public dispose(): void {
        // No terminals to dispose since we create new ones each time
    }
}
