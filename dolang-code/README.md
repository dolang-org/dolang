# Do Language Support

VS Code extension providing language support for the Do programming language.

## Features

- Syntax highlighting for `.dol` files
- Language server integration for:
    - Diagnostics
    - Code completion
    - Go to definition
- Script execution:
    - Run Do scripts with a single click
    - Interactive Do shell

## Requirements

- VS Code 1.75.0 or later
- One of:
    - `dolang`, `dolang-shell-vfs`, and `dolang-lsp` in `PATH`
    - explicit tool paths in VS Code settings
    - automatic download from GitHub release assets

## Running Scripts

### Quick Start

1. Open a `.dol` file
2. Click the play button (▶) in the editor title bar
3. Or right-click the file and select "Run Do Script"

### Interactive Shell

1. Press `Ctrl+Shift+P` and type "Open Do Interactive Shell"
2. Opens a dedicated terminal for interactive Do sessions

## Configuration

Customize execution behavior in VS Code settings:

- `dolang.keepTerminalOpen` (default: true): Keep terminal open after script
  execution
- `dolang.autoSaveBeforeRun` (default: true): Auto-save files before running
- `dolang.runInTerminal` (default: true): Execute scripts in integrated terminal
- `dolang.path`: Explicit path to `dolang`
- `dolang.vfs.path`: Explicit path to `dolang-shell-vfs`
- `dolang.lsp.path`: Explicit path to `dolang-lsp`

Tool resolution order is:

1. Explicit per-tool path setting
2. Matching binary from `PATH`
3. Downloaded GitHub release bundle

If the tools are not available locally, the extension downloads one platform
`tar.gz` bundle from the latest GitHub release for `bkoropoff/dolang` and
extracts it into extension global storage.

## Building

```bash
cd dolang-code
npm install
npm run package:vsix
```

## Installation

Install the extension from the VSIX file:

```bash
code --install-extension dolang-*.vsix
```

Or via the Extensions view: Ctrl+Shift+P → "Extensions: Install from VSIX"
