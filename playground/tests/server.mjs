// Serve the identical production assets at root and a Pages-style nested path.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname, sep } from 'node:path';
const root = resolve(import.meta.dirname, '../../target/playground');
const types = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm', '.css': 'text/css', '.png': 'image/png' };
createServer(async (request, response) => {
  try {
    let path = new URL(request.url, 'http://localhost').pathname.replace(/^\/repo\/playground\//, '/');
    if (path.endsWith('/')) path += 'index.html';
    const file = resolve(root, '.' + decodeURIComponent(path));
    if (!file.startsWith(root + sep)) throw new Error('Invalid path');
    const content = await readFile(file);
    response.writeHead(200, { 'Content-Type': types[extname(file)] ?? 'application/octet-stream' });
    response.end(content);
  } catch {
    response.writeHead(404);
    response.end('Not found');
  }
}).listen(4173, '127.0.0.1');
