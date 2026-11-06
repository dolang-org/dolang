# VS Code

The `dolang-code` extension adds Do support to VS Code, using the
[language server](./lsp.md) for most of its editing features.

## Features

- Syntax highlighting for `.dol` files
- Indentation-aware editing, offside folding and bracket matching
- Language server integration for:
    - Diagnostics, with quick fixes
    - Hover documentation
    - Go to definition and document highlights
    - Document outline
    - Semantic highlighting
- Extensionless scripts are recognized by a `dolang` shebang line, such as
  `#!/usr/bin/env dolang`
- Script execution:
    - Run Do scripts with a single click, with or without arguments
    - `dolang` tasks in `tasks.json` for saved argument sets
    - Interactive Do shell, with Send Selection/Line to Do Shell
- A [project](./index.md)'s tests in the Test Explorer, run with
  [`doze test`](./doze.md#testing)
- Do: Build Project and Do: Check Project, run with `doze`, with the check's
  results in the Problems panel

## Requirements

- VS Code 1.75.0 or later
- One of:
    - `dolang` and `dolang-lsp` in `PATH`; on Unix, `dolang-vfs` must be
      installed beside `dolang`
    - explicit tool paths in VS Code settings
    - automatic download from GitHub release assets

## Installation

Install the extension from the VSIX file:

```bash
code --install-extension dolang-*.vsix
```

Or via the Extensions view: Ctrl+Shift+P → "Extensions: Install from VSIX"

## Running Scripts

### Quick Start

1. Open a `.dol` file
2. Click the play button (▶) in the editor title bar
3. Or right-click the file and select "Run Do Script"

Scripts run as VS Code tasks that start `dolang` directly, with no shell in
between, so paths and arguments are passed through verbatim. A modified file is
saved first. An untitled buffer is written to a temporary file in the
extension's storage and run from the first workspace folder.

A script runs with its project's module path, which the language server reports:
`lib/` by convention, or the `modules` table of a
[`.dolang-lsp.toml`](./lsp-config.md#module-path) at the project's root. An
untitled buffer gets the module path of the first workspace folder's project.
Without a running language server, a script runs with no module path.

### Arguments

"Run Do Script with Arguments…" prompts for arguments and remembers the last
ones for each file. Separate arguments with spaces; group them with `'…'`
(literal) or `"…"` (where `\"` and `\\` are escapes), or escape a single
character with `\`.

To save argument sets, add `dolang` tasks to `.vscode/tasks.json`. Relative
`script` and `cwd` paths resolve against the workspace folder, and `cwd`
defaults to the script's directory:

```json
{
    "version": "2.0.0",
    "tasks": [
        {
            "type": "dolang",
            "label": "deploy staging",
            "script": "scripts/deploy.dol",
            "args": ["--env", "staging"]
        }
    ]
}
```

### Interactive Shell

1. Press `Ctrl+Shift+P` and type "Open Do Interactive Shell"
2. Opens a dedicated terminal for interactive Do sessions

A new shell gets the module path of the active document's project, else of the
first workspace folder's, as a script does.

"Send Selection/Line to Do Shell" sends the selection, or the current line, to
the most recently opened interactive shell, opening one if needed. A
multi-line selection is dedented and runs as a single REPL turn. With no
selection, the cursor moves to the next line.

## Testing

The Test Explorer shows the tests of each project in the workspace: the `.dol`
files under its `test/` directory. The language server reports each project's
root, so tests appear only while it runs. Each file lists its tests, and a
parameterized test groups its cases.

The extension runs `doze test` in the project, so tests run the same way as
from a terminal: `doze test --list` to list them, and `doze test --format jsonl`
to run all of them, a file or a single test, with `--id` for each test chosen.
A failure shows the harness's message at the failing line, with a diff when an
assertion compared two values. Modified Do files in the project are saved
before a run when `dolang.autoSaveBeforeRun` is on. Editing a file under
`test/` lists its project again.

Listing loads the test modules, which runs their top-level code, so tests are
listed only in a trusted workspace.

## Projects

**Do: Build Project** runs `doze bundle`, and **Do: Check Project** runs
`doze check`, on the project holding the active document, or the file or
folder chosen in the Explorer. The language server reports the project's root,
which is passed to `doze` as `--project` and used as its working directory, so
both commands need the server to be running. Modified Do files in the project
are saved first when `dolang.autoSaveBeforeRun` is on.

`doze` tasks run the same commands from `tasks.json`, with extra arguments, on
the project of their workspace folder:

```json
{
    "type": "doze",
    "command": "bundle",
    "args": ["--shebang", "/usr/bin/env dolang"]
}
```

Without a `tasks.json`, Run Build Task offers `doze bundle` and Run Test Task
offers `doze check`, for each workspace folder.

The check's results appear in the Problems panel, read from the
[report](./doze.md#reports) `doze --json` writes. A run replaces the project's
earlier results. Files open in an editor are left to the language server: their
results from `doze` are dropped when they are opened or edited, and later runs
skip them.

## Configuration

Customize execution behavior in VS Code settings:

- `dolang.keepTerminalOpen` (default: true): Keep the script's terminal open
  after it exits, until a key is pressed
- `dolang.autoSaveBeforeRun` (default: true): Save a modified script before
  running it
- `dolang.path`: Explicit path to `dolang`
- `dolang.lsp.path`: Explicit path to `dolang-lsp`; changing it restarts the
  language server
- `dolang.trace.server` (default: `off`): Trace language server messages
  (`messages` or `verbose`) in the "Do Language Server" output channel

Tool resolution order is:

1. Explicit per-tool path setting
2. Matching binary from `PATH`
3. Downloaded GitHub release bundle

If the tools are not available locally, the extension downloads the platform
`tar.gz` bundle from the `dolang-org/dolang` GitHub release whose tag matches
the extension's version (for example, `v0.1.1` for extension 0.1.1), so the
tools always match the extension. The bundle is extracted into extension
global storage, and bundles for other versions are removed. "Re-download Do
Tools" replaces the cached bundle; tools from a configured path or `PATH`
still take precedence over it.

"Restart Do Language Server" restarts the server, for example after
rebuilding a local `dolang-lsp`. A server whose version differs from the
extension's is reported as a warning.

### Restricted Mode

In an untrusted workspace, the extension provides syntax highlighting and
editing support only: the language server doesn't start, and scripts, tasks,
tests, project builds and the interactive shell can't be run. Workspace settings
can't change `dolang.path` or `dolang.lsp.path`. Trusting the workspace starts
the language server. Virtual workspaces aren't supported.

### Troubleshooting

The "Do" output channel logs which tool was used and where it came from
(a setting, `PATH` or a downloaded release), along with download activity.
The language server's own output appears in the "Do Language Server" output
channel.
