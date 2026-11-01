import { constants as fsConstants } from "fs";
import * as fs from "fs/promises";
import * as path from "path";
import * as tar from "tar";
import * as vscode from "vscode";

export type ToolName = "shell" | "vfs" | "lsp";
export type ToolSource = "configured" | "path" | "downloaded";

export interface ResolvedTool {
    readonly tool: ToolName;
    readonly path: string;
    readonly source: ToolSource;
}

/** The parts of the extension context that tool resolution needs. */
export interface ToolContext {
    readonly globalStorageUri: vscode.Uri;
    readonly extension: { readonly packageJSON: { readonly version?: unknown } };
}

/** Where tool resolution reports what it found and what it downloaded. */
export type ToolLog = Pick<vscode.LogOutputChannel, "info" | "warn" | "error">;

/** Fetches release metadata and archives; tests substitute their own. */
export type ToolFetch = (_url: string, _init?: RequestInit) => Promise<Response>;

export interface ToolResolverOptions {
    readonly context: ToolContext;
    readonly configuration?: vscode.WorkspaceConfiguration;
    readonly pathEnv?: string | undefined;
    readonly log?: ToolLog;
}

const DOWNLOADS_ENABLED = true;
const REPOSITORY_OWNER = "dolang-org";
const REPOSITORY_NAME = "dolang";
const BUNDLE_DIRECTORY = "bundle";

/**
 * Downloads keyed by install directory. A finished download stays here until
 * `clear`, so a resolve whose file check started before it finished reuses it.
 */
const downloads = new Map<string, Promise<void>>();

/** The extension's version, which names the tool release it downloads. */
export function extensionVersion(context: ToolContext): string {
    const version = context.extension.packageJSON.version;
    if (typeof version !== "string" || version === "") {
        throw new Error("The extension's package.json has no version");
    }
    return version;
}

const TOOL_METADATA: Record<
    ToolName,
    {
        binaryName: string;
        settingKey?: string;
        displayName: string;
    }
> = {
    shell: {
        binaryName: "dolang",
        settingKey: "path",
        displayName: "dolang"
    },
    vfs: {
        binaryName: "dolang-vfs",
        displayName: "dolang-vfs"
    },
    lsp: {
        binaryName: "dolang-lsp",
        settingKey: "lsp.path",
        displayName: "dolang-lsp"
    }
};

export async function resolveTool(
    tool: ToolName,
    options: ToolResolverOptions
): Promise<ResolvedTool> {
    if (!isToolSupportedOnPlatform(tool, process.platform)) {
        throw new Error(
            `${TOOL_METADATA[tool].displayName} is not supported on ${process.platform}`
        );
    }

    const configuration = options.configuration ?? vscode.workspace.getConfiguration("dolang");
    const displayName = TOOL_METADATA[tool].displayName;
    const log = options.log;
    const attempts: string[] = [];

    const settingKey = TOOL_METADATA[tool].settingKey;
    const configured = settingKey ? configuration.get<string>(settingKey)?.trim() : undefined;
    if (configured) {
        attempts.push(`configured path (${configured})`);
        if (await isExecutableFile(configured)) {
            log?.info(`Resolved ${displayName} from dolang.${settingKey}: ${configured}`);
            return { tool, path: configured, source: "configured" };
        }
        const message = `Configured path for ${displayName} does not exist or is not executable: ${configured}`;
        log?.error(message);
        throw new Error(message);
    }

    const pathMatch = await findOnPath(tool, options.pathEnv);
    attempts.push("PATH");
    if (pathMatch) {
        log?.info(`Resolved ${displayName} from PATH: ${pathMatch}`);
        return { tool, path: pathMatch, source: "path" };
    }

    const downloader = new GitHubReleaseToolDownloader(
        options.context,
        extensionVersion(options.context),
        log
    );
    attempts.push("downloaded release");
    const downloaded = await downloader.resolve(tool);
    if (downloaded) {
        log?.info(
            `Resolved ${displayName} from release v${downloader.version}: ${downloaded.path}`
        );
        return downloaded;
    }

    const message = `Unable to find ${displayName}. Tried ${attempts.join(", ")}.`;
    log?.error(message);
    throw new Error(message);
}

export async function resolveExecutionTools(options: ToolResolverOptions): Promise<{
    readonly shell: ResolvedTool;
}> {
    const shell = await resolveTool("shell", options);
    return { shell };
}

export function binaryFileName(tool: ToolName): string {
    const baseName = TOOL_METADATA[tool].binaryName;
    return process.platform === "win32" ? `${baseName}.exe` : baseName;
}

async function findOnPath(tool: ToolName, pathEnv = process.env.PATH): Promise<string | undefined> {
    if (!pathEnv) {
        return undefined;
    }

    const candidateName = binaryFileName(tool);
    for (const entry of pathEnv.split(path.delimiter)) {
        const trimmed = entry.trim();
        if (!trimmed) {
            continue;
        }

        const candidate = path.join(trimmed, candidateName);
        if (await isExecutableFile(candidate)) {
            return candidate;
        }
    }

    return undefined;
}

async function isExecutableFile(filePath: string): Promise<boolean> {
    try {
        await fs.access(filePath, fsConstants.X_OK);
        return true;
    } catch {
        return false;
    }
}

async function fileExists(filePath: string): Promise<boolean> {
    try {
        await fs.access(filePath, fsConstants.F_OK);
        return true;
    } catch {
        return false;
    }
}

/**
 * Downloads the release bundle whose tag matches the extension version, so the
 * tools always pair with the extension that fetched them.
 */
export class GitHubReleaseToolDownloader {
    readonly cacheRoot: string;
    readonly version: string;
    private readonly log: ToolLog | undefined;
    private readonly fetch: ToolFetch;

    constructor(
        context: Pick<ToolContext, "globalStorageUri">,
        version: string,
        log?: ToolLog,
        fetcher: ToolFetch = (url, init) => fetch(url, init)
    ) {
        this.cacheRoot = path.join(context.globalStorageUri.fsPath, "tools", "releases");
        this.version = version;
        this.log = log;
        this.fetch = fetcher;
    }

    public isEnabled(): boolean {
        return DOWNLOADS_ENABLED;
    }

    public async resolve(tool: ToolName): Promise<ResolvedTool | undefined> {
        const targetPath = this.downloadedBinaryPath(tool);
        const resolved: ResolvedTool = { tool, path: targetPath, source: "downloaded" };
        if (await isExecutableFile(targetPath)) {
            return resolved;
        }

        if (!this.isEnabled()) {
            return undefined;
        }

        const key = this.bundleInstallDir();
        const previous = downloads.get(key);
        if (previous) {
            await previous;
            if (await isExecutableFile(targetPath)) {
                return resolved;
            }
            // The bundle was removed after it was downloaded
            if (downloads.get(key) === previous) {
                downloads.delete(key);
            }
        }

        let download = downloads.get(key);
        if (!download) {
            const started = this.download();
            started.catch(() => {
                if (downloads.get(key) === started) {
                    downloads.delete(key);
                }
            });
            downloads.set(key, started);
            download = started;
        }
        await download;
        return resolved;
    }

    /** Deletes this version's cached bundle, so the next resolve downloads it again. */
    public async clear(): Promise<void> {
        downloads.delete(this.bundleInstallDir());
        await fs.rm(this.versionDir(), { recursive: true, force: true });
    }

    public releaseUrl(): string {
        return `https://api.github.com/repos/${REPOSITORY_OWNER}/${REPOSITORY_NAME}/releases/tags/v${this.version}`;
    }

    public downloadedBinaryPath(tool: ToolName): string {
        return path.join(this.bundleInstallDir(), binaryFileName(tool));
    }

    public releaseAssetName(): string {
        return `dolang-${this.version}-${platformKeyFor(process.platform, process.arch)}.tar.gz`;
    }

    public downloadedArchivePath(): string {
        return path.join(this.platformCacheDir(), this.releaseAssetName());
    }

    private versionDir(): string {
        return path.join(this.cacheRoot, this.version);
    }

    private platformCacheDir(): string {
        return path.join(this.versionDir(), platformKeyFor(process.platform, process.arch));
    }

    private bundleInstallDir(): string {
        return path.join(this.platformCacheDir(), BUNDLE_DIRECTORY);
    }

    private async download(): Promise<void> {
        this.log?.info(`Downloading Do tools v${this.version} (${this.releaseAssetName()})`);
        try {
            await vscode.window.withProgress(
                {
                    location: vscode.ProgressLocation.Notification,
                    title: `Downloading Do tools v${this.version}`
                },
                () => this.downloadAndExtract()
            );
        } catch (error) {
            this.log?.error(
                `Download failed: ${error instanceof Error ? error.message : String(error)}`
            );
            throw error;
        }
        this.log?.info(`Installed Do tools v${this.version} in ${this.bundleInstallDir()}`);
        await this.prune();
    }

    /** Removes every cached bundle other than this version's. */
    private async prune(): Promise<void> {
        let entries: string[];
        try {
            entries = await fs.readdir(this.cacheRoot);
        } catch {
            return;
        }
        await Promise.all(
            entries
                .filter(entry => entry !== this.version)
                .map(async entry => {
                    const stale = path.join(this.cacheRoot, entry);
                    this.log?.info(`Removing cached tools ${stale}`);
                    await fs.rm(stale, { recursive: true, force: true });
                })
        );
    }

    private async downloadAndExtract(): Promise<void> {
        const response = await this.fetch(this.releaseUrl(), {
            headers: {
                Accept: "application/vnd.github+json"
            }
        });

        if (response.status === 404) {
            throw new Error(
                `No Do release v${this.version} exists to download tools from; ` +
                    "install dolang and dolang-lsp on PATH or set dolang.path and dolang.lsp.path"
            );
        }
        if (!response.ok) {
            throw new Error(
                `Failed to query GitHub release v${this.version}: ${response.status} ${response.statusText}`
            );
        }

        const release = (await response.json()) as GitHubReleaseResponse;
        const assetName = this.releaseAssetName();
        const asset = release.assets.find(candidate => candidate.name === assetName);

        if (!asset) {
            throw new Error(`Release ${release.tag_name} does not contain asset ${assetName}`);
        }

        const download = await this.fetch(asset.browser_download_url);
        if (!download.ok || !download.body) {
            throw new Error(
                `Failed to download ${assetName}: ${download.status} ${download.statusText}`
            );
        }

        const bytes = Buffer.from(await download.arrayBuffer());
        const archivePath = this.downloadedArchivePath();
        const installDir = this.bundleInstallDir();
        // Extract beside the install directory and rename it into place, so an
        // interrupted download never leaves a partial bundle that looks usable
        const extractDir = `${installDir}.partial`;

        await fs.rm(extractDir, { recursive: true, force: true });
        await fs.mkdir(extractDir, { recursive: true });
        await fs.writeFile(archivePath, bytes);
        await tar.x({
            cwd: extractDir,
            file: archivePath,
            strict: true
        });
        await this.ensureBundleExecutables(extractDir);
        await fs.rm(installDir, { recursive: true, force: true });
        await fs.rename(extractDir, installDir);
        await fs.rm(archivePath, { force: true });
    }

    private async ensureBundleExecutables(dir: string): Promise<void> {
        const toolNames: ToolName[] = ["shell", "vfs", "lsp"];
        const supportedToolNames = toolNames.filter(tool =>
            isToolSupportedOnPlatform(tool, process.platform)
        );
        await Promise.all(
            supportedToolNames.map(async tool => {
                const executable = path.join(dir, binaryFileName(tool));
                if (!(await fileExists(executable))) {
                    throw new Error(
                        `Downloaded tool bundle is missing ${TOOL_METADATA[tool].displayName}`
                    );
                }

                if (process.platform !== "win32") {
                    await fs.chmod(executable, 0o755);
                }
            })
        );
    }
}

export function platformKeyFor(platformName: NodeJS.Platform, arch: NodeJS.Architecture): string {
    return `${dolangArch(arch)}-${dolangOs(platformName)}`;
}

function isToolSupportedOnPlatform(tool: ToolName, platformName: NodeJS.Platform): boolean {
    return !(tool === "vfs" && platformName === "win32");
}

function dolangOs(platformName: NodeJS.Platform): string {
    switch (platformName) {
        case "darwin":
            return "macos";
        case "linux":
            return "linux";
        case "win32":
            return "windows";
        default:
            throw new Error(`Unsupported platform for release downloads: ${platformName}`);
    }
}

function dolangArch(arch: NodeJS.Architecture): string {
    switch (arch) {
        case "arm64":
            return "aarch64";
        case "x64":
            return "x86_64";
        default:
            throw new Error(`Unsupported architecture for release downloads: ${arch}`);
    }
}

interface GitHubReleaseResponse {
    readonly tag_name: string;
    readonly assets: readonly GitHubReleaseAsset[];
}

interface GitHubReleaseAsset {
    readonly name: string;
    readonly browser_download_url: string;
}
