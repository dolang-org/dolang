# Do Language Support

VS Code extension providing language support for the Do programming language.

## Features

- Syntax highlighting for `.dol` files
- Indentation-aware editing, offside folding and bracket matching
- Language server integration for:
    - Diagnostics
    - Code completion
    - Go to definition
- Script execution:
    - Run Do scripts with a single click, with or without arguments
    - `dolang` tasks in `tasks.json` for saved argument sets
    - Interactive Do shell, with Send Selection/Line to Do Shell

## Requirements

- VS Code 1.75.0 or later
- One of:
    - `dolang` and `dolang-lsp` in `PATH`; on Unix, `dolang-vfs` must be
      installed beside `dolang`
    - explicit tool paths in VS Code settings
    - automatic download from GitHub release assets

## Running Scripts

### Quick Start

1. Open a `.dol` file
2. Click the play button (▶) in the editor title bar
3. Or right-click the file and select "Run Do Script"

Scripts run as VS Code tasks that start `dolang` directly, with no shell in
between, so paths and arguments are passed through verbatim. A modified file is
saved first. An untitled buffer is written to a temporary file in the
extension's storage and run from the first workspace folder.

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

"Send Selection/Line to Do Shell" sends the selection, or the current line, to
the most recently opened interactive shell, opening one if needed. A
multi-line selection is dedented and runs as a single REPL turn. With no
selection, the cursor moves to the next line.

## Configuration

Customize execution behavior in VS Code settings:

- `dolang.keepTerminalOpen` (default: true): Keep the script's terminal open
  after it exits, until a key is pressed
- `dolang.autoSaveBeforeRun` (default: true): Save a modified script before
  running it
- `dolang.path`: Explicit path to `dolang`
- `dolang.lsp.path`: Explicit path to `dolang-lsp`

Tool resolution order is:

1. Explicit per-tool path setting
2. Matching binary from `PATH`
3. Downloaded GitHub release bundle

If the tools are not available locally, the extension downloads one platform
`tar.gz` bundle from the latest GitHub release for `dolang-org/dolang` and
extracts it into extension global storage.

## Building

```bash
cd dolang-code
npm install
npm run package:vsix
```

### Grammar Tests

Run `npm run test:syntax` in this directory after `npm ci`. The tests tokenize
fixtures directly with TextMate, without the language server or an editor.
Shared grammar fixtures live in `test/syntax/` at the workspace root.

## Installation

Install the extension from the VSIX file:

```bash
code --install-extension dolang-*.vsix
```

Or via the Extensions view: Ctrl+Shift+P → "Extensions: Install from VSIX"
