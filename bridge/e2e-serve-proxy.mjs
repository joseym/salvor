#!/usr/bin/env node
// A tiny zero-dependency static+proxy server for the Bridge e2e suite.
//
//   node e2e-serve-proxy.mjs <appPort> <apiHost> <apiPort> <distDir>
//
// Serves the built Angular app from <distDir> and reverse-proxies /v1/* (and any
// SSE stream under it) to the salvor control plane at <apiHost>:<apiPort>. Because
// the app and the API answer on ONE origin, the browser makes same-origin fetches
// and the server needs no CORS (salvor serve ships none). Unknown non-asset paths
// fall back to index.html so path-style deep links (/inspector/<id>) cold-load.

import { createServer, request as httpRequest } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';

const [, , appPortArg, apiHost, apiPortArg, distDir] = process.argv;
const APP_PORT = Number(appPortArg || 4300);
const API_HOST = apiHost || '127.0.0.1';
const API_PORT = Number(apiPortArg || 8080);
const DIST = distDir || join(process.cwd(), 'dist', 'bridge', 'browser');

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.ico': 'image/x-icon',
  '.svg': 'image/svg+xml',
  '.woff2': 'font/woff2',
  '.map': 'application/json; charset=utf-8',
};

function proxy(req, res) {
  const upstream = httpRequest(
    { host: API_HOST, port: API_PORT, method: req.method, path: req.url, headers: req.headers },
    (up) => {
      res.writeHead(up.statusCode || 502, up.headers);
      up.pipe(res);
    },
  );
  upstream.on('error', (err) => {
    res.writeHead(502, { 'content-type': 'text/plain' });
    res.end(`bad gateway to salvor: ${err.message}`);
  });
  req.pipe(upstream);
}

async function serveStatic(req, res) {
  let pathname = decodeURIComponent(new URL(req.url, 'http://x').pathname);
  if (pathname === '/') pathname = '/index.html';
  const rel = normalize(pathname).replace(/^(\.\.[/\\])+/, '');
  let file = join(DIST, rel);
  try {
    const s = await stat(file);
    if (s.isDirectory()) file = join(file, 'index.html');
  } catch {
    // SPA fallback: no such asset → hand back index.html for client routing
    if (!extname(pathname)) file = join(DIST, 'index.html');
  }
  try {
    const body = await readFile(file);
    res.writeHead(200, { 'content-type': MIME[extname(file)] || 'application/octet-stream' });
    res.end(body);
  } catch {
    res.writeHead(404, { 'content-type': 'text/plain' });
    res.end('not found');
  }
}

createServer((req, res) => {
  if (req.url.startsWith('/v1/') || req.url === '/v1' || req.url.startsWith('/health')) {
    proxy(req, res);
  } else {
    void serveStatic(req, res);
  }
}).listen(APP_PORT, '127.0.0.1', () => {
  process.stdout.write(`[bridge-proxy] app on http://127.0.0.1:${APP_PORT}/  →  api ${API_HOST}:${API_PORT}\n`);
});
