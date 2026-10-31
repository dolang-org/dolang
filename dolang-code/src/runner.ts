import * as fs from "fs/promises";
import * as path from "path";
import * as vscode from "vscode";
import { splitArgs } from "./args";
import { resolveExecutionTools } from "./tools";

export const TASK_TYPE = "dolang";

export interface DoTaskDefinition extends vscode.TaskDefinition {
    readonly type: typeof TASK_TYPE;
    readonly script: string;
    readonly args?: string[];
    readonly cwd?: string;
}

export interface ScriptTaskOptions {
    readonly dolang: string;
    readonly script: string;
    readonly args: readonly string[];
    readonly cwd: string;
    readonly keepOpen: boolean;
    readonly scope?: vscode.WorkspaceFolder | vscode.TaskScope;
    readonly definition?: DoTaskDefinition;
}

/**
 * Builds a task that runs `dolang` directly with an argument vector, so no
 * shell ever parses the script path or its arguments.
 */
export function buildScriptTask(options: ScriptTaskOptions): vscode.Task {
    const definition: DoTaskDefinition = options.definition ?? {
        type: TASK_TYPE,
        script: options.script,
        args: [...options.args],
        cwd: options.cwd
    };
    const execution = new vscode.ProcessExecution(
        options.dolang,
        [options.script, ...options.args],
        {
            cwd: options.cwd
        }
    );
    const task = new vscode.Task(
        definition,
        options.scope ?? vscode.TaskScope.Workspace,
        path.basename(options.script),
        TASK_TYPE,
        execution,
        []
    );
    task.presentationOptions = {
        reveal: vscode.TaskRevealKind.Always,
        panel: vscode.TaskPanelKind.Dedicated,
        clear: true,
        echo: false,
        close: !options.keepOpen
    };
    return task;
}

function keepOpenSetting(): boolean {
    return vscode.workspace.getConfiguration("dolang").get<boolean>("keepTerminalOpen", true);
}

/**
 * Returns whether the workspace is trusted, telling the user why nothing ran
 * when it isn't.
 */
export function requireTrustedWorkspace(): boolean {
    if (vscode.workspace.isTrusted) {
        return true;
    }
    void vscode.window.showErrorMessage("Running Do scripts requires a trusted workspace");
    return false;
}

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}

export class DoScriptRunner implements vscode.TaskProvider {
    private static readonly LAST_ARGS_KEY = "dolang.lastArgs";

    private readonly context: vscode.ExtensionContext;
    private readonly log: vscode.LogOutputChannel;

    constructor(context: vscode.ExtensionContext, log: vscode.LogOutputChannel) {
        this.context = context;
        this.log = log;
    }

    /** Runs the script at `uri`, or the active editor's document. */
    public async run(uri?: vscode.Uri, args: readonly string[] = []): Promise<void> {
        if (!requireTrustedWorkspace()) {
            return;
        }
        const target = this.targetDocument(uri);
        if (!target) {
            return;
        }
        const prepared = await this.prepare(target);
        if (!prepared) {
            return;
        }
        const { shell } = await this.resolveExecutionTools();
        const folder = vscode.workspace.getWorkspaceFolder(target.uri);
        const task = buildScriptTask({
            dolang: shell.path,
            script: prepared.script,
            args,
            cwd: prepared.cwd,
            keepOpen: keepOpenSetting(),
            scope: folder
        });
        await vscode.tasks.executeTask(task);
    }

    /** Prompts for arguments, then runs like {@link run}. */
    public async runWithArgs(uri?: vscode.Uri): Promise<void> {
        if (!requireTrustedWorkspace()) {
            return;
        }
        const target = this.targetDocument(uri);
        if (!target) {
            return;
        }
        const key = target.uri.toString();
        const remembered = this.context.workspaceState.get<Record<string, string>>(
            DoScriptRunner.LAST_ARGS_KEY,
            {}
        );
        const input = await vscode.window.showInputBox({
            title: `Run ${path.basename(target.uri.path)} with Arguments`,
            prompt: "Arguments, separated by spaces; quote with ' or \"",
            value: remembered[key] ?? "",
            validateInput: text => {
                try {
                    splitArgs(text);
                    return undefined;
                } catch (error) {
                    return errorMessage(error);
                }
            }
        });
        if (input === undefined) {
            return;
        }
        await this.context.workspaceState.update(DoScriptRunner.LAST_ARGS_KEY, {
            ...remembered,
            [key]: input
        });
        await this.run(target.uri, splitArgs(input));
    }

    public provideTasks(): vscode.Task[] {
        return [];
    }

    public async resolveTask(task: vscode.Task): Promise<vscode.Task | undefined> {
        const definition = task.definition as DoTaskDefinition;
        if (definition.type !== TASK_TYPE || !requireTrustedWorkspace()) {
            return undefined;
        }
        if (typeof definition.script !== "string" || definition.script === "") {
            vscode.window.showErrorMessage("A dolang task needs a `script` path");
            return undefined;
        }
        const folder = typeof task.scope === "object" ? task.scope : undefined;
        const base = folder?.uri.fsPath;
        const resolve = (p: string): string | undefined => {
            if (path.isAbsolute(p)) {
                return p;
            }
            return base ? path.join(base, p) : undefined;
        };
        const script = resolve(definition.script);
        const cwd =
            definition.cwd !== undefined ? resolve(definition.cwd) : script && path.dirname(script);
        if (!script || !cwd) {
            vscode.window.showErrorMessage(
                "A dolang task outside a workspace folder needs absolute `script` and `cwd` paths"
            );
            return undefined;
        }
        const { shell } = await this.resolveExecutionTools();
        return buildScriptTask({
            dolang: shell.path,
            script,
            args: definition.args ?? [],
            cwd,
            keepOpen: keepOpenSetting(),
            scope: task.scope,
            definition
        });
    }

    private targetDocument(
        uri?: vscode.Uri
    ): { uri: vscode.Uri; document?: vscode.TextDocument } | undefined {
        if (uri) {
            const document = vscode.workspace.textDocuments.find(
                d => d.uri.toString() === uri.toString()
            );
            return { uri, document };
        }
        const editor = vscode.window.activeTextEditor;
        if (!editor) {
            vscode.window.showErrorMessage("No active editor found");
            return undefined;
        }
        if (editor.document.languageId !== "dolang") {
            vscode.window.showErrorMessage("Active file is not a Do language file (.dol)");
            return undefined;
        }
        return { uri: editor.document.uri, document: editor.document };
    }

    /** Saves or materializes the document, returning the file to run. */
    private async prepare(target: {
        uri: vscode.Uri;
        document?: vscode.TextDocument;
    }): Promise<{ script: string; cwd: string } | undefined> {
        const { uri, document } = target;
        if (uri.scheme === "untitled") {
            if (!document) {
                vscode.window.showErrorMessage("The untitled document is no longer open");
                return undefined;
            }
            const dir = vscode.Uri.joinPath(this.context.globalStorageUri, "untitled").fsPath;
            const name = path.basename(uri.path).replace(/[^A-Za-z0-9_-]/g, "_") || "untitled";
            const script = path.join(dir, `${name}.dol`);
            try {
                await fs.mkdir(dir, { recursive: true });
                await fs.writeFile(script, document.getText());
            } catch (error) {
                vscode.window.showErrorMessage(
                    `Failed to write untitled script: ${errorMessage(error)}`
                );
                return undefined;
            }
            const folders = vscode.workspace.workspaceFolders;
            const cwd = folders && folders.length > 0 ? folders[0].uri.fsPath : dir;
            return { script, cwd };
        }
        if (uri.scheme !== "file") {
            vscode.window.showErrorMessage(
                `Only local files can be run (got a ${uri.scheme}: document)`
            );
            return undefined;
        }
        const autoSave = vscode.workspace
            .getConfiguration("dolang")
            .get<boolean>("autoSaveBeforeRun", true);
        if (document?.isDirty && autoSave && !(await document.save())) {
            vscode.window.showErrorMessage(`Failed to save ${path.basename(uri.fsPath)}`);
            return undefined;
        }
        return { script: uri.fsPath, cwd: path.dirname(uri.fsPath) };
    }

    private async resolveExecutionTools() {
        try {
            return await resolveExecutionTools({ context: this.context, log: this.log });
        } catch (error) {
            vscode.window.showErrorMessage(errorMessage(error));
            throw error;
        }
    }
}
