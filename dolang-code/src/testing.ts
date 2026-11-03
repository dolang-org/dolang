import { spawn } from "child_process";
import * as fs from "fs/promises";
import * as path from "path";
import * as readline from "readline";
import * as vscode from "vscode";

/** How a `doze` process ended. */
export interface DozeResult {
    /** The exit status, or `null` when the process didn't start or was killed */
    readonly code: number | null;
    readonly stderr: string;
}

/** A running `doze` process: its standard output by line, and its end. */
export interface DozeProcess {
    readonly lines: AsyncIterable<string>;
    readonly done: Promise<DozeResult>;
}

/** Starts `dolang` with `args` in `cwd`; tests substitute their own. */
export type DozeSpawner = (
    _dolang: string,
    _args: readonly string[],
    _cwd: string,
    _token?: vscode.CancellationToken
) => DozeProcess;

/** Runs `dolang` without a shell, killing it when `token` is cancelled. */
export const spawnDoze: DozeSpawner = (dolang, args, cwd, token) => {
    const child = spawn(dolang, [...args], {
        cwd,
        stdio: ["ignore", "pipe", "pipe"],
        windowsHide: true
    });
    let stderr = "";
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (data: string) => {
        stderr += data;
    });
    const cancel = token?.onCancellationRequested(() => child.kill());
    const done = new Promise<DozeResult>(resolve => {
        child.on("error", error => {
            stderr += error.message;
            resolve({ code: null, stderr });
        });
        child.on("close", code => resolve({ code, stderr }));
    }).finally(() => cancel?.dispose());
    const lines = readline.createInterface({ input: child.stdout, crlfDelay: Infinity });
    return { lines, done };
};

/** The `dolang` arguments that run `doze test` on the project at `root`. */
export function dozeTestArgs(root: string, args: readonly string[]): string[] {
    return ["-m", "doze", "--project", root, "test", ...args];
}

/**
 * Converts a column counted in characters (code points) on `line` to one
 * counted in UTF-16 code units, as VS Code counts them. A column past the end
 * of the line is the line's end.
 */
export function charToUtf16Column(line: string, column: number): number {
    let units = 0;
    let chars = 0;
    for (const c of line) {
        if (chars >= column) {
            break;
        }
        units += c.length;
        chars++;
    }
    return units;
}

/** A location as the harness reports it: zero-based, in characters. */
interface HarnessLocation {
    readonly file: string;
    readonly line: number;
    readonly column: number;
    readonly len: number;
}

interface HarnessTest {
    readonly id: string;
    readonly module: string;
    readonly name: string;
    readonly base: string;
    readonly params: readonly unknown[];
    readonly tags: readonly string[];
    readonly location: HarnessLocation | null;
}

interface HarnessDiagnostic {
    readonly severity: string;
    readonly message: string;
    readonly location: HarnessLocation;
}

interface HarnessModule {
    readonly path: string;
    readonly module: string;
    readonly status: "loaded" | "failed" | "skipped" | "ignored";
    readonly tests?: readonly HarnessTest[];
    readonly message?: string | null;
    readonly error?: {
        readonly kind: string;
        readonly message: string;
        readonly diagnostics?: readonly HarnessDiagnostic[];
        readonly rendered?: string;
        readonly output?: string;
    };
}

interface HarnessEvent {
    readonly event: string;
    readonly id?: string;
    readonly outcome?: string;
    readonly duration?: number;
    readonly message?: string | null;
    readonly output?: string;
    readonly failure_location?: HarnessLocation | null;
    readonly error?: {
        readonly rendered?: string;
        readonly expected?: unknown;
        readonly actual?: unknown;
    };
}

/** What the test controller needs from the rest of the extension. */
export interface DoTestDependencies {
    /** The root of the project holding `uri`, or `undefined` when unknown */
    projectRoot(_uri: vscode.Uri): Promise<string | undefined>;
    /** The `dolang` executable */
    dolang(): Promise<string>;
    readonly log: Pick<vscode.LogOutputChannel, "info" | "warn" | "error">;
    readonly spawn?: DozeSpawner;
}

/** The test run operations the controller uses; tests record them. */
export type DoTestRun = Pick<
    vscode.TestRun,
    "enqueued" | "started" | "passed" | "failed" | "skipped" | "errored" | "appendOutput" | "end"
>;

type ItemData =
    | { readonly kind: "project"; readonly root: string }
    | { readonly kind: "file"; readonly root: string; readonly module: string }
    | { readonly kind: "group"; readonly root: string }
    | { readonly kind: "test"; readonly root: string; readonly id: string };

class Project {
    /** Bumped by each listing, so a stale one is dropped */
    public generation = 0;
    public readonly files = new Map<string, vscode.TestItem>();
    public readonly tests = new Map<string, vscode.TestItem>();

    public readonly root: string;
    /** The project's own item, when the workspace has several projects */
    public readonly item: vscode.TestItem | undefined;

    constructor(root: string, item: vscode.TestItem | undefined) {
        this.root = root;
        this.item = item;
    }

    public get testDir(): string {
        return path.join(this.root, "test");
    }
}

/** Reads source lines for converting harness locations, once per file. */
class SourceLines {
    private readonly files = new Map<string, Promise<string[] | undefined>>();

    public async location(
        root: string,
        location: HarnessLocation
    ): Promise<vscode.Location | undefined> {
        const file = path.resolve(root, location.file);
        let lines = this.files.get(file);
        if (!lines) {
            lines = fs.readFile(file, "utf8").then(
                text => text.split(/\r?\n/),
                () => undefined
            );
            this.files.set(file, lines);
        }
        const text = (await lines)?.[location.line];
        const start = text === undefined ? 0 : charToUtf16Column(text, location.column);
        const end =
            text === undefined ? 0 : charToUtf16Column(text, location.column + location.len);
        return new vscode.Location(
            vscode.Uri.file(file),
            new vscode.Range(location.line, start, location.line, end)
        );
    }
}

function isUnder(file: string, dir: string): boolean {
    const relative = path.relative(dir, file);
    return relative !== "" && !relative.startsWith("..") && !path.isAbsolute(relative);
}

function crlf(text: string): string {
    return text.replace(/\r?\n/g, "\r\n");
}

function display(value: unknown): string {
    return typeof value === "string" ? value : JSON.stringify(value, null, 2);
}

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}

/** The tests of each Do project in the workspace, run with `doze test`. */
export class DoTestController implements vscode.Disposable {
    public readonly controller: vscode.TestController;

    private readonly deps: DoTestDependencies;
    private readonly spawn: DozeSpawner;
    private readonly data = new WeakMap<vscode.TestItem, ItemData>();
    private projects = new Map<string, Project>();
    private readonly disposables: vscode.Disposable[] = [];
    /** Whether the Test Explorer has asked for the tests */
    private wanted = false;
    private pending = new Set<string>();
    private rediscover = false;
    private timer: ReturnType<typeof setTimeout> | undefined;

    constructor(deps: DoTestDependencies) {
        this.deps = deps;
        this.spawn = deps.spawn ?? spawnDoze;
        this.controller = vscode.tests.createTestController("dolang", "Do");
        this.controller.resolveHandler = async item => {
            if (!item) {
                this.wanted = true;
                await this.discover();
            }
        };
        this.controller.refreshHandler = async () => {
            this.wanted = true;
            await this.discover();
        };
        this.controller.createRunProfile(
            "Run",
            vscode.TestRunProfileKind.Run,
            (request, token) => this.run(request, token),
            true
        );

        const watcher = vscode.workspace.createFileSystemWatcher("**/test/**/*.dol");
        const changed = (uri: vscode.Uri) => this.changed(uri);
        this.disposables.push(
            this.controller,
            watcher,
            watcher.onDidCreate(changed),
            watcher.onDidChange(changed),
            watcher.onDidDelete(changed),
            vscode.workspace.onDidGrantWorkspaceTrust(() => {
                if (this.wanted) {
                    void this.discover();
                }
            })
        );
    }

    public dispose(): void {
        clearTimeout(this.timer);
        for (const disposable of this.disposables) {
            disposable.dispose();
        }
    }

    /**
     * Finds the projects with tests in the workspace and lists them. Listing
     * loads the test modules, so an untrusted workspace has none.
     */
    public async discover(): Promise<void> {
        if (!vscode.workspace.isTrusted) {
            this.deps.log.info("Not listing Do tests in an untrusted workspace");
            return;
        }
        const roots = new Set<string>();
        for (const file of await vscode.workspace.findFiles("**/test/**/*.dol")) {
            if ([...roots].some(root => isUnder(file.fsPath, path.join(root, "test")))) {
                continue;
            }
            const root = await this.deps.projectRoot(file);
            if (root !== undefined && isUnder(file.fsPath, path.join(root, "test"))) {
                roots.add(root);
            }
        }
        await this.load([...roots]);
    }

    /** Replaces the tests with those of the projects at `roots`. */
    public async load(roots: readonly string[]): Promise<void> {
        const sorted = [...new Set(roots)].sort();
        const several = sorted.length > 1;
        this.projects = new Map(
            sorted.map(root => {
                const item = several ? this.projectItem(root) : undefined;
                return [root, new Project(root, item)];
            })
        );
        this.controller.items.replace(
            several ? [...this.projects.values()].map(project => project.item!) : []
        );
        await Promise.all([...this.projects.values()].map(project => this.list(project)));
    }

    /** Runs the tests `request` selects, project by project. */
    public async run(
        request: vscode.TestRunRequest,
        token: vscode.CancellationToken,
        run: DoTestRun = this.controller.createTestRun(request)
    ): Promise<void> {
        try {
            const selection = this.select(request);
            if (selection.size > 0) {
                await this.saveDocuments([...selection.keys()]);
            }
            for (const [project, leaves] of selection) {
                if (token.isCancellationRequested) {
                    break;
                }
                await this.runProject(project, leaves, run, token);
            }
        } catch (error) {
            run.appendOutput(crlf(`Failed to run Do tests: ${errorMessage(error)}\n`));
        } finally {
            run.end();
        }
    }

    private projectItem(root: string): vscode.TestItem {
        const uri = vscode.Uri.file(root);
        let label = vscode.workspace.asRelativePath(uri, true);
        if (label === uri.fsPath || label === "") {
            label = path.basename(root);
        }
        const item = this.controller.createTestItem(root, label, uri);
        this.data.set(item, { kind: "project", root });
        return item;
    }

    private container(project: Project): vscode.TestItemCollection {
        return project.item?.children ?? this.controller.items;
    }

    /** Lists `project`'s tests with `doze test --list` into its items. */
    private async list(project: Project): Promise<void> {
        const generation = ++project.generation;
        let result: DozeResult;
        let text = "";
        try {
            const dolang = await this.deps.dolang();
            const child = this.spawn(
                dolang,
                dozeTestArgs(project.root, ["--list", "--format", "json"]),
                project.root
            );
            for await (const line of child.lines) {
                text += line + "\n";
            }
            result = await child.done;
        } catch (error) {
            result = { code: null, stderr: errorMessage(error) };
        }
        if (generation !== project.generation || this.projects.get(project.root) !== project) {
            return;
        }

        let modules: readonly HarnessModule[] | undefined;
        try {
            modules = (JSON.parse(text) as { modules?: HarnessModule[] }).modules;
        } catch {}
        project.files.clear();
        project.tests.clear();
        if (!Array.isArray(modules)) {
            const message =
                result.stderr.trim() || `doze exited with status ${result.code ?? "unknown"}`;
            this.deps.log.warn(`Failed to list the tests of ${project.root}: ${message}`);
            let item = project.item;
            if (!item) {
                item = this.projectItem(project.root);
                this.controller.items.replace([item]);
            } else {
                item.children.replace([]);
            }
            item.error = message;
            return;
        }
        if (project.item) {
            project.item.error = undefined;
        }

        const lines = new SourceLines();
        const files: vscode.TestItem[] = [];
        for (const [index, module] of modules.entries()) {
            const file = await this.fileItem(project, module, lines);
            file.sortText = String(index).padStart(6, "0");
            files.push(file);
        }
        this.container(project).replace(files);
    }

    /** Builds the item of a listed module, with its tests. */
    private async fileItem(
        project: Project,
        module: HarnessModule,
        lines: SourceLines
    ): Promise<vscode.TestItem> {
        const uri = vscode.Uri.file(path.resolve(project.root, module.path));
        const item = this.controller.createTestItem(
            `${project.root}::file:${module.path}`,
            module.path,
            uri
        );
        this.data.set(item, { kind: "file", root: project.root, module: module.module });
        project.files.set(module.module, item);
        if (module.status === "failed") {
            item.error = this.moduleError(module);
        }
        for (const test of module.tests ?? []) {
            await this.addTest(project, item, test, lines);
        }
        return item;
    }

    private moduleError(module: HarnessModule): string {
        const error = module.error;
        if (!error) {
            return `${module.path} failed to load`;
        }
        const details = (error.diagnostics ?? []).map(
            d =>
                `${d.location.file}:${d.location.line + 1}:${d.location.column + 1}: ` +
                `${d.severity}: ${d.message}`
        );
        return [error.message, ...details].join("\n");
    }

    /** Adds `test` under its file item, nesting parameterized tests under their base. */
    private async addTest(
        project: Project,
        file: vscode.TestItem,
        test: HarnessTest,
        lines: SourceLines
    ): Promise<vscode.TestItem> {
        const location = test.location
            ? await lines.location(project.root, test.location)
            : undefined;
        let parent = file;
        let label = test.name;
        if (test.params.length > 0) {
            const groupId = `${project.root}::group:${test.module}/${test.base}`;
            let group = file.children.get(groupId);
            if (!group) {
                group = this.controller.createTestItem(groupId, test.base, location?.uri);
                group.range = location?.range;
                group.sortText = String(file.children.size).padStart(6, "0");
                this.data.set(group, { kind: "group", root: project.root });
                file.children.add(group);
            }
            parent = group;
            if (label.startsWith(test.base + "/")) {
                label = label.slice(test.base.length + 1);
            }
        }
        const item = this.controller.createTestItem(
            `${project.root}::${test.id}`,
            label,
            location?.uri
        );
        item.range = location?.range;
        item.tags = test.tags.map(tag => new vscode.TestTag(tag));
        item.sortText = String(parent.children.size).padStart(6, "0");
        this.data.set(item, { kind: "test", root: project.root, id: test.id });
        parent.children.add(item);
        project.tests.set(test.id, item);
        return item;
    }

    /** Re-lists the project holding a changed test file, after a pause. */
    private changed(uri: vscode.Uri): void {
        if (!this.wanted) {
            return;
        }
        const project = [...this.projects.values()].find(p => isUnder(uri.fsPath, p.testDir));
        if (project) {
            this.pending.add(project.root);
        } else {
            this.rediscover = true;
        }
        clearTimeout(this.timer);
        this.timer = setTimeout(() => {
            const roots = this.pending;
            const rediscover = this.rediscover;
            this.pending = new Set();
            this.rediscover = false;
            if (rediscover) {
                void this.discover();
                return;
            }
            for (const root of roots) {
                const project = this.projects.get(root);
                if (project) {
                    void this.list(project);
                }
            }
        }, 300);
    }

    /**
     * Groups the leaves `request` selects by project. A project maps to
     * `undefined` when all its tests are selected, so no `--id` is passed.
     */
    private select(request: vscode.TestRunRequest): Map<Project, vscode.TestItem[] | undefined> {
        const selection = new Map<Project, vscode.TestItem[] | undefined>();
        const excluded = new Set(request.exclude ?? []);
        const isExcluded = (item: vscode.TestItem): boolean => {
            for (let i: vscode.TestItem | undefined = item; i; i = i.parent) {
                if (excluded.has(i)) {
                    return true;
                }
            }
            return false;
        };
        const whole = (project: Project) =>
            excluded.size === 0 ? undefined : this.leaves(project.tests.values(), isExcluded);

        if (!request.include) {
            for (const project of this.projects.values()) {
                selection.set(project, whole(project));
            }
            return selection;
        }
        for (const item of request.include) {
            const data = this.data.get(item);
            const project = data && this.projects.get(data.root);
            if (!project) {
                continue;
            }
            if (data.kind === "project") {
                selection.set(project, whole(project));
                continue;
            }
            if (selection.has(project) && selection.get(project) === undefined) {
                continue;
            }
            const leaves = selection.get(project) ?? [];
            for (const leaf of this.leaves(this.descendants(item), isExcluded)) {
                if (!leaves.includes(leaf)) {
                    leaves.push(leaf);
                }
            }
            selection.set(project, leaves);
        }
        return selection;
    }

    private *descendants(item: vscode.TestItem): Iterable<vscode.TestItem> {
        yield item;
        for (const [, child] of item.children) {
            yield* this.descendants(child);
        }
    }

    private leaves(
        items: Iterable<vscode.TestItem>,
        isExcluded: (_item: vscode.TestItem) => boolean
    ): vscode.TestItem[] {
        return [...items].filter(item => this.data.get(item)?.kind === "test" && !isExcluded(item));
    }

    /** Saves modified Do documents in the projects, when the settings ask for it. */
    private async saveDocuments(projects: readonly Project[]): Promise<void> {
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
                projects.some(project => isUnder(document.uri.fsPath, project.root))
        );
        await Promise.all(dirty.map(document => document.save()));
    }

    /** Runs `leaves` of `project`, or all its tests, applying the harness's events. */
    private async runProject(
        project: Project,
        leaves: readonly vscode.TestItem[] | undefined,
        run: DoTestRun,
        token: vscode.CancellationToken
    ): Promise<void> {
        const selected = leaves ?? [...project.tests.values()];
        if (leaves && leaves.length === 0) {
            return;
        }
        const unfinished = new Set(selected);
        for (const item of selected) {
            run.enqueued(item);
        }
        const args = ["--format", "jsonl"];
        for (const item of leaves ?? []) {
            const data = this.data.get(item);
            if (data?.kind === "test") {
                args.push("--id", data.id);
            }
        }

        const dolang = await this.deps.dolang();
        const child = this.spawn(dolang, dozeTestArgs(project.root, args), project.root, token);
        const lines = new SourceLines();
        let ended = false;
        for await (const line of child.lines) {
            let event: HarnessEvent;
            try {
                event = JSON.parse(line) as HarnessEvent;
            } catch {
                run.appendOutput(crlf(line + "\n"));
                continue;
            }
            if (event.event === "run-end") {
                ended = true;
            }
            await this.apply(project, event, run, unfinished, lines);
        }
        const result = await child.done;
        if (token.isCancellationRequested) {
            return;
        }
        if (result.stderr !== "") {
            run.appendOutput(crlf(result.stderr));
        }
        if (!ended || unfinished.size > 0) {
            const message =
                result.stderr.trim() ||
                `doze exited with status ${result.code ?? "unknown"} before the tests finished`;
            for (const item of unfinished) {
                run.errored(item, new vscode.TestMessage(message));
            }
        }
    }

    private async apply(
        project: Project,
        event: HarnessEvent,
        run: DoTestRun,
        unfinished: Set<vscode.TestItem>,
        lines: SourceLines
    ): Promise<void> {
        switch (event.event) {
            case "module-end": {
                const module = event as unknown as HarnessModule;
                let file = project.files.get(module.module);
                if (!file) {
                    file = await this.fileItem(project, module, lines);
                    this.container(project).add(file);
                } else {
                    for (const test of module.tests ?? []) {
                        if (!project.tests.has(test.id)) {
                            await this.addTest(project, file, test, lines);
                        }
                    }
                }
                if (module.status === "loaded") {
                    return;
                }
                const items = [...unfinished].filter(item => this.isIn(item, file));
                if (module.status === "failed") {
                    const message = new vscode.TestMessage(this.moduleError(module));
                    if (module.error?.rendered) {
                        run.appendOutput(crlf(module.error.rendered + "\n"), undefined, file);
                    }
                    if (module.error?.output) {
                        run.appendOutput(crlf(module.error.output), undefined, file);
                    }
                    for (const item of items.length > 0 ? items : [file]) {
                        run.errored(item, message);
                    }
                } else {
                    for (const item of items) {
                        run.skipped(item);
                    }
                }
                for (const item of items) {
                    unfinished.delete(item);
                }
                return;
            }
            case "test-start": {
                const item = event.id === undefined ? undefined : project.tests.get(event.id);
                if (item) {
                    run.started(item);
                }
                return;
            }
            case "test-end":
                await this.testEnded(project, event, run, unfinished, lines);
                return;
        }
    }

    private async testEnded(
        project: Project,
        event: HarnessEvent,
        run: DoTestRun,
        unfinished: Set<vscode.TestItem>,
        lines: SourceLines
    ): Promise<void> {
        const item = event.id === undefined ? undefined : project.tests.get(event.id);
        if (!item) {
            return;
        }
        unfinished.delete(item);
        const duration = event.duration === undefined ? undefined : event.duration * 1000;
        const failure = event.failure_location
            ? await lines.location(project.root, event.failure_location)
            : undefined;
        const location =
            failure ??
            (item.uri && item.range ? new vscode.Location(item.uri, item.range) : undefined);
        if (event.output) {
            run.appendOutput(crlf(event.output), location, item);
        }
        switch (event.outcome) {
            case "passed":
                run.passed(item, duration);
                return;
            case "skipped":
            case "ignored":
                run.skipped(item);
                return;
        }
        const text = event.message ?? "failed";
        const error = event.error;
        const message =
            error && error.expected !== undefined && error.actual !== undefined
                ? vscode.TestMessage.diff(text, display(error.expected), display(error.actual))
                : new vscode.TestMessage(text);
        message.location = location;
        if (error?.rendered) {
            run.appendOutput(crlf(error.rendered + "\n"), location, item);
        }
        run.failed(item, message, duration);
    }

    private isIn(item: vscode.TestItem, ancestor: vscode.TestItem): boolean {
        for (let i: vscode.TestItem | undefined = item; i; i = i.parent) {
            if (i === ancestor) {
                return true;
            }
        }
        return false;
    }
}
