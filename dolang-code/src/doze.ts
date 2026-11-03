import { createHash } from "crypto";
import * as fs from "fs/promises";
import * as path from "path";
import * as vscode from "vscode";
import { requireTrustedWorkspace } from "./runner";
import { charToUtf16Column } from "./testing";
import type { ToolLog } from "./tools";

export const DOZE_TASK_TYPE = "doze";

/** The `doze` commands the extension runs. */
export type DozeCommand = "check" | "bundle";

export interface DozeTaskDefinition extends vscode.TaskDefinition {
    readonly type: typeof DOZE_TASK_TYPE;
    readonly command: DozeCommand;
    readonly args?: string[];
}

/** The `dolang` arguments that run `doze command` on the project at `root`. */
export function dozeArgs(
    root: string,
    command: DozeCommand,
    args: readonly string[],
    report: string
): string[] {
    return ["-m", "doze", "--project", root, command, ...args, "--json", report];
}

/**
 * Where `doze` writes the report of the project at `root`, under `storage`.
 * It is the same on every run, so a task VS Code reruns with the same
 * arguments writes the report the extension reads.
 */
export function reportPath(storage: vscode.Uri, root: string): string {
    const hash = createHash("sha256").update(root).digest("hex").slice(0, 32);
    return path.join(storage.fsPath, "doze", `${hash}.json`);
}

export interface DozeTaskOptions {
    readonly dolang: string;
    readonly root: string;
    readonly command: DozeCommand;
    readonly args?: readonly string[];
    readonly report: string;
    readonly keepOpen: boolean;
    readonly scope?: vscode.WorkspaceFolder | vscode.TaskScope;
    readonly definition?: DozeTaskDefinition;
}

/**
 * Builds a task that runs `doze` in the project's root, with `dolang`
 * started directly rather than through a shell or a `doze` symlink.
 */
export function buildDozeTask(options: DozeTaskOptions): vscode.Task {
    const args = options.args ?? [];
    const definition: DozeTaskDefinition = options.definition ?? {
        type: DOZE_TASK_TYPE,
        command: options.command,
        args: [...args]
    };
    const execution = new vscode.ProcessExecution(
        options.dolang,
        dozeArgs(options.root, options.command, args, options.report),
        { cwd: options.root }
    );
    const task = new vscode.Task(
        definition,
        options.scope ?? vscode.TaskScope.Workspace,
        options.command,
        DOZE_TASK_TYPE,
        execution,
        []
    );
    task.group = options.command === "bundle" ? vscode.TaskGroup.Build : vscode.TaskGroup.Test;
    task.presentationOptions = {
        reveal: vscode.TaskRevealKind.Always,
        panel: vscode.TaskPanelKind.Dedicated,
        clear: true,
        echo: false,
        close: !options.keepOpen
    };
    return task;
}

/** The project root and report path of a task {@link buildDozeTask} built. */
export function dozeTaskPaths(task: vscode.Task): { root: string; report: string } | undefined {
    const execution = task.execution;
    if (
        task.definition.type !== DOZE_TASK_TYPE ||
        !(execution instanceof vscode.ProcessExecution)
    ) {
        return undefined;
    }
    const args = execution.args;
    const project = args.indexOf("--project");
    const json = args.lastIndexOf("--json");
    if (project < 0 || json < 0 || project + 1 >= args.length || json + 1 >= args.length) {
        return undefined;
    }
    return { root: args[project + 1], report: args[json + 1] };
}

/** A span as `doze` reports it: zero-based, in characters. */
export interface DozeSpan {
    readonly start: { readonly line: number; readonly column: number };
    readonly len: number;
}

interface DozeLocation {
    readonly path: string;
    readonly span: DozeSpan;
    readonly message?: string;
}

interface DozeDiagnostic extends DozeLocation {
    readonly severity: string;
    readonly message: string;
    readonly annotations?: readonly (DozeLocation & { readonly kind?: string })[];
    readonly notes?: readonly { readonly kind: string; readonly message: string }[];
}

interface DozeReport {
    readonly diagnostics: readonly DozeDiagnostic[];
}

/**
 * The range of `span` in `text`, in UTF-16 code units as VS Code counts them.
 * The span's length can run onto later lines; a span past the end of the
 * text ends there.
 */
export function spanRange(text: string, span: DozeSpan): vscode.Range {
    const lines = text.split("\n");
    const visible = (line: number) => (lines[line] ?? "").replace(/\r$/, "");
    const position = (line: number, column: number) =>
        new vscode.Position(line, charToUtf16Column(visible(line), column));

    let line = span.start.line;
    let chars = [...(lines[line] ?? "")].length;
    let column = Math.min(span.start.column, chars);
    const start = position(line, column);
    let remaining = span.len;
    while (remaining > chars - column) {
        if (line + 1 >= lines.length) {
            return new vscode.Range(start, position(line, chars));
        }
        // The rest of the line and its `\n`
        remaining -= chars - column + 1;
        line++;
        chars = [...lines[line]].length;
        column = 0;
    }
    return new vscode.Range(start, position(line, column + remaining));
}

function severity(name: string): vscode.DiagnosticSeverity {
    return name === "error" ? vscode.DiagnosticSeverity.Error : vscode.DiagnosticSeverity.Warning;
}

/**
 * Reads `doze`'s JSON report from `file` as diagnostics, by source path.
 * Paths are resolved against `root`. Returns `undefined` when there is no
 * report, or it can't be parsed.
 */
export async function readReport(
    file: string,
    root: string
): Promise<Map<string, vscode.Diagnostic[]> | undefined> {
    let report: DozeReport;
    try {
        report = JSON.parse(await fs.readFile(file, "utf8")) as DozeReport;
    } catch {
        return undefined;
    }
    if (!Array.isArray(report?.diagnostics)) {
        return undefined;
    }
    const sources = new Map<string, Promise<string | undefined>>();
    // The text of the source at `file`, or `undefined` if it can't be read,
    // as for a bundled typelib
    const source = (file: string): Promise<string | undefined> => {
        let text = sources.get(file);
        if (!text) {
            text = fs.readFile(file, "utf8").then(
                t => t,
                () => undefined
            );
            sources.set(file, text);
        }
        return text;
    };

    const result = new Map<string, vscode.Diagnostic[]>();
    for (const diag of report.diagnostics) {
        const file = path.resolve(root, diag.path);
        const text = await source(file);
        const range = spanRange(text ?? "", diag.span);
        const notes = (diag.notes ?? []).map(
            (note: { kind: string; message: string }) => `${note.kind}: ${note.message}`
        );
        const diagnostic = new vscode.Diagnostic(
            range,
            [diag.message, ...notes].join("\n"),
            severity(diag.severity)
        );
        diagnostic.source = DOZE_TASK_TYPE;
        const related: vscode.DiagnosticRelatedInformation[] = [];
        for (const annotation of diag.annotations ?? []) {
            if (!annotation.message) {
                continue;
            }
            const other = path.resolve(root, annotation.path);
            const otherText = await source(other);
            if (otherText === undefined) {
                continue;
            }
            related.push(
                new vscode.DiagnosticRelatedInformation(
                    new vscode.Location(
                        vscode.Uri.file(other),
                        spanRange(otherText, annotation.span)
                    ),
                    annotation.message
                )
            );
        }
        if (related.length > 0) {
            diagnostic.relatedInformation = related;
        }
        let list = result.get(file);
        if (!list) {
            list = [];
            result.set(file, list);
        }
        list.push(diagnostic);
    }
    return result;
}

function isUnder(file: string, dir: string): boolean {
    const relative = path.relative(dir, file);
    return !relative.startsWith("..") && !path.isAbsolute(relative);
}

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}

/** What the `doze` runner needs from the rest of the extension. */
export interface DozeDependencies {
    /** The root of the project holding `uri`, as the language server reports it */
    projectRoot(_uri: vscode.Uri): Promise<string | undefined>;
    /** The `dolang` executable to run */
    dolang(): Promise<string>;
    /** Where reports are written */
    readonly storage: vscode.Uri;
    readonly log: ToolLog;
    /** Whether the workspace is trusted; tests substitute their own */
    readonly trusted?: () => boolean;
    /** Runs a task; tests substitute their own */
    readonly execute?: (_task: vscode.Task) => Thenable<unknown>;
    /** Shows an error message; tests substitute their own */
    readonly showError?: (_message: string) => void;
}

/**
 * Checks and bundles projects with `doze`: the commands, the `doze` task
 * type, and the reports' diagnostics in the Problems panel.
 */
export class DozeRunner implements vscode.TaskProvider, vscode.Disposable {
    public readonly diagnostics = vscode.languages.createDiagnosticCollection(DOZE_TASK_TYPE);

    private readonly deps: DozeDependencies;
    private readonly updated = new vscode.EventEmitter<string>();
    private readonly subscriptions: vscode.Disposable[];

    /** Fires with a project's root once its report has been read. */
    public readonly onDidReadReport = this.updated.event;

    constructor(deps: DozeDependencies) {
        this.deps = deps;
        const drop = (document: vscode.TextDocument) => this.diagnostics.delete(document.uri);
        this.subscriptions = [
            this.diagnostics,
            this.updated,
            vscode.tasks.onDidStartTask(event => {
                const paths = dozeTaskPaths(event.execution.task);
                if (paths) {
                    void fs.rm(paths.report, { force: true }).catch(() => undefined);
                }
            }),
            vscode.tasks.onDidEndTaskProcess(event => {
                void this.taskEnded(event.execution.task);
            }),
            vscode.workspace.onDidOpenTextDocument(drop),
            vscode.workspace.onDidChangeTextDocument(event => drop(event.document))
        ];
    }

    public dispose(): void {
        for (const subscription of this.subscriptions) {
            subscription.dispose();
        }
    }

    /**
     * Runs `doze command` on the project holding `uri`, a file or directory,
     * or else the active editor's document.
     */
    public async run(command: DozeCommand, uri?: vscode.Uri): Promise<void> {
        if (!requireTrustedWorkspace("Running doze", this.deps.trusted?.())) {
            return;
        }
        const target = uri ?? vscode.window.activeTextEditor?.document.uri;
        if (!target) {
            this.showError("No active editor or selected resource to find the project of");
            return;
        }
        if (target.scheme !== "file") {
            this.showError(`Only local projects can be run (got a ${target.scheme}: document)`);
            return;
        }
        const root = await this.root(target);
        if (!root) {
            return;
        }
        const task = await this.task(
            command,
            [],
            root,
            vscode.workspace.getWorkspaceFolder(target)
        );
        if (!task) {
            return;
        }
        await this.saveDocuments(root);
        await fs.rm(reportPath(this.deps.storage, root), { force: true });
        await (this.deps.execute ?? vscode.tasks.executeTask)(task);
    }

    public async provideTasks(): Promise<vscode.Task[]> {
        if (!(this.deps.trusted?.() ?? vscode.workspace.isTrusted)) {
            return [];
        }
        const tasks: vscode.Task[] = [];
        const roots = new Set<string>();
        for (const folder of vscode.workspace.workspaceFolders ?? []) {
            const root = await this.deps.projectRoot(folder.uri);
            if (!root || roots.has(root)) {
                continue;
            }
            roots.add(root);
            for (const command of ["bundle", "check"] as const) {
                const task = await this.task(command, [], root, folder);
                if (task) {
                    tasks.push(task);
                }
            }
        }
        return tasks;
    }

    public async resolveTask(task: vscode.Task): Promise<vscode.Task | undefined> {
        const definition = task.definition as DozeTaskDefinition;
        if (
            definition.type !== DOZE_TASK_TYPE ||
            !requireTrustedWorkspace("Running doze", this.deps.trusted?.())
        ) {
            return undefined;
        }
        if (definition.command !== "check" && definition.command !== "bundle") {
            this.showError("A doze task's `command` must be `check` or `bundle`");
            return undefined;
        }
        const folder = typeof task.scope === "object" ? task.scope : undefined;
        if (!folder) {
            this.showError("A doze task must belong to a workspace folder");
            return undefined;
        }
        const root = await this.root(folder.uri);
        if (!root) {
            return undefined;
        }
        return this.task(definition.command, definition.args ?? [], root, folder, definition);
    }

    /**
     * Reads the report of a finished `doze` task into the Problems panel,
     * replacing its project's entries. Documents open in an editor are left
     * to the language server.
     */
    public async taskEnded(task: vscode.Task): Promise<void> {
        const paths = dozeTaskPaths(task);
        if (!paths) {
            return;
        }
        const { root, report } = paths;
        const results = await readReport(report, root);
        await fs.rm(report, { force: true }).catch(() => undefined);
        const stale: vscode.Uri[] = [];
        this.diagnostics.forEach(uri => {
            if (isUnder(uri.fsPath, root)) {
                stale.push(uri);
            }
        });
        for (const uri of stale) {
            this.diagnostics.delete(uri);
        }
        if (!results) {
            this.deps.log.info(`No doze report for ${root}`);
        } else {
            const open = new Set(vscode.workspace.textDocuments.map(d => d.uri.toString()));
            for (const [file, diagnostics] of results) {
                const uri = vscode.Uri.file(file);
                if (!open.has(uri.toString())) {
                    this.diagnostics.set(uri, diagnostics);
                }
            }
        }
        this.updated.fire(root);
    }

    private async task(
        command: DozeCommand,
        args: readonly string[],
        root: string,
        scope: vscode.WorkspaceFolder | undefined,
        definition?: DozeTaskDefinition
    ): Promise<vscode.Task | undefined> {
        let dolang: string;
        try {
            dolang = await this.deps.dolang();
        } catch (error) {
            this.showError(errorMessage(error));
            return undefined;
        }
        await fs.mkdir(path.dirname(reportPath(this.deps.storage, root)), { recursive: true });
        return buildDozeTask({
            dolang,
            root,
            command,
            args,
            report: reportPath(this.deps.storage, root),
            keepOpen: vscode.workspace
                .getConfiguration("dolang")
                .get<boolean>("keepTerminalOpen", true),
            scope,
            definition
        });
    }

    /** The root of the project holding `uri`, telling the user when there's none. */
    private async root(uri: vscode.Uri): Promise<string | undefined> {
        const root = await this.deps.projectRoot(uri);
        if (!root) {
            this.showError(
                "Finding the Do project needs the Do language server, which isn't running or didn't answer"
            );
        }
        return root;
    }

    /** Saves modified Do documents in the project, when the settings ask for it. */
    private async saveDocuments(root: string): Promise<void> {
        const autoSave = vscode.workspace
            .getConfiguration("dolang")
            .get<boolean>("autoSaveBeforeRun", true);
        if (!autoSave) {
            return;
        }
        const dirty = vscode.workspace.textDocuments.filter(
            document =>
                document.isDirty &&
                document.languageId === "dolang" &&
                document.uri.scheme === "file" &&
                isUnder(document.uri.fsPath, root)
        );
        await Promise.all(dirty.map(document => document.save()));
    }

    private showError(message: string): void {
        (this.deps.showError ?? (m => void vscode.window.showErrorMessage(m)))(message);
    }
}
