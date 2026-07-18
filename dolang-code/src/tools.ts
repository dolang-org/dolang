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

export interface ToolResolverOptions {
    readonly context: Pick<vscode.ExtensionContext, "globalStorageUri">;
    readonly configuration?: vscode.WorkspaceConfiguration;
    readonly pathEnv?: string | undefined;
}

const DOWNLOADS_ENABLED = true;
const REPOSITORY_OWNER = "bkoropoff";
const REPOSITORY_NAME = "dolang";
const BUNDLE_DIRECTORY = "bundle";

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
    const attempts: string[] = [];

    const settingKey = TOOL_METADATA[tool].settingKey;
    const configured = settingKey ? configuration.get<string>(settingKey)?.trim() : undefined;
    if (configured) {
        attempts.push(`configured path (${configured})`);
        if (await isExecutableFile(configured)) {
            return { tool, path: configured, source: "configured" };
        }
        throw new Error(
            `Configured path for ${TOOL_METADATA[tool].displayName} does not exist or is not executable: ${configured}`
        );
    }

    const pathMatch = await findOnPath(tool, options.pathEnv);
    attempts.push("PATH");
    if (pathMatch) {
        return { tool, path: pathMatch, source: "path" };
    }

    const downloader = new GitHubReleaseToolDownloader(options.context);
    attempts.push("downloaded release");
    const downloaded = await downloader.resolve(tool);
    if (downloaded) {
        return downloaded;
    }

    throw new Error(
        `Unable to find ${TOOL_METADATA[tool].displayName}. Tried ${attempts.join(", ")}.`
    );
}

export async function resolveExecutionTools(
    options: ToolResolverOptions
): Promise<{
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

export class GitHubReleaseToolDownloader {
    readonly cacheRoot: string;

    constructor(private readonly context: Pick<vscode.ExtensionContext, "globalStorageUri">) {
        this.cacheRoot = path.join(context.globalStorageUri.fsPath, "tools", "releases");
    }

    public isEnabled(): boolean {
        return DOWNLOADS_ENABLED;
    }

    public async resolve(tool: ToolName): Promise<ResolvedTool | undefined> {
        const targetPath = this.downloadedBinaryPath(tool);
        if (await isExecutableFile(targetPath)) {
            return { tool, path: targetPath, source: "downloaded" };
        }

        if (!this.isEnabled()) {
            return undefined;
        }

        await fs.mkdir(this.platformCacheDir(), { recursive: true });
        await this.downloadAndExtractLatestReleaseBundle();
        return { tool, path: targetPath, source: "downloaded" };
    }

    public downloadedBinaryPath(tool: ToolName): string {
        return path.join(this.bundleInstallDir(), binaryFileName(tool));
    }

    public releaseAssetName(): string {
        return `dolang-${platformTripleFor(process.platform, process.arch)}.tar.gz`;
    }

    public downloadedArchivePath(): string {
        return path.join(this.platformCacheDir(), this.releaseAssetName());
    }

    private platformCacheDir(): string {
        return path.join(this.cacheRoot, platformTripleFor(process.platform, process.arch));
    }

    private bundleInstallDir(): string {
        return path.join(this.platformCacheDir(), BUNDLE_DIRECTORY);
    }

    private async downloadAndExtractLatestReleaseBundle(): Promise<void> {
        const response = await fetch(
            `https://api.github.com/repos/${REPOSITORY_OWNER}/${REPOSITORY_NAME}/releases/latest`,
            {
                headers: {
                    Accept: "application/vnd.github+json"
                }
            }
        );

        if (!response.ok) {
            throw new Error(
                `Failed to query GitHub releases: ${response.status} ${response.statusText}`
            );
        }

        const release = (await response.json()) as GitHubReleaseResponse;
        const assetName = this.releaseAssetName();
        const asset = release.assets.find(candidate => candidate.name === assetName);

        if (!asset) {
            throw new Error(
                `Release ${release.tag_name} does not contain asset ${assetName}`
            );
        }

        const download = await fetch(asset.browser_download_url);
        if (!download.ok || !download.body) {
            throw new Error(
                `Failed to download ${assetName}: ${download.status} ${download.statusText}`
            );
        }

        const bytes = Buffer.from(await download.arrayBuffer());
        const archivePath = this.downloadedArchivePath();
        const extractDir = this.bundleInstallDir();

        await fs.rm(extractDir, { recursive: true, force: true });
        await fs.mkdir(extractDir, { recursive: true });
        await fs.writeFile(archivePath, bytes);
        await tar.x({
            cwd: extractDir,
            file: archivePath,
            strict: true
        });
        await this.ensureBundleExecutables();
    }

    private async ensureBundleExecutables(): Promise<void> {
        const toolNames: ToolName[] = ["shell", "vfs", "lsp"];
        const supportedToolNames = toolNames.filter(tool =>
            isToolSupportedOnPlatform(tool, process.platform)
        );
        await Promise.all(
            supportedToolNames.map(async tool => {
                const executable = this.downloadedBinaryPath(tool);
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

export function platformTripleFor(
    platformName: NodeJS.Platform,
    arch: NodeJS.Architecture
): string {
    return `${githubArch(arch)}-${githubPlatform(platformName)}`;
}

function isToolSupportedOnPlatform(tool: ToolName, platformName: NodeJS.Platform): boolean {
    return !(tool === "vfs" && platformName === "win32");
}

function githubPlatform(platformName: NodeJS.Platform): string {
    switch (platformName) {
        case "darwin":
            return "apple-darwin";
        case "linux":
            return "unknown-linux-gnu";
        case "win32":
            return "pc-windows-msvc";
        default:
            throw new Error(`Unsupported platform for release downloads: ${platformName}`);
    }
}

function githubArch(arch: NodeJS.Architecture): string {
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
