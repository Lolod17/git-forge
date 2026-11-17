// GitHub REST v3 under /api/v3: repositories, branches, refs, commits, contents, labels, issues, comments
// and pull requests (files, commits, merge), in GitHub's JSON shapes. Every reply is JSON; errors carry a
// string `message`; list endpoints page with per_page/page and a Link header.

import { ApiError, type Comment, type Forge, type Issue, type Label, type Pull, type Repo } from './forge.ts';
import { isOid, parseCommit, parseSignature } from './git.ts';

export interface RestRequest {
  method: string;
  /** The path after /api/v3, raw (still percent-encoded), starting with '/' or empty. */
  path: string;
  query: URLSearchParams;
  /** The parsed JSON body; `undefined` when the request had none. */
  body: unknown;
  /** `http://host` as the client addressed this server. */
  origin: string;
}

export interface RestReply {
  status: number;
  body: unknown;
  headers?: Record<string, string>;
}

const DOCS = 'https://docs.github.com/rest';

export function errorBody(message: string, errors?: unknown[]): Record<string, unknown> {
  return errors === undefined ? { message, documentation_url: DOCS } : { message, errors, documentation_url: DOCS };
}

function ok(body: unknown, status = 200, headers?: Record<string, string>): RestReply {
  return { status, body, headers };
}

function notFound(): never {
  throw new ApiError(404, 'Not Found');
}

/** The request body as an object (an absent body is {}); anything else is a 400. */
function objectBody(req: RestRequest): Record<string, unknown> {
  if (req.body === undefined || req.body === null) return {};
  if (typeof req.body !== 'object' || Array.isArray(req.body)) throw new ApiError(400, 'Problems parsing JSON');
  return req.body as Record<string, unknown>;
}

function decode(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    throw new ApiError(400, 'Invalid percent-encoding in the path');
  }
}

// ------------------------------------------------------------------ serializers

export class Shapes {
  readonly forge: Forge;
  readonly origin: string;
  readonly api: string;

  constructor(forge: Forge, origin: string) {
    this.forge = forge;
    this.origin = origin;
    this.api = `${origin}/api/v3`;
  }

  user(): Record<string, unknown> {
    const user = this.forge.user;
    return {
      login: user.login,
      id: user.id,
      node_id: nodeId('U', user.id),
      avatar_url: `${this.origin}/avatars/${user.login}`,
      gravatar_id: '',
      url: `${this.api}/users/${user.login}`,
      html_url: `${this.origin}/${user.login}`,
      repos_url: `${this.api}/users/${user.login}/repos`,
      type: 'User',
      site_admin: false,
    };
  }

  repo(repo: Repo): Record<string, unknown> {
    const login = this.forge.user.login;
    const full = `${login}/${repo.name}`;
    const url = `${this.api}/repos/${full}`;
    const openIssues = [...repo.issues.values()].filter((issue) => issue.state === 'open').length;
    return {
      id: repo.id,
      node_id: nodeId('R', repo.id),
      name: repo.name,
      full_name: full,
      private: repo.isPrivate,
      owner: this.user(),
      html_url: `${this.origin}/${full}`,
      description: repo.description,
      fork: false,
      url,
      clone_url: `${this.origin}/${full}.git`,
      git_url: `${this.origin}/${full}.git`,
      ssh_url: `${this.origin}/${full}.git`,
      issues_url: `${url}/issues{/number}`,
      pulls_url: `${url}/pulls{/number}`,
      contents_url: `${url}/contents/{+path}`,
      branches_url: `${url}/branches{/branch}`,
      commits_url: `${url}/commits{/sha}`,
      created_at: repo.createdAt,
      updated_at: repo.updatedAt,
      pushed_at: repo.pushedAt ?? repo.createdAt,
      size: 0,
      stargazers_count: 0,
      watchers_count: 0,
      forks_count: 0,
      open_issues_count: openIssues,
      open_issues: openIssues,
      language: null,
      has_issues: repo.hasIssues,
      has_projects: false,
      has_wiki: false,
      has_pages: false,
      has_downloads: false,
      archived: false,
      disabled: false,
      visibility: repo.isPrivate ? 'private' : 'public',
      default_branch: repo.defaultBranch,
      permissions: { admin: true, maintain: true, push: true, triage: true, pull: true },
      allow_squash_merge: true,
      allow_merge_commit: true,
      allow_rebase_merge: true,
    };
  }

  label(repo: Repo, label: Label): Record<string, unknown> {
    return {
      id: label.id,
      node_id: nodeId('LA', label.id),
      url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/labels/${encodeURIComponent(label.name)}`,
      name: label.name,
      color: label.color,
      description: label.description,
      default: false,
    };
  }

  issue(repo: Repo, issue: Issue): Record<string, unknown> {
    const url = `${this.api}/repos/${this.forge.user.login}/${repo.name}/issues/${issue.number}`;
    return {
      id: issue.id,
      node_id: nodeId('I', issue.id),
      url,
      repository_url: `${this.api}/repos/${this.forge.user.login}/${repo.name}`,
      comments_url: `${url}/comments`,
      html_url: `${this.origin}/${this.forge.user.login}/${repo.name}/issues/${issue.number}`,
      number: issue.number,
      state: issue.state,
      state_reason: issue.stateReason,
      title: issue.title,
      body: issue.body,
      user: this.user(),
      labels: issue.labels.map((name) => repo.labels.get(name.toLowerCase())).filter((label): label is Label => label !== undefined).map((label) => this.label(repo, label)),
      assignee: null,
      assignees: [],
      milestone: null,
      locked: false,
      comments: issue.comments.length,
      created_at: issue.createdAt,
      updated_at: issue.updatedAt,
      closed_at: issue.closedAt,
      author_association: 'OWNER',
    };
  }

  comment(repo: Repo, number: number, comment: Comment): Record<string, unknown> {
    return {
      id: comment.id,
      node_id: nodeId('IC', comment.id),
      url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/issues/comments/${comment.id}`,
      html_url: `${this.origin}/${this.forge.user.login}/${repo.name}/issues/${number}#issuecomment-${comment.id}`,
      issue_url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/issues/${number}`,
      body: comment.body,
      user: this.user(),
      created_at: comment.createdAt,
      updated_at: comment.updatedAt,
      author_association: 'OWNER',
    };
  }

  private branchSide(repo: Repo, ref: string, sha: string): Record<string, unknown> {
    return { label: `${this.forge.user.login}:${ref}`, ref, sha, user: this.user(), repo: this.repo(repo) };
  }

  pull(repo: Repo, pull: Pull, full = true): Record<string, unknown> {
    const tips = this.forge.pullTips(repo, pull);
    const url = `${this.api}/repos/${this.forge.user.login}/${repo.name}/pulls/${pull.number}`;
    const shape: Record<string, unknown> = {
      id: pull.id,
      node_id: nodeId('PR', pull.id),
      url,
      html_url: `${this.origin}/${this.forge.user.login}/${repo.name}/pull/${pull.number}`,
      diff_url: `${this.origin}/${this.forge.user.login}/${repo.name}/pull/${pull.number}.diff`,
      issue_url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/issues/${pull.number}`,
      number: pull.number,
      state: pull.state,
      locked: false,
      title: pull.title,
      body: pull.body,
      user: this.user(),
      labels: [],
      created_at: pull.createdAt,
      updated_at: pull.updatedAt,
      closed_at: pull.closedAt,
      merged_at: pull.mergedAt,
      merge_commit_sha: pull.mergeCommitSha,
      draft: false,
      head: this.branchSide(repo, pull.head, tips.head),
      base: this.branchSide(repo, pull.base, tips.base),
      author_association: 'OWNER',
      merged: pull.merged,
    };
    if (full) {
      const mergeable = this.forge.mergeable(repo, pull);
      const files = this.forge.pullFiles(repo, pull);
      shape.mergeable = mergeable;
      shape.mergeable_state = mergeable === null ? 'unknown' : mergeable ? 'clean' : 'dirty';
      shape.merged_by = pull.merged ? this.user() : null;
      shape.comments = pull.comments.length;
      shape.review_comments = 0;
      shape.commits = this.forge.pullCommits(repo, pull).length;
      shape.additions = files.reduce((sum, file) => sum + file.additions, 0);
      shape.deletions = files.reduce((sum, file) => sum + file.deletions, 0);
      shape.changed_files = files.length;
    }
    return shape;
  }

  commit(repo: Repo, oid: string): Record<string, unknown> {
    const commit = this.forge.commitOf(repo, oid);
    if (commit === null) notFound();
    const author = parseSignature(commit.author);
    const committer = parseSignature(commit.committer);
    const url = `${this.api}/repos/${this.forge.user.login}/${repo.name}/commits/${oid}`;
    const person = (sig: ReturnType<typeof parseSignature>): Record<string, unknown> => ({ name: sig.name, email: sig.email, date: new Date(sig.time * 1000).toISOString().replace(/\.\d{3}Z$/, 'Z') });
    return {
      sha: oid,
      node_id: nodeId('C', oid),
      url,
      html_url: `${this.origin}/${this.forge.user.login}/${repo.name}/commit/${oid}`,
      commit: {
        url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/git/commits/${oid}`,
        author: person(author),
        committer: person(committer),
        message: commit.message.replace(/\n$/, ''),
        tree: { sha: commit.tree, url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/git/trees/${commit.tree}` },
        comment_count: 0,
      },
      author: null,
      committer: null,
      parents: commit.parents.map((sha) => ({ sha, url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/commits/${sha}` })),
    };
  }

  contentFile(repo: Repo, path: string, oid: string, withContent: boolean): Record<string, unknown> {
    const blob = repo.get(oid);
    const size = blob?.data.length ?? 0;
    const name = path.slice(path.lastIndexOf('/') + 1);
    const url = `${this.api}/repos/${this.forge.user.login}/${repo.name}/contents/${encodePath(path)}`;
    const shape: Record<string, unknown> = {
      type: 'file',
      name,
      path,
      sha: oid,
      size,
      url,
      html_url: `${this.origin}/${this.forge.user.login}/${repo.name}/blob/${repo.defaultBranch}/${encodePath(path)}`,
      git_url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/git/blobs/${oid}`,
      download_url: `${this.origin}/${this.forge.user.login}/${repo.name}/raw/${repo.defaultBranch}/${encodePath(path)}`,
    };
    if (withContent) {
      shape.encoding = 'base64';
      shape.content = (blob?.data ?? Buffer.alloc(0)).toString('base64').replace(/(.{60})/g, '$1\n');
    }
    shape._links = { self: url, git: shape.git_url, html: shape.html_url };
    return shape;
  }

  contentDirEntry(repo: Repo, path: string, mode: string, oid: string): Record<string, unknown> {
    if (mode !== '40000') return { ...this.contentFile(repo, path, oid, false), type: mode === '120000' ? 'symlink' : mode === '160000' ? 'submodule' : 'file' };
    const url = `${this.api}/repos/${this.forge.user.login}/${repo.name}/contents/${encodePath(path)}`;
    return {
      type: 'dir',
      name: path.slice(path.lastIndexOf('/') + 1),
      path,
      sha: oid,
      size: 0,
      url,
      html_url: `${this.origin}/${this.forge.user.login}/${repo.name}/tree/${repo.defaultBranch}/${encodePath(path)}`,
      git_url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/git/trees/${oid}`,
      download_url: null,
      _links: { self: url },
    };
  }

  branch(repo: Repo, name: string, sha: string): Record<string, unknown> {
    return {
      name,
      commit: { sha, url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/commits/${sha}` },
      protected: false,
    };
  }

  ref(repo: Repo, ref: string, sha: string): Record<string, unknown> {
    const type = repo.get(sha)?.type ?? 'commit';
    return {
      ref,
      node_id: nodeId('REF', `${repo.id}:${ref}`),
      url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/git/${ref}`,
      object: { sha, type, url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/git/${type}s/${sha}` },
    };
  }

  /** The {content, commit} reply of a contents write. */
  contentWrite(repo: Repo, path: string | null, blob: string | null, commit: string): Record<string, unknown> {
    const full = this.commit(repo, commit);
    const inner = full.commit as Record<string, unknown>;
    return {
      content: path === null || blob === null ? null : this.contentFile(repo, path, blob, true),
      commit: {
        sha: commit,
        node_id: full.node_id,
        url: `${this.api}/repos/${this.forge.user.login}/${repo.name}/git/commits/${commit}`,
        html_url: full.html_url,
        author: inner.author,
        committer: inner.committer,
        message: inner.message,
        tree: inner.tree,
        parents: full.parents,
      },
    };
  }
}

/** An opaque, stable global id: a type prefix and the base64url of the entity's key. */
export function nodeId(prefix: string, key: string | number): string {
  return `${prefix}_${Buffer.from(`gitforge:${prefix}:${key}`, 'utf8').toString('base64url')}`;
}

/** The type prefix and key of a global id made by nodeId, or null. */
export function parseNodeId(id: string): { prefix: string; key: string } | null {
  const under = id.indexOf('_');
  if (under <= 0) return null;
  const prefix = id.slice(0, under);
  let decoded: string;
  try {
    decoded = Buffer.from(id.slice(under + 1), 'base64url').toString('utf8');
  } catch {
    return null;
  }
  const head = `gitforge:${prefix}:`;
  if (!decoded.startsWith(head)) return null;
  return { prefix, key: decoded.slice(head.length) };
}

function encodePath(path: string): string {
  return path.split('/').map(encodeURIComponent).join('/');
}

// ------------------------------------------------------------------ pagination

function pageParams(query: URLSearchParams, defaultSize = 30): { perPage: number; page: number } {
  const perPageRaw = Number.parseInt(query.get('per_page') ?? '', 10);
  const pageRaw = Number.parseInt(query.get('page') ?? '', 10);
  const perPage = Number.isFinite(perPageRaw) && perPageRaw > 0 ? Math.min(100, perPageRaw) : defaultSize;
  const page = Number.isFinite(pageRaw) && pageRaw > 0 ? pageRaw : 1;
  return { perPage, page };
}

/** One page of `items` with GitHub's Link header (next, last, first, prev) on this server's own origin. */
function paged<T>(req: RestRequest, items: readonly T[], render: (item: T) => unknown): RestReply {
  const { perPage, page } = pageParams(req.query);
  const slice = items.slice((page - 1) * perPage, page * perPage);
  const last = Math.max(1, Math.ceil(items.length / perPage));
  const links: string[] = [];
  const at = (n: number): string => {
    const query = new URLSearchParams(req.query);
    query.set('per_page', String(perPage));
    query.set('page', String(n));
    return `<${req.origin}/api/v3${req.path}?${query.toString()}>`;
  };
  if (page < last) links.push(`${at(page + 1)}; rel="next"`, `${at(last)}; rel="last"`);
  if (page > 1) links.push(`${at(1)}; rel="first"`, `${at(Math.min(page - 1, last))}; rel="prev"`);
  return ok(slice.map(render), 200, links.length > 0 ? { link: links.join(', ') } : undefined);
}

// ------------------------------------------------------------------ the router

type Handler = (req: RestRequest, match: string[], shapes: Shapes) => RestReply;

interface Route {
  method: string;
  pattern: RegExp;
  handler: Handler;
}

export class RestApi {
  readonly forge: Forge;
  private readonly routes: Route[] = [];

  constructor(forge: Forge) {
    this.forge = forge;
    this.register();
  }

  handle(req: RestRequest): RestReply {
    const shapes = new Shapes(this.forge, req.origin);
    const path = req.path === '' ? '/' : req.path;
    for (const route of this.routes) {
      if (route.method !== req.method && !(route.method === 'GET' && req.method === 'HEAD')) continue;
      const match = route.pattern.exec(path);
      if (match !== null) return route.handler(req, match.slice(1), shapes);
    }
    throw new ApiError(404, 'Not Found');
  }

  private on(method: string, pattern: string, handler: Handler): void {
    // `:x` matches one segment, `*x` the rest of the path (slashes included).
    const source = pattern.replace(/:\w+/g, '([^/]+)').replace(/\*\w+/g, '(.+)');
    this.routes.push({ method, pattern: new RegExp(`^${source}/?$`), handler });
  }

  /** The repository a /repos/{owner}/{repo} path names, or a 404 (another owner's name is a 404 too). */
  private repo(match: string[]): Repo {
    const repo = this.forge.repoByName(decode(match[0] ?? ''), decode(match[1] ?? ''));
    if (repo === null) throw new ApiError(404, 'Not Found');
    return repo;
  }

  private issue(repo: Repo, raw: string | undefined): Issue {
    const issue = repo.issues.get(Number(raw));
    if (issue === undefined) throw new ApiError(404, 'Not Found');
    return issue;
  }

  private pull(repo: Repo, raw: string | undefined): Pull {
    const pull = repo.pulls.get(Number(raw));
    if (pull === undefined) throw new ApiError(404, 'Not Found');
    return pull;
  }

  private register(): void {
    const forge = this.forge;

    this.on('GET', '/', (req) =>
      ok({
        current_user_url: `${req.origin}/api/v3/user`,
        repository_url: `${req.origin}/api/v3/repos/{owner}/{repo}`,
        user_repositories_url: `${req.origin}/api/v3/user/repos`,
        issues_url: `${req.origin}/api/v3/issues`,
        rate_limit_url: `${req.origin}/api/v3/rate_limit`,
      }),
    );
    this.on('GET', '/meta', () => ok({ verifiable_password_authentication: false, installed_version: '3.15.0', gitforge: { version: '0.1.0' } }));
    this.on('GET', '/rate_limit', () => ok({ resources: { core: { limit: 5000, remaining: 5000, reset: 0, used: 0 } }, rate: { limit: 5000, remaining: 5000, reset: 0, used: 0 } }));
    this.on('GET', '/user', (_req, _m, s) => ok({ ...s.user(), name: forge.user.name, email: null, public_repos: forge.listRepos().length }));
    this.on('GET', '/users/:login', (_req, m, s) => {
      if (!forge.isViewer(decode(m[0] ?? ''))) notFound();
      return ok({ ...s.user(), name: forge.user.name });
    });

    // repositories
    this.on('GET', '/user/repos', (req, _m, s) => {
      const visibility = req.query.get('visibility');
      let repos = forge.listRepos();
      if (visibility === 'private') repos = repos.filter((repo) => repo.isPrivate);
      if (visibility === 'public') repos = repos.filter((repo) => !repo.isPrivate);
      return paged(req, repos, (repo) => s.repo(repo));
    });
    this.on('GET', '/users/:login/repos', (req, m, s) => {
      if (!forge.isViewer(decode(m[0] ?? ''))) notFound();
      return paged(req, forge.listRepos(), (repo) => s.repo(repo));
    });
    this.on('POST', '/user/repos', (req, _m, s) => {
      const body = objectBody(req);
      const repo = forge.createRepo({ name: body.name, description: body.description, private: body.private ?? (body.visibility === 'private' ? true : undefined), hasIssues: body.has_issues });
      if (body.auto_init === true) {
        forge.writeFile(repo, { path: 'README.md', content: Buffer.from(`# ${repo.name}\n${repo.description === null ? '' : `\n${repo.description}\n`}`, 'utf8'), message: 'Initial commit' });
      }
      return ok(s.repo(repo), 201);
    });
    this.on('GET', '/repos/:owner/:repo', (_req, m, s) => ok(s.repo(this.repo(m))));
    this.on('PATCH', '/repos/:owner/:repo', (req, m, s) => ok(s.repo(forge.updateRepo(this.repo(m), objectBody(req)))));
    this.on('DELETE', '/repos/:owner/:repo', (_req, m) => {
      forge.deleteRepo(this.repo(m));
      return ok(null, 204);
    });

    // branches and refs
    this.on('GET', '/repos/:owner/:repo/branches', (req, m, s) => {
      const repo = this.repo(m);
      const branches = [...repo.refs.entries()].filter(([ref]) => ref.startsWith('refs/heads/')).sort(([a], [b]) => (a < b ? -1 : 1));
      return paged(req, branches, ([ref, sha]) => s.branch(repo, ref.slice('refs/heads/'.length), sha));
    });
    this.on('GET', '/repos/:owner/:repo/branches/*branch', (_req, m, s) => {
      const repo = this.repo(m);
      const name = decode(m[2] ?? '');
      const sha = repo.branchTip(name);
      if (sha === undefined) throw new ApiError(404, 'Branch not found');
      return ok({ ...s.branch(repo, name, sha), commit: s.commit(repo, sha), _links: { self: `${s.api}/repos/${forge.user.login}/${repo.name}/branches/${name}` } });
    });
    const listRefs = (req: RestRequest, m: string[], s: Shapes): RestReply => {
      const repo = this.repo(m);
      const prefix = m[2] === undefined ? 'refs/' : `refs/${decode(m[2])}`;
      const refs = [...repo.refs.entries()].filter(([ref]) => ref.startsWith(prefix)).sort(([a], [b]) => (a < b ? -1 : 1));
      return paged(req, refs, ([ref, sha]) => s.ref(repo, ref, sha));
    };
    this.on('GET', '/repos/:owner/:repo/git/refs', listRefs);
    this.on('GET', '/repos/:owner/:repo/git/matching-refs/*ref', listRefs);
    const getRef = (_req: RestRequest, m: string[], s: Shapes): RestReply => {
      const repo = this.repo(m);
      const ref = `refs/${decode(m[2] ?? '')}`;
      const sha = repo.refs.get(ref);
      if (sha === undefined) throw new ApiError(404, 'Not Found');
      return ok(s.ref(repo, ref, sha));
    };
    this.on('GET', '/repos/:owner/:repo/git/refs/*ref', getRef);
    this.on('GET', '/repos/:owner/:repo/git/ref/*ref', getRef);
    this.on('POST', '/repos/:owner/:repo/git/refs', (req, m, s) => {
      const repo = this.repo(m);
      const body = objectBody(req);
      const ref = typeof body.ref === 'string' ? body.ref : '';
      const sha = typeof body.sha === 'string' ? body.sha : '';
      if (!ref.startsWith('refs/') || ref.split('/').length < 3) throw new ApiError(422, 'Reference name must start with \'refs/\' and have at least two slashes.');
      if (!isOid(sha) || repo.get(sha) === undefined) throw new ApiError(422, 'Object does not exist');
      if (repo.refs.has(ref)) throw new ApiError(422, 'Reference already exists');
      forge.setRef(repo, ref, sha);
      return ok(s.ref(repo, ref, sha), 201);
    });
    this.on('PATCH', '/repos/:owner/:repo/git/refs/*ref', (req, m, s) => {
      const repo = this.repo(m);
      const ref = `refs/${decode(m[2] ?? '')}`;
      const current = repo.refs.get(ref);
      if (current === undefined) throw new ApiError(422, 'Reference does not exist');
      const body = objectBody(req);
      const sha = typeof body.sha === 'string' ? body.sha : '';
      if (!isOid(sha) || repo.get(sha) === undefined) throw new ApiError(422, 'Object does not exist');
      if (body.force !== true && !forge.isAncestor(repo, current, sha)) throw new ApiError(422, 'Update is not a fast forward');
      forge.setRef(repo, ref, sha);
      return ok(s.ref(repo, ref, sha));
    });
    this.on('DELETE', '/repos/:owner/:repo/git/refs/*ref', (_req, m) => {
      const repo = this.repo(m);
      const ref = `refs/${decode(m[2] ?? '')}`;
      if (!repo.refs.has(ref)) throw new ApiError(422, 'Reference does not exist');
      forge.setRef(repo, ref, null);
      return ok(null, 204);
    });

    // git data
    this.on('GET', '/repos/:owner/:repo/git/commits/:sha', (_req, m, s) => {
      const repo = this.repo(m);
      const full = s.commit(repo, decode(m[2] ?? ''));
      const inner = full.commit as Record<string, unknown>;
      return ok({ sha: full.sha, node_id: full.node_id, url: inner.url, author: inner.author, committer: inner.committer, message: inner.message, tree: inner.tree, parents: full.parents });
    });
    this.on('GET', '/repos/:owner/:repo/git/trees/*sha', (req, m, s) => {
      const repo = this.repo(m);
      const sha = forge.resolve(repo, decode(m[2] ?? ''));
      const treeSha = sha === null ? decode(m[2] ?? '') : (forge.commitOf(repo, sha)?.tree ?? sha);
      if (repo.get(treeSha)?.type !== 'tree') notFound();
      const recursive = req.query.has('recursive') && req.query.get('recursive') !== '0' && req.query.get('recursive') !== 'false';
      const tree: Record<string, unknown>[] = [];
      const walk = (oid: string, prefix: string): void => {
        for (const entry of forge.treeEntries(repo, oid)) {
          const path = prefix === '' ? entry.name : `${prefix}/${entry.name}`;
          const isTree = entry.mode === '40000';
          tree.push({ path, mode: isTree ? '040000' : entry.mode, type: isTree ? 'tree' : entry.mode === '160000' ? 'commit' : 'blob', sha: entry.oid, ...(isTree ? {} : { size: repo.get(entry.oid)?.data.length ?? 0 }), url: `${s.api}/repos/${forge.user.login}/${repo.name}/git/${isTree ? 'trees' : 'blobs'}/${entry.oid}` });
          if (isTree && recursive) walk(entry.oid, path);
        }
      };
      walk(treeSha, '');
      return ok({ sha: treeSha, url: `${s.api}/repos/${forge.user.login}/${repo.name}/git/trees/${treeSha}`, tree, truncated: false });
    });
    this.on('GET', '/repos/:owner/:repo/git/blobs/:sha', (_req, m, s) => {
      const repo = this.repo(m);
      const sha = decode(m[2] ?? '');
      const blob = repo.get(sha);
      if (blob === undefined || blob.type !== 'blob') notFound();
      return ok({ sha, node_id: nodeId('B', sha), size: blob.data.length, url: `${s.api}/repos/${forge.user.login}/${repo.name}/git/blobs/${sha}`, content: blob.data.toString('base64'), encoding: 'base64' });
    });

    // commits
    this.on('GET', '/repos/:owner/:repo/commits', (req, m, s) => {
      const repo = this.repo(m);
      if (repo.refs.size === 0) throw new ApiError(409, 'Git Repository is empty.');
      const from = req.query.get('sha') ?? repo.defaultBranch;
      const tip = forge.resolve(repo, from);
      if (tip === null) throw new ApiError(404, `No commit found for SHA: ${from}`);
      let commits = forge.log(repo, tip);
      const path = req.query.get('path');
      if (path !== null && path !== '') {
        commits = commits.filter((oid) => {
          const commit = forge.commitOf(repo, oid) as ReturnType<typeof parseCommit>;
          const here = forge.lookupPath(repo, oid, path);
          const parent = commit.parents[0];
          const there = parent === undefined ? null : forge.lookupPath(repo, parent, path);
          return JSON.stringify(here === null ? null : here.oid) !== JSON.stringify(there === null ? null : there.oid);
        });
      }
      return paged(req, commits, (oid) => s.commit(repo, oid));
    });
    this.on('GET', '/repos/:owner/:repo/commits/*ref', (_req, m, s) => {
      const repo = this.repo(m);
      const sha = forge.resolve(repo, decode(m[2] ?? ''));
      if (sha === null) throw new ApiError(422, `No commit found for SHA: ${decode(m[2] ?? '')}`);
      const commit = forge.commitOf(repo, sha) as ReturnType<typeof parseCommit>;
      const before = commit.parents[0] === undefined ? new Map() : forge.flatten(repo, (forge.commitOf(repo, commit.parents[0]) as { tree: string }).tree);
      const after = forge.flatten(repo, commit.tree);
      const files: Record<string, unknown>[] = [];
      for (const path of [...new Set([...before.keys(), ...after.keys()])].sort()) {
        const b = before.get(path);
        const a = after.get(path);
        if (b?.oid === a?.oid) continue;
        files.push({ filename: path, status: b === undefined ? 'added' : a === undefined ? 'removed' : 'modified', sha: (a ?? b).oid });
      }
      return ok({ ...s.commit(repo, sha), files, stats: { total: files.length, additions: 0, deletions: 0 } });
    });

    // contents
    const readContents = (req: RestRequest, m: string[], s: Shapes): RestReply => {
      const repo = this.repo(m);
      const path = m[2] === undefined ? '' : decode(m[2]).replace(/^\/+|\/+$/g, '');
      if (repo.refs.size === 0) throw new ApiError(404, 'This repository is empty.');
      const ref = req.query.get('ref') ?? repo.defaultBranch;
      const commit = forge.resolve(repo, ref);
      if (commit === null) throw new ApiError(404, `No commit found for the ref ${ref}`);
      const found = forge.lookupPath(repo, commit, path);
      if (found === null) notFound();
      if (found.kind === 'file') return ok(s.contentFile(repo, path, found.oid, true));
      return ok(found.entries.map((entry) => s.contentDirEntry(repo, path === '' ? entry.name : `${path}/${entry.name}`, entry.mode, entry.oid)));
    };
    this.on('GET', '/repos/:owner/:repo/contents', readContents);
    this.on('GET', '/repos/:owner/:repo/contents/*path', readContents);
    this.on('GET', '/repos/:owner/:repo/readme', (req, m, s) => {
      const repo = this.repo(m);
      const commit = forge.resolve(repo, req.query.get('ref') ?? repo.defaultBranch);
      if (commit === null) notFound();
      const root = forge.lookupPath(repo, commit, '');
      const entry = root?.kind === 'dir' ? root.entries.find((candidate) => /^readme(\.|$)/i.test(candidate.name) && candidate.mode !== '40000') : undefined;
      if (entry === undefined) notFound();
      return ok(s.contentFile(repo, entry.name, entry.oid, true));
    });
    this.on('PUT', '/repos/:owner/:repo/contents/*path', (req, m, s) => {
      const repo = this.repo(m);
      const body = objectBody(req);
      const path = decode(m[2] ?? '');
      if (typeof body.message !== 'string' || body.message === '') throw new ApiError(422, 'Invalid request.\n\n"message" wasn\'t supplied.');
      if (typeof body.content !== 'string') throw new ApiError(422, 'Invalid request.\n\n"content" wasn\'t supplied.');
      const content = Buffer.from(body.content.replace(/\s+/g, ''), 'base64');
      const result = forge.writeFile(repo, {
        path,
        content,
        message: body.message,
        branch: typeof body.branch === 'string' && body.branch !== '' ? body.branch : undefined,
        sha: typeof body.sha === 'string' ? body.sha : undefined,
      });
      return ok(s.contentWrite(repo, path.replace(/^\/+|\/+$/g, ''), result.blob, result.commit), result.created ? 201 : 200);
    });
    this.on('DELETE', '/repos/:owner/:repo/contents/*path', (req, m, s) => {
      const repo = this.repo(m);
      const body = objectBody(req);
      const path = decode(m[2] ?? '');
      if (typeof body.message !== 'string' || body.message === '') throw new ApiError(422, 'Invalid request.\n\n"message" wasn\'t supplied.');
      const result = forge.deleteFile(repo, {
        path,
        message: body.message,
        branch: typeof body.branch === 'string' && body.branch !== '' ? body.branch : undefined,
        sha: typeof body.sha === 'string' ? body.sha : undefined,
      });
      return ok(s.contentWrite(repo, null, null, result.commit));
    });

    // labels
    this.on('GET', '/repos/:owner/:repo/labels', (req, m, s) => {
      const repo = this.repo(m);
      return paged(req, [...repo.labels.values()].sort((a, b) => a.id - b.id), (label) => s.label(repo, label));
    });
    this.on('POST', '/repos/:owner/:repo/labels', (req, m, s) => {
      const repo = this.repo(m);
      const body = objectBody(req);
      return ok(s.label(repo, forge.createLabel(repo, { name: body.name, color: body.color, description: body.description })), 201);
    });
    this.on('GET', '/repos/:owner/:repo/labels/:name', (_req, m, s) => {
      const repo = this.repo(m);
      const label = repo.labels.get(decode(m[2] ?? '').toLowerCase());
      if (label === undefined) notFound();
      return ok(s.label(repo, label));
    });

    // issues
    this.on('GET', '/repos/:owner/:repo/issues', (req, m, s) => {
      const repo = this.repo(m);
      const labels = req.query.get('labels');
      const issues = forge.listIssues(repo, {
        state: req.query.get('state') ?? 'open',
        sort: req.query.get('sort') ?? 'created',
        direction: req.query.get('direction') ?? 'desc',
        labels: labels === null || labels === '' ? undefined : labels.split(','),
      });
      return paged(req, issues, (issue) => s.issue(repo, issue));
    });
    this.on('POST', '/repos/:owner/:repo/issues', (req, m, s) => {
      const repo = this.repo(m);
      if (!repo.hasIssues) throw new ApiError(410, 'Issues are disabled for this repo');
      const body = objectBody(req);
      return ok(s.issue(repo, forge.createIssue(repo, { title: body.title, body: body.body, labels: body.labels })), 201);
    });
    this.on('GET', '/repos/:owner/:repo/issues/:number', (_req, m, s) => {
      const repo = this.repo(m);
      return ok(s.issue(repo, this.issue(repo, m[2])));
    });
    this.on('PATCH', '/repos/:owner/:repo/issues/:number', (req, m, s) => {
      const repo = this.repo(m);
      return ok(s.issue(repo, forge.updateIssue(repo, this.issue(repo, m[2]), objectBody(req))));
    });
    this.on('GET', '/repos/:owner/:repo/issues/:number/comments', (req, m, s) => {
      const repo = this.repo(m);
      const number = Number(m[2]);
      const target = repo.issues.get(number) ?? repo.pulls.get(number);
      if (target === undefined) notFound();
      return paged(req, target.comments, (comment) => s.comment(repo, number, comment));
    });
    this.on('POST', '/repos/:owner/:repo/issues/:number/comments', (req, m, s) => {
      const repo = this.repo(m);
      const number = Number(m[2]);
      const target = repo.issues.get(number) ?? repo.pulls.get(number);
      if (target === undefined) notFound();
      return ok(s.comment(repo, number, forge.addComment(repo, target, objectBody(req).body)), 201);
    });
    this.on('GET', '/repos/:owner/:repo/issues/:number/labels', (_req, m, s) => {
      const repo = this.repo(m);
      return ok(s.issue(repo, this.issue(repo, m[2])).labels);
    });
    this.on('POST', '/repos/:owner/:repo/issues/:number/labels', (req, m, s) => {
      const repo = this.repo(m);
      const issue = this.issue(repo, m[2]);
      const body = req.body;
      const wanted = Array.isArray(body) ? body : objectBody(req).labels;
      const added = forge.labelNames(repo, wanted);
      forge.updateIssue(repo, issue, { labels: [...new Set([...issue.labels, ...added])] });
      return ok(s.issue(repo, issue).labels);
    });

    // pull requests
    this.on('GET', '/repos/:owner/:repo/pulls', (req, m, s) => {
      const repo = this.repo(m);
      const state = req.query.get('state') ?? 'open';
      const head = req.query.get('head');
      const base = req.query.get('base');
      let pulls = [...repo.pulls.values()];
      if (state === 'open' || state === 'closed') pulls = pulls.filter((pull) => pull.state === state);
      if (head !== null && head !== '') {
        const branch = head.includes(':') ? head.slice(head.indexOf(':') + 1) : head;
        pulls = pulls.filter((pull) => pull.head === branch);
      }
      if (base !== null && base !== '') pulls = pulls.filter((pull) => pull.base === base);
      const ascending = req.query.get('direction') === 'asc';
      pulls.sort((a, b) => (ascending ? a.number - b.number : b.number - a.number));
      return paged(req, pulls, (pull) => s.pull(repo, pull, false));
    });
    this.on('POST', '/repos/:owner/:repo/pulls', (req, m, s) => {
      const repo = this.repo(m);
      const body = objectBody(req);
      return ok(s.pull(repo, forge.createPull(repo, { title: body.title, head: body.head, base: body.base, body: body.body })), 201);
    });
    this.on('GET', '/repos/:owner/:repo/pulls/:number', (_req, m, s) => {
      const repo = this.repo(m);
      return ok(s.pull(repo, this.pull(repo, m[2])));
    });
    this.on('PATCH', '/repos/:owner/:repo/pulls/:number', (req, m, s) => {
      const repo = this.repo(m);
      return ok(s.pull(repo, forge.updatePull(repo, this.pull(repo, m[2]), objectBody(req))));
    });
    this.on('GET', '/repos/:owner/:repo/pulls/:number/files', (req, m, s) => {
      const repo = this.repo(m);
      const pull = this.pull(repo, m[2]);
      return paged(req, forge.pullFiles(repo, pull), (file) => ({
        sha: file.sha,
        filename: file.filename,
        status: file.status,
        additions: file.additions,
        deletions: file.deletions,
        changes: file.additions + file.deletions,
        blob_url: `${s.origin}/${forge.user.login}/${repo.name}/blob/${file.sha}/${file.filename}`,
        raw_url: `${s.origin}/${forge.user.login}/${repo.name}/raw/${file.sha}/${file.filename}`,
        contents_url: `${s.api}/repos/${forge.user.login}/${repo.name}/contents/${file.filename}`,
        patch: file.patch,
      }));
    });
    this.on('GET', '/repos/:owner/:repo/pulls/:number/commits', (req, m, s) => {
      const repo = this.repo(m);
      return paged(req, forge.pullCommits(repo, this.pull(repo, m[2])), (oid) => s.commit(repo, oid));
    });
    this.on('GET', '/repos/:owner/:repo/pulls/:number/merge', (_req, m) => {
      const repo = this.repo(m);
      if (!this.pull(repo, m[2]).merged) notFound();
      return ok(null, 204);
    });
    this.on('PUT', '/repos/:owner/:repo/pulls/:number/merge', (req, m) => {
      const repo = this.repo(m);
      const pull = this.pull(repo, m[2]);
      const body = objectBody(req);
      const result = forge.mergePull(repo, pull, { method: body.merge_method, title: body.commit_title, message: body.commit_message, sha: body.sha });
      return ok({ sha: result.sha, merged: true, message: result.message });
    });
  }
}
