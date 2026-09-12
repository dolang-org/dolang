// Serve the identical production assets at root and a Pages-style nested path,
// plus same-origin endpoints for the http extension tests.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname, sep } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
const root = resolve(import.meta.dirname, '../../target/playground');
const types = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm', '.css': 'text/css', '.png': 'image/png' };

// Writes chunks with pauses so clients see them arrive separately.
async function writeChunks(response, chunks) {
  for (const chunk of chunks) {
    response.write(chunk);
    await delay(10);
  }
  response.end();
}

const api = {
  'GET /api/json': (_request, response) => {
    response.writeHead(200, { 'Content-Type': 'application/json' });
    response.end(JSON.stringify({ name: 'Do', tags: ['http', 'wasm'] }));
  },
  // Echoes the raw body, reporting the request's content type in a header.
  'POST /api/echo': async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    response.writeHead(200, {
      'Content-Type': 'application/octet-stream',
      'X-Request-Content-Type': request.headers['content-type'] ?? '',
    });
    response.end(Buffer.concat(chunks));
  },
  'GET /api/lines': async (_request, response) => {
    response.writeHead(200, { 'Content-Type': 'text/plain' });
    await writeChunks(response, ['one\ntw', 'o\r\nthree\n', 'four']);
  },
  'GET /api/events': async (_request, response) => {
    response.writeHead(200, { 'Content-Type': 'text/event-stream' });
    await writeChunks(response, ['event: greeting\ndata: hello\n\n', 'data: line one\ndata: line two\n\n']);
  },
  'GET /api/missing': (_request, response) => {
    response.writeHead(404, { 'Content-Type': 'text/plain' });
    response.end('no such thing');
  },
};

createServer(async (request, response) => {
  try {
    const url = new URL(request.url, 'http://localhost');
    const endpoint = api[`${request.method} ${url.pathname}`];
    if (endpoint) {
      await endpoint(request, response);
      return;
    }
    let path = url.pathname.replace(/^\/repo\/playground\//, '/');
    if (path.endsWith('/')) path += 'index.html';
    const file = resolve(root, '.' + decodeURIComponent(path));
    if (!file.startsWith(root + sep)) throw new Error('Invalid path');
    const content = await readFile(file);
    response.writeHead(200, { 'Content-Type': types[extname(file)] ?? 'application/octet-stream' });
    response.end(content);
  } catch {
    if (!response.headersSent) response.writeHead(404);
    response.end('Not found');
  }
}).listen(4173, '127.0.0.1');
