// The forge's HTTP server: one listener for the four surfaces the suite drives, plus the boot probe.
//
//   GET  /healthz                                   200 {"ok":true}, no token (the env's boot probe)
//   GET  /{owner}/{repo}.git/info/refs?service=...  git smart HTTP advertisements (githttp.ts)
//   POST /{owner}/{repo}.git/git-upload-pack        clone and fetch, protocol v2 (v0 fallback)
//   POST /{owner}/{repo}.git/git-receive-pack       push, protocol v0
//   *    /api/v3/...                                GitHub REST v3 (rest.ts)
//   POST /api/graphql                               GitHub GraphQL v4 (graphql.ts)
//   POST /mcp                                       Model Context Protocol, Streamable HTTP (mcp.ts)
//   anything else                                   404 {"message":"Not Found"}
//
// Every request but /healthz must carry the trial's token (Authorization: Bearer <MAKEFUN_SHOP_TOKEN>, or
// git's Basic form with the token as the password); without it the answer is 401. The token names one
// login (FORGE_LOGIN, by default the contest scenario's viewer), and every repository belongs to it.
//
// app.ts stays separate from main.ts: this is the server; main.ts is the entry <-> env lifecycle.

import { mkdirSync } from 'node:fs';
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { join } from 'node:path';
import { ApiError, Forge } from './forge.ts';
import { infoRefs, receivePack, uploadPack, type GitReply } from './githttp.ts';
import { runGraphQL } from './graphql.ts';
import { McpServer } from './mcp.ts';
import { RestApi, errorBody } from './rest.ts';

const JSON_TYPE = 'application/json; charset=utf-8';
/** The login the trial's token names when FORGE_LOGIN does not say otherwise: wire.ts's scenario viewer. */
export const DEFAULT_LOGIN = 'adaiversen';
const MAX_JSON_BODY = 16 * 1024 * 1024;
const MAX_GIT_BODY = 512 * 1024 * 1024;

let requestCounter = 0;

/** A non-empty, unique-per-process request id in GitHub's colon-separated hex style. */
function requestId(): string {
  requestCounter += 1;
  return `${process.pid.toString(16).toUpperCase()}:${requestCounter.toString(16).toUpperCase().padStart(6, '0')}`;
}

function send(res: ServerResponse, status: number, body: Buffer | string, headers: Record<string, string>): void {
  if (res.headersSent) return;
  const bytes = typeof body === 'string' ? Buffer.from(body, 'utf8') : body;
  res.writeHead(status, { ...headers, 'content-length': String(bytes.length) });
  res.end(bytes);
}

function sendJson(res: ServerResponse, status: number, body: unknown, headers: Record<string, string> = {}): void {
  const text = status === 204 || body === null ? '' : `${JSON.stringify(body)}\n`;
  send(res, status, text, { 'content-type': JSON_TYPE, 'x-github-request-id': requestId(), ...headers });
}

/** The whole request body, refusing one past `limit` bytes. */
function readBody(req: IncomingMessage, limit: number): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    req.on('data', (chunk: Buffer) => {
      size += chunk.length;
      if (size > limit) {
        reject(new ApiError(413, 'Request body too large'));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

type AuthResult = 'ok' | 'missing' | 'bad';

function authenticate(req: IncomingMessage, token: string | null): AuthResult {
  const header = req.headers.authorization;
  if (header === undefined || header.trim() === '') return 'missing';
  const [scheme, ...rest] = header.trim().split(/\s+/);
  const value = rest.join(' ');
  let presented: string | null = null;
  const kind = (scheme ?? '').toLowerCase();
  if (kind === 'bearer' || kind === 'token') presented = value;
  else if (kind === 'basic') {
    const decoded = Buffer.from(value, 'base64').toString('utf8');
    const colon = decoded.indexOf(':');
    presented = colon >= 0 ? decoded.slice(colon + 1) : decoded;
  }
  if (presented === null || presented === '') return 'missing';
  if (token === null) return 'ok';
  return presented === token ? 'ok' : 'bad';
}

/** The origin the client used to reach this server (for absolute URLs such as a Link header). */
function originOf(req: IncomingMessage): string {
  const host = req.headers.host ?? '127.0.0.1';
  return `http://${host}`;
}

function dataDirFromEnv(): string {
  const configured = process.env.FORGE_DATA_DIR;
  if (configured !== undefined && configured !== '') return configured;
  const dir = join('/tmp', 'gitforge-data', `${process.pid}-${Date.now().toString(36)}`);
  mkdirSync(dir, { recursive: true });
  return dir;
}

export function createForgeFromEnv(): Forge {
  const token = process.env.MAKEFUN_SHOP_TOKEN;
  return new Forge({
    login: process.env.FORGE_LOGIN !== undefined && process.env.FORGE_LOGIN !== '' ? process.env.FORGE_LOGIN : DEFAULT_LOGIN,
    token: token === undefined || token === '' ? null : token,
    dataDir: dataDirFromEnv(),
  });
}

const GIT_PATH = /^\/([^/]+)\/([^/]+?)\.git\/(info\/refs|git-upload-pack|git-receive-pack)$/;

/** Build the server. It binds nothing until the caller listens; the entry listens on an ephemeral port. */
export function createApp(forge: Forge = createForgeFromEnv()): Server {
  const rest = new RestApi(forge);
  const mcp = new McpServer(forge);

  const handle = async (req: IncomingMessage, res: ServerResponse): Promise<void> => {
    const method = (req.method ?? 'GET').toUpperCase();
    let url: URL;
    try {
      url = new URL(req.url ?? '/', 'http://entry');
    } catch {
      sendJson(res, 400, errorBody('Bad Request'));
      return;
    }
    const path = url.pathname;

    // The env's boot check calls this twice: 200 {"ok":true}.
    if (path === '/healthz') {
      sendJson(res, 200, { ok: true });
      return;
    }

    const auth = authenticate(req, forge.token);

    // ---------------------------------------------------------------- git smart HTTP
    const git = GIT_PATH.exec(path);
    if (git !== null) {
      if (auth !== 'ok') {
        send(res, 401, 'Authentication required\n', { 'content-type': 'text/plain; charset=utf-8', 'www-authenticate': 'Basic realm="gitforge"', 'x-github-request-id': requestId() });
        return;
      }
      const [, owner, name, action] = git;
      const repo = forge.repoByName(decodeURIComponent(owner ?? ''), decodeURIComponent(name ?? ''));
      if (repo === null) {
        send(res, 404, 'Repository not found\n', { 'content-type': 'text/plain; charset=utf-8', 'x-github-request-id': requestId() });
        return;
      }
      const gitProtocol = String(req.headers['git-protocol'] ?? '');
      let reply: GitReply;
      if (action === 'info/refs' && (method === 'GET' || method === 'HEAD')) {
        reply = infoRefs(forge, repo, url.searchParams.get('service') ?? '', gitProtocol);
      } else if (action === 'git-upload-pack' && method === 'POST') {
        reply = uploadPack(forge, repo, await readBody(req, MAX_GIT_BODY), gitProtocol);
      } else if (action === 'git-receive-pack' && method === 'POST') {
        reply = receivePack(forge, repo, await readBody(req, MAX_GIT_BODY));
      } else {
        send(res, 405, 'Method Not Allowed\n', { 'content-type': 'text/plain; charset=utf-8' });
        return;
      }
      send(res, reply.status, reply.body, { 'content-type': reply.contentType, 'x-github-request-id': requestId(), ...(reply.headers ?? {}) });
      return;
    }

    // ---------------------------------------------------------------- REST v3
    if (path === '/api/v3' || path.startsWith('/api/v3/')) {
      const scopes = { 'x-oauth-scopes': 'repo, read:org', 'x-accepted-oauth-scopes': 'repo', 'x-github-media-type': 'github.v3; format=json' };
      if (auth === 'missing') {
        sendJson(res, 401, errorBody('Requires authentication'), { 'www-authenticate': 'Bearer realm="gitforge"' });
        return;
      }
      if (auth === 'bad') {
        sendJson(res, 401, errorBody('Bad credentials'), { 'www-authenticate': 'Bearer realm="gitforge"' });
        return;
      }
      const raw = method === 'GET' || method === 'HEAD' ? Buffer.alloc(0) : await readBody(req, MAX_JSON_BODY);
      let body: unknown;
      if (raw.length > 0 && raw.toString('utf8').trim() !== '') {
        try {
          body = JSON.parse(raw.toString('utf8'));
        } catch {
          sendJson(res, 400, errorBody('Problems parsing JSON'), scopes);
          return;
        }
      }
      const reply = rest.handle({ method, path: path.slice('/api/v3'.length), query: url.searchParams, body, origin: originOf(req) });
      sendJson(res, reply.status, reply.body, { ...scopes, ...(reply.headers ?? {}) });
      return;
    }

    // ---------------------------------------------------------------- GraphQL v4
    if (path === '/api/graphql' || path === '/api/graphql/') {
      if (auth !== 'ok') {
        sendJson(res, 401, errorBody('This endpoint requires you to be authenticated.'), { 'www-authenticate': 'Bearer realm="gitforge"' });
        return;
      }
      if (method !== 'POST') {
        sendJson(res, 404, errorBody('Not Found'));
        return;
      }
      const raw = await readBody(req, MAX_JSON_BODY);
      let body: unknown;
      try {
        body = JSON.parse(raw.toString('utf8'));
      } catch {
        sendJson(res, 400, errorBody('Problems parsing JSON'));
        return;
      }
      const ctx = { forge, origin: originOf(req) };
      const answer = Array.isArray(body) ? body.map((operation) => runGraphQL(ctx, operation)) : runGraphQL(ctx, body);
      sendJson(res, 200, answer, { 'x-oauth-scopes': 'repo, read:org' });
      return;
    }

    // ---------------------------------------------------------------- MCP
    if (path === '/mcp' || path === '/mcp/') {
      if (auth !== 'ok') {
        sendJson(res, 401, { jsonrpc: '2.0', id: null, error: { code: -32001, message: auth === 'missing' ? 'Unauthorized: an Authorization: Bearer token is required' : 'Unauthorized: bad credentials' } }, { 'www-authenticate': 'Bearer realm="gitforge"' });
        return;
      }
      if (method === 'DELETE') {
        send(res, 204, '', { 'x-github-request-id': requestId() });
        return;
      }
      if (method !== 'POST') {
        sendJson(res, 405, { jsonrpc: '2.0', id: null, error: { code: -32000, message: 'Method Not Allowed: this endpoint takes POST' } }, { allow: 'POST, DELETE' });
        return;
      }
      const raw = await readBody(req, MAX_JSON_BODY);
      const reply = mcp.handle(raw.toString('utf8'), originOf(req));
      if (reply.status === 202 || reply.body === null) {
        send(res, 202, '', { 'x-github-request-id': requestId(), ...(reply.headers ?? {}) });
        return;
      }
      sendJson(res, reply.status, reply.body, reply.headers ?? {});
      return;
    }

    // Everything else, every method: GitHub's 404 body.
    sendJson(res, 404, errorBody('Not Found'));
  };

  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    handle(req, res).catch((err: unknown) => {
      if (err instanceof ApiError) {
        sendJson(res, err.status, errorBody(err.message, err.errors), { 'x-oauth-scopes': 'repo, read:org' });
        return;
      }
      process.stderr.write(`[forge] ${req.method ?? '?'} ${req.url ?? '?'} failed: ${err instanceof Error ? (err.stack ?? err.message) : String(err)}\n`);
      sendJson(res, 500, errorBody('Server Error'));
    });
  });
  // Keep connections warm for the suite's keep-alive read workload; never time out a slow pack.
  server.keepAliveTimeout = 65_000;
  server.headersTimeout = 66_000;
  server.requestTimeout = 0;
  server.on('clientError', (_err, socket) => {
    if (socket.writable) socket.end('HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{"message":"Bad Request"}\n\n');
    else socket.destroy();
  });
  server.on('close', () => forge.close());
  return server;
}
