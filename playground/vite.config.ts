import { defineConfig } from 'vite';

export default defineConfig({
  base: './',
  build: { outDir: '../target/playground', emptyOutDir: true },
  // The editor reads its indentation rules from the VS Code extension.
  server: { fs: { allow: ['..'] } },
  worker: { format: 'es' },
});
