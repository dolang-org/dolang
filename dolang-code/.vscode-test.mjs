import { defineConfig } from "@vscode/test-cli";
import { version } from "./test/vscode-version.mjs";

export default defineConfig({
    version,
    files: "out/test/**/*.test.js",
    launchArgs: ["--disable-gpu"]
});
