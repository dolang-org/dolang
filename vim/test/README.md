# Vim Grammar Tests

Run `dodo vim-test` from the workspace root.
The runner needs Neovim or Vim with `+syntax` and `+eval`.
Set `DOLANG_SYNTAX_VIM` to select an editor; otherwise it prefers Neovim and
falls back to Vim.

The tests check syntax groups against the shared fixtures in `test/syntax/`.
`dodo syntax-test` runs both the TextMate and Vim suites; the TextMate suite
also needs `npm --prefix dolang-code ci`.
