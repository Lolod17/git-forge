// Model Context Protocol at POST /mcp: JSON-RPC 2.0 over the Streamable HTTP transport, answered as
// application/json. It speaks revision 2026-07-28 (server/discover, every request self-describing, no
// session) and the handshake revisions before it (initialize + notifications/initialized, with an
// Mcp-Session-Id). The tools call the same forge operations as REST, so what one surface creates the other
// reads back with the same ids.

import { ApiError, type Forge, type Repo } from './forge.ts';
import { Shapes } from './rest.ts';

export const MCP_REVISIONS = ['2026-07-28', '2025-11-25', '2025-06-18', '2025-03-26', '2024-11-05'];
const SERVER_INFO = { name: 'gitforge', title: 'gitforge — a GitHub-compatible forge', version: '0.1.0' };

interface ToolDef {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  run: (args: Record<string, unknown>, ctx: ToolCtx) => unknown;
}

interface ToolCtx {
  readonly forge: Forge;
  readonly shapes: Shapes;
}

const str = (description: string): Record<string, unknown> => ({ type: 'string', description });
const OWNER = str('Repository owner (the authenticated user\'s login)');
const REPO = str('Repository name');

class ToolInputError extends Error {}

function need(args: Record<string, unknown>, name: string): string {
  const value = args[name];
  if (typeof value === 'string' && value !== '') return value;
  if (typeof value === 'number') return String(value);
  throw new ToolInputError(`missing required parameter: ${name}`);
}

function needNumber(args: Record<string, unknown>, name: string): number {
  const value = args[name];
  const number = typeof value === 'number' ? value : typeof value === 'string' && /^\d+$/.test(value) ? Number(value) : Number.NaN;
  if (!Number.isInteger(number) || number <= 0) throw new ToolInputError(`missing required parameter: ${name} (a positive integer)`);
  return number;
}

function repoOf(ctx: ToolCtx, args: Record<string, unknown>): Repo {
  const owner = need(args, 'owner');
  const name = need(args, 'repo');
  const repo = ctx.forge.repoByName(owner, name);
  if (repo === null) throw new ApiError(404, `repository ${owner}/${name} not found`);
  return repo;
}

const TOOLS: readonly ToolDef[] = [
  {
    name: 'create_repo',
    description: 'Create a new repository owned by the authenticated user.',
    inputSchema: {
      type: 'object',
      properties: { name: str('Repository name'), description: str('Repository description'), private: { type: 'boolean', description: 'Whether the repository is private' }, autoInit: { type: 'boolean', description: 'Initialize with a README' } },
      required: ['name'],
    },
    run: (args, ctx) => {
      const repo = ctx.forge.createRepo({ name: args.name, description: args.description, private: args.private === true || args.private === 'true' });
      if (args.autoInit === true) ctx.forge.writeFile(repo, { path: 'README.md', content: Buffer.from(`# ${repo.name}\n`, 'utf8'), message: 'Initial commit' });
      return ctx.shapes.repo(repo);
    },
  },
  {
    name: 'get_repo',
    description: 'Get a repository by owner and name.',
    inputSchema: { type: 'object', properties: { owner: OWNER, repo: REPO }, required: ['owner', 'repo'] },
    run: (args, ctx) => ctx.shapes.repo(repoOf(ctx, args)),
  },
  {
    name: 'list_repos',
    description: 'List the repositories of the authenticated user.',
    inputSchema: { type: 'object', properties: { perPage: { type: 'number', description: 'Results per page (max 100)' }, page: { type: 'number', description: 'Page number' } } },
    run: (args, ctx) => {
      const perPage = Math.min(100, Math.max(1, Number(args.perPage) || 30));
      const page = Math.max(1, Number(args.page) || 1);
      return ctx.forge.listRepos().slice((page - 1) * perPage, page * perPage).map((repo) => ctx.shapes.repo(repo));
    },
  },
  {
    name: 'create_issue',
    description: 'Create a new issue in a repository.',
    inputSchema: {
      type: 'object',
      properties: { owner: OWNER, repo: REPO, title: str('Issue title'), body: str('Issue body (Markdown)'), labels: { type: 'array', items: { type: 'string' }, description: 'Label names (each must exist)' } },
      required: ['owner', 'repo', 'title'],
    },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      need(args, 'title');
      return ctx.shapes.issue(repo, ctx.forge.createIssue(repo, { title: args.title, body: args.body, labels: args.labels }));
    },
  },
  {
    name: 'get_issue',
    description: 'Get one issue of a repository by number.',
    inputSchema: { type: 'object', properties: { owner: OWNER, repo: REPO, issue_number: { type: 'number', description: 'Issue number' } }, required: ['owner', 'repo', 'issue_number'] },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      const issue = repo.issues.get(needNumber(args, 'issue_number'));
      if (issue === undefined) throw new ApiError(404, 'issue not found');
      return ctx.shapes.issue(repo, issue);
    },
  },
  {
    name: 'list_issues',
    description: 'List the issues of a repository, newest first.',
    inputSchema: {
      type: 'object',
      properties: { owner: OWNER, repo: REPO, state: { type: 'string', enum: ['open', 'closed', 'all'], description: 'Filter by state (default open)' }, labels: { type: 'array', items: { type: 'string' }, description: 'Only issues carrying all these labels' } },
      required: ['owner', 'repo'],
    },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      const state = typeof args.state === 'string' ? args.state.toLowerCase() : 'open';
      const labels = Array.isArray(args.labels) ? args.labels.filter((label): label is string => typeof label === 'string') : undefined;
      return ctx.forge.listIssues(repo, { state, labels }).map((issue) => ctx.shapes.issue(repo, issue));
    },
  },
  {
    name: 'update_issue',
    description: 'Update an issue: title, body, state (open or closed) or labels.',
    inputSchema: {
      type: 'object',
      properties: { owner: OWNER, repo: REPO, issue_number: { type: 'number', description: 'Issue number' }, title: str('New title'), body: str('New body'), state: { type: 'string', enum: ['open', 'closed'], description: 'New state' } },
      required: ['owner', 'repo', 'issue_number'],
    },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      const issue = repo.issues.get(needNumber(args, 'issue_number'));
      if (issue === undefined) throw new ApiError(404, 'issue not found');
      const patch: Record<string, unknown> = {};
      for (const key of ['title', 'body', 'state']) if (args[key] !== undefined) patch[key] = args[key];
      return ctx.shapes.issue(repo, ctx.forge.updateIssue(repo, issue, patch));
    },
  },
  {
    name: 'add_issue_comment',
    description: 'Add a comment to an issue or pull request.',
    inputSchema: { type: 'object', properties: { owner: OWNER, repo: REPO, issue_number: { type: 'number', description: 'Issue or pull request number' }, body: str('Comment body') }, required: ['owner', 'repo', 'issue_number', 'body'] },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      const number = needNumber(args, 'issue_number');
      const target = repo.issues.get(number) ?? repo.pulls.get(number);
      if (target === undefined) throw new ApiError(404, 'issue not found');
      return ctx.shapes.comment(repo, number, ctx.forge.addComment(repo, target, need(args, 'body')));
    },
  },
  {
    name: 'create_branch',
    description: 'Create a branch from another branch (default: the repository\'s default branch).',
    inputSchema: { type: 'object', properties: { owner: OWNER, repo: REPO, branch: str('New branch name'), from_branch: str('Source branch') }, required: ['owner', 'repo', 'branch'] },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      const branch = need(args, 'branch');
      const from = typeof args.from_branch === 'string' && args.from_branch !== '' ? args.from_branch : repo.defaultBranch;
      const sha = repo.branchTip(from);
      if (sha === undefined) throw new ApiError(404, `branch ${from} not found`);
      if (repo.branchTip(branch) !== undefined) throw new ApiError(422, 'Reference already exists');
      ctx.forge.setRef(repo, `refs/heads/${branch}`, sha);
      return ctx.shapes.ref(repo, `refs/heads/${branch}`, sha);
    },
  },
  {
    name: 'create_pull_request',
    description: 'Open a pull request from a head branch into a base branch.',
    inputSchema: {
      type: 'object',
      properties: { owner: OWNER, repo: REPO, title: str('Pull request title'), body: str('Pull request description'), head: str('Branch with the changes'), base: str('Branch to merge into') },
      required: ['owner', 'repo', 'title', 'head', 'base'],
    },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      need(args, 'title');
      need(args, 'head');
      need(args, 'base');
      return ctx.shapes.pull(repo, ctx.forge.createPull(repo, { title: args.title, head: args.head, base: args.base, body: args.body }));
    },
  },
  {
    name: 'get_pull_request',
    description: 'Get one pull request by number.',
    inputSchema: { type: 'object', properties: { owner: OWNER, repo: REPO, pull_number: { type: 'number', description: 'Pull request number' } }, required: ['owner', 'repo', 'pull_number'] },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      const pull = repo.pulls.get(needNumber(args, 'pull_number'));
      if (pull === undefined) throw new ApiError(404, 'pull request not found');
      return ctx.shapes.pull(repo, pull);
    },
  },
  {
    name: 'list_pull_requests',
    description: 'List the pull requests of a repository.',
    inputSchema: { type: 'object', properties: { owner: OWNER, repo: REPO, state: { type: 'string', enum: ['open', 'closed', 'all'], description: 'Filter by state (default open)' } }, required: ['owner', 'repo'] },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      const state = typeof args.state === 'string' ? args.state.toLowerCase() : 'open';
      return [...repo.pulls.values()].filter((pull) => state === 'all' || pull.state === state).sort((a, b) => b.number - a.number).map((pull) => ctx.shapes.pull(repo, pull, false));
    },
  },
  {
    name: 'merge_pull_request',
    description: 'Merge a pull request (merge, squash or rebase).',
    inputSchema: {
      type: 'object',
      properties: { owner: OWNER, repo: REPO, pull_number: { type: 'number', description: 'Pull request number' }, merge_method: { type: 'string', enum: ['merge', 'squash', 'rebase'], description: 'Merge method (default merge)' }, commit_title: str('Title of the merge commit'), commit_message: str('Extra detail for the merge commit') },
      required: ['owner', 'repo', 'pull_number'],
    },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      const pull = repo.pulls.get(needNumber(args, 'pull_number'));
      if (pull === undefined) throw new ApiError(404, 'pull request not found');
      const result = ctx.forge.mergePull(repo, pull, { method: args.merge_method, title: args.commit_title, message: args.commit_message });
      return { sha: result.sha, merged: true, message: result.message, pull_number: pull.number };
    },
  },
  {
    name: 'get_file_contents',
    description: 'Get a file (base64 and decoded text) or a directory listing at a path and ref.',
    inputSchema: { type: 'object', properties: { owner: OWNER, repo: REPO, path: str('Path in the repository (empty for the root)'), ref: str('Branch, tag or commit (default branch when omitted)') }, required: ['owner', 'repo'] },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      const ref = typeof args.ref === 'string' && args.ref !== '' ? args.ref : repo.defaultBranch;
      const commit = ctx.forge.resolve(repo, ref);
      if (commit === null) throw new ApiError(404, `ref ${ref} not found`);
      const path = typeof args.path === 'string' ? args.path.replace(/^\/+|\/+$/g, '') : '';
      const found = ctx.forge.lookupPath(repo, commit, path);
      if (found === null) throw new ApiError(404, `${path} not found`);
      if (found.kind === 'dir') return found.entries.map((entry) => ctx.shapes.contentDirEntry(repo, path === '' ? entry.name : `${path}/${entry.name}`, entry.mode, entry.oid));
      return { ...ctx.shapes.contentFile(repo, path, found.oid, true), text: repo.get(found.oid)?.data.toString('utf8') ?? '' };
    },
  },
  {
    name: 'create_or_update_file',
    description: 'Create or update one file on a branch with a commit (pass the current blob sha to update).',
    inputSchema: {
      type: 'object',
      properties: { owner: OWNER, repo: REPO, path: str('Path of the file'), content: str('New file content (plain text)'), message: str('Commit message'), branch: str('Branch (default branch when omitted)'), sha: str('Current blob sha, required when the file exists') },
      required: ['owner', 'repo', 'path', 'content', 'message'],
    },
    run: (args, ctx) => {
      const repo = repoOf(ctx, args);
      const path = need(args, 'path');
      if (typeof args.content !== 'string') throw new ToolInputError('missing required parameter: content');
      const result = ctx.forge.writeFile(repo, { path, content: Buffer.from(args.content, 'utf8'), message: need(args, 'message'), branch: typeof args.branch === 'string' && args.branch !== '' ? args.branch : undefined, sha: typeof args.sha === 'string' ? args.sha : undefined });
      return ctx.shapes.contentWrite(repo, path, result.blob, result.commit);
    },
  },
];

/** The tools/list result, built once: two calls answer the same bytes. */
const TOOL_LIST = TOOLS.map((tool) => ({ name: tool.name, description: tool.description, inputSchema: tool.inputSchema }));

export interface McpReply {
  status: number;
  body: unknown;
  headers?: Record<string, string>;
}

type JsonRpcId = string | number | null;

function rpcError(id: JsonRpcId, code: number, message: string, data?: unknown): Record<string, unknown> {
  return { jsonrpc: '2.0', id, error: data === undefined ? { code, message } : { code, message, data } };
}

function rpcResult(id: JsonRpcId, result: unknown): Record<string, unknown> {
  return { jsonrpc: '2.0', id, result };
}

export class McpServer {
  private readonly forge: Forge;
  private readonly sessions = new Set<string>();
  private nextSession = 1;

  constructor(forge: Forge) {
    this.forge = forge;
  }

  /** One POST body (already read): JSON-RPC request, notification, or a batch of them. */
  handle(raw: string, origin: string): McpReply {
    let message: unknown;
    try {
      message = JSON.parse(raw);
    } catch {
      return { status: 400, body: rpcError(null, -32700, 'Parse error: the request body is not valid JSON') };
    }
    const shapes = new Shapes(this.forge, origin);
    if (Array.isArray(message)) {
      if (message.length === 0) return { status: 400, body: rpcError(null, -32600, 'Invalid Request: empty batch') };
      const answers = message.map((entry) => this.one(entry, shapes)).filter((answer) => answer.body !== null);
      if (answers.length === 0) return { status: 202, body: null };
      const headers = Object.assign({}, ...answers.map((answer) => answer.headers ?? {})) as Record<string, string>;
      return { status: 200, body: answers.map((answer) => answer.body), headers };
    }
    const answer = this.one(message, shapes);
    if (answer.body === null) return { status: 202, body: null, headers: answer.headers };
    return answer;
  }

  private one(message: unknown, shapes: Shapes): McpReply {
    if (typeof message !== 'object' || message === null || Array.isArray(message)) return { status: 400, body: rpcError(null, -32600, 'Invalid Request') };
    const request = message as { jsonrpc?: unknown; id?: unknown; method?: unknown; params?: unknown; result?: unknown; error?: unknown };
    const hasId = 'id' in request && (typeof request.id === 'string' || typeof request.id === 'number');
    const id: JsonRpcId = hasId ? (request.id as string | number) : null;
    if (typeof request.method !== 'string') {
      // A response or an error object sent to the server: nothing to answer.
      if (request.result !== undefined || request.error !== undefined) return { status: 202, body: null };
      return { status: 400, body: rpcError(id, -32600, 'Invalid Request: no method') };
    }
    const params = typeof request.params === 'object' && request.params !== null && !Array.isArray(request.params) ? (request.params as Record<string, unknown>) : {};
    if (!hasId) return { status: 202, body: null }; // a notification (notifications/initialized, cancelled, ...)
    switch (request.method) {
      case 'server/discover':
        return {
          status: 200,
          body: rpcResult(id, {
            supportedVersions: MCP_REVISIONS,
            capabilities: { tools: { listChanged: false } },
            serverInfo: SERVER_INFO,
            instructions: 'A GitHub-compatible forge. Tools act on the authenticated user\'s repositories, issues and pull requests.',
            _meta: { 'io.modelcontextprotocol/serverInfo': SERVER_INFO },
          }),
        };
      case 'initialize': {
        const asked = typeof params.protocolVersion === 'string' ? params.protocolVersion : '';
        const legacy = MCP_REVISIONS.slice(1);
        const version = legacy.includes(asked) ? asked : (legacy[0] as string);
        const session = `gitforge-session-${this.nextSession++}`;
        this.sessions.add(session);
        return {
          status: 200,
          headers: { 'mcp-session-id': session },
          body: rpcResult(id, { protocolVersion: version, capabilities: { tools: { listChanged: false } }, serverInfo: SERVER_INFO, instructions: 'A GitHub-compatible forge.' }),
        };
      }
      case 'ping':
        return { status: 200, body: rpcResult(id, {}) };
      case 'tools/list':
        return { status: 200, body: rpcResult(id, { tools: TOOL_LIST }) };
      case 'tools/call':
        return { status: 200, body: this.callTool(id, params, shapes) };
      case 'resources/list':
        return { status: 200, body: rpcResult(id, { resources: [] }) };
      case 'prompts/list':
        return { status: 200, body: rpcResult(id, { prompts: [] }) };
      default:
        return { status: 200, body: rpcError(id, -32601, `Method not found: ${request.method}`) };
    }
  }

  private callTool(id: JsonRpcId, params: Record<string, unknown>, shapes: Shapes): Record<string, unknown> {
    const name = typeof params.name === 'string' ? params.name : '';
    const tool = TOOLS.find((candidate) => candidate.name === name);
    if (tool === undefined) return rpcError(id, -32602, `Unknown tool: ${name === '' ? '(no name)' : name}`);
    const args = typeof params.arguments === 'object' && params.arguments !== null && !Array.isArray(params.arguments) ? (params.arguments as Record<string, unknown>) : {};
    const required = Array.isArray(tool.inputSchema.required) ? (tool.inputSchema.required as string[]) : [];
    const missing = required.filter((key) => args[key] === undefined || args[key] === null || args[key] === '');
    if (missing.length > 0) {
      return rpcResult(id, { content: [{ type: 'text', text: `missing required parameter${missing.length > 1 ? 's' : ''}: ${missing.join(', ')}` }], isError: true });
    }
    try {
      const value = tool.run(args, { forge: this.forge, shapes });
      const structured = Array.isArray(value) ? { items: value } : value;
      return rpcResult(id, { content: [{ type: 'text', text: JSON.stringify(value) }], structuredContent: structured, isError: false });
    } catch (err) {
      const message = err instanceof ApiError ? `${err.message}${err.errors === undefined ? '' : ` ${JSON.stringify(err.errors)}`} (HTTP ${err.status})` : err instanceof Error ? err.message : String(err);
      return rpcResult(id, { content: [{ type: 'text', text: message }], isError: true });
    }
  }
}
