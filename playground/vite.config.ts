import { defineConfig } from 'vite';

export default defineConfig({
  base: './',
  build: { outDir: '../target/playground', emptyOutDir: true },
  worker: { format: 'es' },
});
