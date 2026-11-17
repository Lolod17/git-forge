// The forge's domain layer: repositories, their git objects and refs, issues, labels, comments and pull
// requests, and the operations every surface (smart HTTP, REST, GraphQL, MCP) shares. One store, so an
// object made over one surface is the same object on every other.
//
// Persistence: everything lives in memory for speed and is written through to a data directory on disk.
//   <data>/journal.ndjson        one JSON record per metadata change (repo, ref, issue, pull, label, ...)
//   <data>/repos/<id>/objects    every git object the repository holds, appended as
//                                [20-byte oid][1-byte type code][4-byte length][raw bytes]
// A forge started on an existing data directory (FORGE_DATA_DIR) replays both and comes back as it was.

import { appendFileSync, closeSync, existsSync, mkdirSync, openSync, readFileSync, rmSync, writeSync } from 'node:fs';
import { join } from 'node:path';
import {
  ZERO_OID,
  TYPE_CODE,
  encodeCommit,
  encodeTree,
  hashObject,
  isOid,
  packEntry,
  parseCommit,
  parseTree,
  tagTarget,
  type GitObject,
  type ObjType,
  type Signature,
  type TreeEntry,
} from './git.ts';

const CODE_TYPE: Readonly<Record<number, ObjType>> = { 1: 'commit', 2: 'tree', 3: 'blob', 4: 'tag' };

/** An error a surface turns into its own failure shape: an HTTP status and a message. */
export class ApiError extends Error {
  readonly status: number;
  readonly errors: unknown[] | undefined;
  constructor(status: number, message: string, errors?: unknown[]) {
    super(message);
    this.status = status;
    this.errors = errors;
  }
}

export function nowIso(): string {
  return new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
}

export interface Label {
  id: number;
  name: string;
  color: string;
  description: string | null;
}

export interface Comment {
  id: number;
  body: string;
  createdAt: string;
  updatedAt: string;
}

export interface Issue {
  id: number;
  number: number;
  title: string;
  body: string;
  state: 'open' | 'closed';
  stateReason: string | null;
  labels: string[];
  comments: Comment[];
  createdAt: string;
  updatedAt: string;
  closedAt: string | null;
}

export interface Pull {
  id: number;
  number: number;
  title: string;
  body: string;
  state: 'open' | 'closed';
  head: string;
  base: string;
  headSha: string;
  baseSha: string;
  merged: boolean;
  mergedAt: string | null;
  mergeCommitSha: string | null;
  comments: Comment[];
  createdAt: string;
  updatedAt: string;
  closedAt: string | null;
}

export interface FileEntry {
  readonly mode: string;
  readonly oid: string;
}

export type Flat = Map<string, FileEntry>;

export class Repo {
  id: number;
  name: string;
  description: string | null;
  isPrivate: boolean;
  defaultBranch: string;
  hasIssues: boolean;
  createdAt: string;
  updatedAt: string;
  pushedAt: string | null;
  nextNumber: number;
  readonly objects = new Map<string, GitObject>();
  readonly refs = new Map<string, string>();
  readonly issues = new Map<number, Issue>();
  readonly pulls = new Map<number, Pull>();
  readonly labels = new Map<string, Label>();
  /** Each object's pack entry, built once: concurrent clones share them. */
  readonly packCache = new Map<string, Buffer>();
  readonly dir: string;
  deleted = false;

  constructor(id: number, name: string, dir: string) {
    this.id = id;
    this.name = name;
    this.dir = dir;
    this.description = null;
    this.isPrivate = false;
    this.defaultBranch = 'main';
    this.hasIssues = true;
    this.createdAt = nowIso();
    this.updatedAt = this.createdAt;
    this.pushedAt = null;
    this.nextNumber = 1;
  }

  get(oid: string): GitObject | undefined {
    return this.objects.get(oid);
  }

  /** The tip of a branch name, or undefined. */
  branchTip(branch: string): string | undefined {
    return this.refs.get(`refs/heads/${branch}`);
  }

  entryFor(oid: string): Buffer {
    let entry = this.packCache.get(oid);
    if (entry === undefined) {
      const object = this.objects.get(oid);
      if (object === undefined) throw new Error(`object ${oid} is missing`);
      entry = packEntry(object);
      this.packCache.set(oid, entry);
    }
    return entry;
  }
}

export interface User {
  readonly login: string;
  readonly id: number;
  readonly name: string;
  readonly email: string;
}

/** Commit metadata the forge writes for contents, merge and squash commits. */
function signatureOf(user: User, time: number): Signature {
  return { name: user.name, email: user.email, time, tz: '+0000' };
}

export class Forge {
  readonly user: User;
  readonly token: string | null;
  readonly dataDir: string;
  private readonly repos = new Map<string, Repo>();
  private readonly reposById = new Map<number, Repo>();
  private nextRepoId = 1001;
  private nextIssueId = 1;
  private nextCommentId = 1;
  private nextLabelId = 1;
  private nextPullId = 1;
  private journalFd: number;
  private loading = false;
  /** mergeability by `${baseTip}:${headTip}`: computing it reads both trees. */
  private readonly mergeableCache = new Map<string, boolean>();

  constructor(options: { login: string; token: string | null; dataDir: string }) {
    this.user = { login: options.login, id: 1, name: options.login, email: `${options.login}@users.noreply.gitforge.local` };
    this.token = options.token;
    this.dataDir = options.dataDir;
    mkdirSync(join(this.dataDir, 'repos'), { recursive: true });
    const journal = join(this.dataDir, 'journal.ndjson');
    if (existsSync(journal)) this.replay(journal);
    this.journalFd = openSync(journal, 'a');
  }

  close(): void {
    try {
      closeSync(this.journalFd);
    } catch {
      // already closed
    }
  }

  // ---------------------------------------------------------------- persistence

  private record(entry: Record<string, unknown>): void {
    if (this.loading) return;
    writeSync(this.journalFd, `${JSON.stringify(entry)}\n`);
  }

  private persistObjects(repo: Repo, objects: ReadonlyArray<readonly [string, GitObject]>): void {
    if (this.loading || objects.length === 0) return;
    const parts: Buffer[] = [];
    for (const [oid, object] of objects) {
      const header = Buffer.alloc(25);
      header.write(oid, 0, 'hex');
      header.writeUInt8(TYPE_CODE[object.type], 20);
      header.writeUInt32BE(object.data.length, 21);
      parts.push(header, object.data);
    }
    appendFileSync(join(repo.dir, 'objects'), Buffer.concat(parts));
  }

  private snapshotRepo(repo: Repo): void {
    this.record({
      t: 'repo',
      id: repo.id,
      name: repo.name,
      description: repo.description,
      private: repo.isPrivate,
      defaultBranch: repo.defaultBranch,
      hasIssues: repo.hasIssues,
      createdAt: repo.createdAt,
      updatedAt: repo.updatedAt,
      pushedAt: repo.pushedAt,
      nextNumber: repo.nextNumber,
    });
  }

  private replay(path: string): void {
    this.loading = true;
    try {
      for (const line of readFileSync(path, 'utf8').split('\n')) {
        if (line.trim() === '') continue;
        let entry: Record<string, unknown>;
        try {
          entry = JSON.parse(line) as Record<string, unknown>;
        } catch {
          continue; // a torn last line from a crash
        }
        this.apply(entry);
      }
      for (const repo of this.reposById.values()) this.loadObjects(repo);
    } finally {
      this.loading = false;
    }
  }

  private apply(entry: Record<string, unknown>): void {
    const repo = typeof entry.repo === 'number' ? this.reposById.get(entry.repo) : undefined;
    switch (entry.t) {
      case 'repo': {
        const id = entry.id as number;
        let target = this.reposById.get(id);
        if (target === undefined) {
          target = new Repo(id, entry.name as string, join(this.dataDir, 'repos', String(id)));
          this.reposById.set(id, target);
        } else {
          this.repos.delete(target.name.toLowerCase());
        }
        target.name = entry.name as string;
        target.description = (entry.description as string | null) ?? null;
        target.isPrivate = entry.private === true;
        target.defaultBranch = entry.defaultBranch as string;
        target.hasIssues = entry.hasIssues !== false;
        target.createdAt = entry.createdAt as string;
        target.updatedAt = entry.updatedAt as string;
        target.pushedAt = (entry.pushedAt as string | null) ?? null;
        target.nextNumber = Math.max(target.nextNumber, entry.nextNumber as number);
        this.repos.set(target.name.toLowerCase(), target);
        this.nextRepoId = Math.max(this.nextRepoId, id + 1);
        break;
      }
      case 'del': {
        const target = this.reposById.get(entry.id as number);
        if (target !== undefined) {
          this.reposById.delete(target.id);
          if (this.repos.get(target.name.toLowerCase()) === target) this.repos.delete(target.name.toLowerCase());
        }
        break;
      }
      case 'ref':
        if (repo !== undefined) {
          if (typeof entry.oid === 'string') repo.refs.set(entry.ref as string, entry.oid);
          else repo.refs.delete(entry.ref as string);
        }
        break;
      case 'issue':
        if (repo !== undefined) {
          const issue = entry.issue as Issue;
          repo.issues.set(issue.number, issue);
          repo.nextNumber = Math.max(repo.nextNumber, issue.number + 1);
          this.nextIssueId = Math.max(this.nextIssueId, issue.id + 1);
          for (const comment of issue.comments) this.nextCommentId = Math.max(this.nextCommentId, comment.id + 1);
        }
        break;
      case 'pull':
        if (repo !== undefined) {
          const pull = entry.pull as Pull;
          repo.pulls.set(pull.number, pull);
          repo.nextNumber = Math.max(repo.nextNumber, pull.number + 1);
          this.nextPullId = Math.max(this.nextPullId, pull.id + 1);
          for (const comment of pull.comments) this.nextCommentId = Math.max(this.nextCommentId, comment.id + 1);
        }
        break;
      case 'label':
        if (repo !== undefined) {
          const label = entry.label as Label;
          repo.labels.set(label.name.toLowerCase(), label);
          this.nextLabelId = Math.max(this.nextLabelId, label.id + 1);
        }
        break;
      default:
        break;
    }
  }

  private loadObjects(repo: Repo): void {
    const file = join(repo.dir, 'objects');
    if (!existsSync(file)) return;
    const bytes = readFileSync(file);
    let pos = 0;
    while (pos + 25 <= bytes.length) {
      const oid = bytes.toString('hex', pos, pos + 20);
      const type = CODE_TYPE[bytes.readUInt8(pos + 20)];
      const length = bytes.readUInt32BE(pos + 21);
      if (type === undefined || pos + 25 + length > bytes.length) break;
      repo.objects.set(oid, { type, data: Buffer.from(bytes.subarray(pos + 25, pos + 25 + length)) });
      pos += 25 + length;
    }
  }

  // ---------------------------------------------------------------- auth and repositories

  /** Whether `owner` is the token's login (GitHub logins compare case-insensitively). */
  isViewer(owner: string): boolean {
    return owner.toLowerCase() === this.user.login.toLowerCase();
  }

  listRepos(): Repo[] {
    return [...this.reposById.values()].filter((repo) => !repo.deleted).sort((a, b) => a.id - b.id);
  }

  repoByName(owner: string, name: string): Repo | null {
    if (!this.isViewer(owner)) return null;
    return this.repos.get(name.toLowerCase()) ?? null;
  }

  repoById(id: number): Repo | null {
    return this.reposById.get(id) ?? null;
  }

  createRepo(input: { name: unknown; description?: unknown; private?: unknown; hasIssues?: unknown }): Repo {
    const name = typeof input.name === 'string' ? input.name.trim() : '';
    if (name === '' || !/^[A-Za-z0-9._-]{1,100}$/.test(name) || name === '.' || name === '..' || name.toLowerCase().endsWith('.git')) {
      throw new ApiError(422, 'Repository creation failed.', [{ resource: 'Repository', code: 'custom', field: 'name', message: 'name is invalid' }]);
    }
    if (this.repos.has(name.toLowerCase())) {
      throw new ApiError(422, 'Repository creation failed.', [{ resource: 'Repository', code: 'custom', field: 'name', message: 'name already exists on this account' }]);
    }
    const id = this.nextRepoId++;
    const repo = new Repo(id, name, join(this.dataDir, 'repos', String(id)));
    mkdirSync(repo.dir, { recursive: true });
    repo.description = typeof input.description === 'string' ? input.description : null;
    repo.isPrivate = input.private === true;
    repo.hasIssues = input.hasIssues !== false;
    this.repos.set(name.toLowerCase(), repo);
    this.reposById.set(id, repo);
    this.snapshotRepo(repo);
    return repo;
  }

  updateRepo(repo: Repo, patch: Record<string, unknown>): Repo {
    if (patch.name !== undefined) {
      const name = typeof patch.name === 'string' ? patch.name.trim() : '';
      if (!/^[A-Za-z0-9._-]{1,100}$/.test(name)) throw new ApiError(422, 'Validation Failed', [{ resource: 'Repository', code: 'invalid', field: 'name' }]);
      const clash = this.repos.get(name.toLowerCase());
      if (clash !== undefined && clash !== repo) throw new ApiError(422, 'Validation Failed', [{ resource: 'Repository', code: 'custom', field: 'name', message: 'name already exists on this account' }]);
      this.repos.delete(repo.name.toLowerCase());
      repo.name = name;
      this.repos.set(name.toLowerCase(), repo);
    }
    if (patch.description !== undefined) repo.description = typeof patch.description === 'string' ? patch.description : null;
    if (typeof patch.private === 'boolean') repo.isPrivate = patch.private;
    if (patch.visibility === 'private' || patch.visibility === 'public') repo.isPrivate = patch.visibility === 'private';
    if (typeof patch.has_issues === 'boolean') repo.hasIssues = patch.has_issues;
    if (patch.default_branch !== undefined) {
      const branch = String(patch.default_branch);
      if (repo.refs.size > 0 && repo.branchTip(branch) === undefined) {
        throw new ApiError(422, 'Validation Failed', [{ resource: 'Repository', code: 'invalid', field: 'default_branch' }]);
      }
      repo.defaultBranch = branch;
    }
    repo.updatedAt = nowIso();
    this.snapshotRepo(repo);
    return repo;
  }

  deleteRepo(repo: Repo): void {
    repo.deleted = true;
    this.repos.delete(repo.name.toLowerCase());
    this.reposById.delete(repo.id);
    this.record({ t: 'del', id: repo.id });
    try {
      rmSync(repo.dir, { recursive: true, force: true });
    } catch {
      // the directory is gone or busy; the journal already says the repository is deleted
    }
  }

  // ---------------------------------------------------------------- objects and refs

  putObject(repo: Repo, type: ObjType, data: Buffer): string {
    const oid = hashObject(type, data);
    if (!repo.objects.has(oid)) {
      const object = { type, data };
      repo.objects.set(oid, object);
      this.persistObjects(repo, [[oid, object]]);
    }
    return oid;
  }

  putObjects(repo: Repo, objects: ReadonlyMap<string, GitObject>): void {
    const fresh: Array<[string, GitObject]> = [];
    for (const [oid, object] of objects) {
      if (repo.objects.has(oid)) continue;
      repo.objects.set(oid, object);
      fresh.push([oid, object]);
    }
    this.persistObjects(repo, fresh);
  }

  setRef(repo: Repo, ref: string, oid: string | null): void {
    if (oid === null) repo.refs.delete(ref);
    else repo.refs.set(ref, oid);
    repo.pushedAt = nowIso();
    this.record({ t: 'ref', repo: repo.id, ref, oid });
  }

  /** HEAD's target ref: the default branch. */
  headRef(repo: Repo): string {
    return `refs/heads/${repo.defaultBranch}`;
  }

  /** After a push that created the first branch, a default branch that does not exist follows it. */
  adoptDefaultBranch(repo: Repo, createdBranches: readonly string[]): void {
    if (repo.branchTip(repo.defaultBranch) !== undefined) return;
    const first = createdBranches[0];
    if (first === undefined) return;
    repo.defaultBranch = first;
    this.snapshotRepo(repo);
  }

  commitOf(repo: Repo, oid: string): ReturnType<typeof parseCommit> | null {
    const object = repo.get(oid);
    if (object === undefined || object.type !== 'commit') return null;
    return parseCommit(object.data);
  }

  /** Peel a tag chain to the object it finally names. */
  peel(repo: Repo, oid: string): string {
    let current = oid;
    for (let depth = 0; depth < 10; depth += 1) {
      const object = repo.get(current);
      if (object === undefined || object.type !== 'tag') return current;
      current = tagTarget(object.data).object;
    }
    return current;
  }

  /** A branch name, a full ref, a tag name or a commit sha, as a commit id; null when it names nothing. */
  resolve(repo: Repo, refish: string): string | null {
    if (refish === '') return null;
    const candidates = refish.startsWith('refs/') ? [refish] : [`refs/heads/${refish}`, `refs/tags/${refish}`];
    for (const ref of candidates) {
      const oid = repo.refs.get(ref);
      if (oid !== undefined) return this.peel(repo, oid);
    }
    if (isOid(refish) && repo.get(refish)?.type === 'commit') return refish;
    if (/^[0-9a-f]{7,39}$/.test(refish)) {
      for (const [oid, object] of repo.objects) if (object.type === 'commit' && oid.startsWith(refish)) return oid;
    }
    return null;
  }

  /** Whether `ancestor` is reachable from `descendant` along parent links (a commit is its own ancestor). */
  isAncestor(repo: Repo, ancestor: string, descendant: string): boolean {
    if (ancestor === descendant) return true;
    const seen = new Set<string>();
    const queue = [descendant];
    while (queue.length > 0) {
      const oid = queue.pop() as string;
      if (oid === ancestor) return true;
      if (seen.has(oid)) continue;
      seen.add(oid);
      const commit = this.commitOf(repo, oid);
      if (commit !== null) for (const parent of commit.parents) if (!seen.has(parent)) queue.push(parent);
    }
    return false;
  }

  ancestors(repo: Repo, tip: string): Set<string> {
    const seen = new Set<string>();
    const queue = [tip];
    while (queue.length > 0) {
      const oid = queue.pop() as string;
      if (seen.has(oid)) continue;
      const commit = this.commitOf(repo, oid);
      if (commit === null) continue;
      seen.add(oid);
      for (const parent of commit.parents) if (!seen.has(parent)) queue.push(parent);
    }
    return seen;
  }

  /** The nearest common ancestor of two commits (breadth-first from `b`), or null for unrelated histories. */
  mergeBase(repo: Repo, a: string, b: string): string | null {
    const fromA = this.ancestors(repo, a);
    const seen = new Set<string>();
    const queue = [b];
    while (queue.length > 0) {
      const oid = queue.shift() as string;
      if (seen.has(oid)) continue;
      seen.add(oid);
      if (fromA.has(oid)) return oid;
      const commit = this.commitOf(repo, oid);
      if (commit !== null) for (const parent of commit.parents) queue.push(parent);
    }
    return null;
  }

  /**
   * Commits reachable from `tip`, newest first by committer time (ties by discovery order), excluding those
   * reachable from `stop`, at most `limit` after skipping `skip`.
   */
  log(repo: Repo, tip: string, options: { stop?: string | null; skip?: number; limit?: number } = {}): string[] {
    const excluded = options.stop ? this.ancestors(repo, options.stop) : new Set<string>();
    const all = [...this.ancestors(repo, tip)].filter((oid) => !excluded.has(oid));
    const times = new Map<string, number>();
    const order = new Map<string, number>();
    let index = 0;
    const visit = [tip];
    const seen = new Set<string>();
    while (visit.length > 0) {
      const oid = visit.shift() as string;
      if (seen.has(oid)) continue;
      seen.add(oid);
      order.set(oid, index++);
      const commit = this.commitOf(repo, oid);
      if (commit === null) continue;
      const time = Number(/ (\d+) [+-]\d{4}$/.exec(commit.committer)?.[1] ?? 0);
      times.set(oid, time);
      for (const parent of commit.parents) visit.push(parent);
    }
    all.sort((x, y) => (times.get(y) ?? 0) - (times.get(x) ?? 0) || (order.get(x) ?? 0) - (order.get(y) ?? 0));
    const skip = options.skip ?? 0;
    return all.slice(skip, options.limit === undefined ? undefined : skip + options.limit);
  }

  // ---------------------------------------------------------------- trees

  treeEntries(repo: Repo, oid: string): TreeEntry[] {
    const object = repo.get(oid);
    if (object === undefined || object.type !== 'tree') throw new Error(`tree ${oid} is missing`);
    return parseTree(object.data);
  }

  /** Every non-tree entry under a tree, by its full path. */
  flatten(repo: Repo, treeOid: string, prefix = '', out: Flat = new Map()): Flat {
    for (const entry of this.treeEntries(repo, treeOid)) {
      const path = prefix === '' ? entry.name : `${prefix}/${entry.name}`;
      if (entry.mode === '40000') this.flatten(repo, entry.oid, path, out);
      else out.set(path, { mode: entry.mode, oid: entry.oid });
    }
    return out;
  }

  /** Write the trees a flat path map describes and answer the root tree id. Throws on a file/dir clash. */
  buildTree(repo: Repo, flat: Flat): string {
    interface Node {
      files: Map<string, FileEntry>;
      dirs: Map<string, Node>;
    }
    const root: Node = { files: new Map(), dirs: new Map() };
    for (const [path, entry] of flat) {
      const segments = path.split('/');
      let node = root;
      for (let index = 0; index < segments.length - 1; index += 1) {
        const segment = segments[index] as string;
        if (node.files.has(segment)) throw new ApiError(409, `${segments.slice(0, index + 1).join('/')} is a file, not a directory`);
        let child = node.dirs.get(segment);
        if (child === undefined) {
          child = { files: new Map(), dirs: new Map() };
          node.dirs.set(segment, child);
        }
        node = child;
      }
      const leaf = segments[segments.length - 1] as string;
      if (node.dirs.has(leaf)) throw new ApiError(409, `${path} is a directory, not a file`);
      node.files.set(leaf, entry);
    }
    const write = (node: Node): string => {
      const entries: TreeEntry[] = [];
      for (const [name, entry] of node.files) entries.push({ mode: entry.mode, name, oid: entry.oid });
      for (const [name, child] of node.dirs) entries.push({ mode: '40000', name, oid: write(child) });
      return this.putObject(repo, 'tree', encodeTree(entries));
    };
    return write(root);
  }

  /** What a path names in a commit's tree: a file, a directory listing, or nothing. */
  lookupPath(repo: Repo, commitOid: string, path: string): { kind: 'file'; mode: string; oid: string } | { kind: 'dir'; oid: string; entries: TreeEntry[] } | null {
    const commit = this.commitOf(repo, commitOid);
    if (commit === null) return null;
    let treeOid = commit.tree;
    const segments = path.split('/').filter((segment) => segment !== '');
    if (segments.length === 0) return { kind: 'dir', oid: treeOid, entries: this.treeEntries(repo, treeOid) };
    for (let index = 0; index < segments.length; index += 1) {
      const entry = this.treeEntries(repo, treeOid).find((candidate) => candidate.name === segments[index]);
      if (entry === undefined) return null;
      if (index === segments.length - 1) {
        if (entry.mode === '40000') return { kind: 'dir', oid: entry.oid, entries: this.treeEntries(repo, entry.oid) };
        return { kind: 'file', mode: entry.mode, oid: entry.oid };
      }
      if (entry.mode !== '40000') return null;
      treeOid = entry.oid;
    }
    return null;
  }

  // ---------------------------------------------------------------- commits the forge writes

  makeCommit(repo: Repo, tree: string, parents: readonly string[], message: string, author?: Signature): string {
    const time = Math.floor(Date.now() / 1000);
    const committer = signatureOf(this.user, time);
    return this.putObject(repo, 'commit', encodeCommit({ tree, parents, author: author ?? committer, committer, message }));
  }

  /** PUT /contents: create or update one file on a branch with a commit. */
  writeFile(repo: Repo, input: { path: string; content: Buffer; message: string; branch?: string; sha?: string }): { created: boolean; blob: string; commit: string } {
    const branch = input.branch ?? repo.defaultBranch;
    const path = normalizePath(input.path);
    const tip = repo.branchTip(branch);
    if (tip === undefined && repo.refs.size > 0) throw new ApiError(404, `Branch ${branch} not found`);
    const flat: Flat = tip === undefined ? new Map() : this.flatten(repo, (this.commitOf(repo, tip) as { tree: string }).tree);
    const existing = flat.get(path);
    if (existing !== undefined) {
      if (input.sha === undefined || input.sha === '') throw new ApiError(422, 'Invalid request.\n\n"sha" wasn\'t supplied.');
      if (input.sha !== existing.oid) throw new ApiError(409, `${path} does not match ${input.sha}`);
    }
    const blob = this.putObject(repo, 'blob', input.content);
    flat.set(path, { mode: existing?.mode ?? '100644', oid: blob });
    const tree = this.buildTree(repo, flat);
    const commit = this.makeCommit(repo, tree, tip === undefined ? [] : [tip], input.message);
    this.setRef(repo, `refs/heads/${branch}`, commit);
    if (tip === undefined) this.adoptDefaultBranch(repo, [branch]);
    return { created: existing === undefined, blob, commit };
  }

  /** DELETE /contents: remove one file from a branch with a commit. */
  deleteFile(repo: Repo, input: { path: string; message: string; branch?: string; sha?: string }): { commit: string } {
    const branch = input.branch ?? repo.defaultBranch;
    const path = normalizePath(input.path);
    const tip = repo.branchTip(branch);
    if (tip === undefined) throw new ApiError(404, 'Not Found');
    const flat = this.flatten(repo, (this.commitOf(repo, tip) as { tree: string }).tree);
    const existing = flat.get(path);
    if (existing === undefined) throw new ApiError(404, 'Not Found');
    if (input.sha === undefined || input.sha === '') throw new ApiError(422, 'Invalid request.\n\n"sha" wasn\'t supplied.');
    if (input.sha !== existing.oid) throw new ApiError(409, `${path} does not match ${input.sha}`);
    flat.delete(path);
    const tree = this.buildTree(repo, flat);
    const commit = this.makeCommit(repo, tree, [tip], input.message);
    this.setRef(repo, `refs/heads/${branch}`, commit);
    return { commit };
  }

  /** A three-way merge of flat trees; null when a path changed differently on both sides. */
  merge3(base: Flat, ours: Flat, theirs: Flat): Flat | null {
    const out: Flat = new Map();
    const paths = new Set<string>([...base.keys(), ...ours.keys(), ...theirs.keys()]);
    const same = (x: FileEntry | undefined, y: FileEntry | undefined): boolean =>
      (x === undefined && y === undefined) || (x !== undefined && y !== undefined && x.oid === y.oid && x.mode === y.mode);
    for (const path of paths) {
      const b = base.get(path);
      const o = ours.get(path);
      const t = theirs.get(path);
      let pick: FileEntry | undefined;
      if (same(o, t)) pick = o;
      else if (same(o, b)) pick = t;
      else if (same(t, b)) pick = o;
      else return null;
      if (pick !== undefined) out.set(path, pick);
    }
    return out;
  }

  private treeOfCommit(repo: Repo, oid: string | null): Flat {
    if (oid === null) return new Map();
    const commit = this.commitOf(repo, oid);
    return commit === null ? new Map() : this.flatten(repo, commit.tree);
  }

  /** Merge `head` into `baseTip`: the merged flat tree, or null on a conflict. */
  mergedTree(repo: Repo, baseTip: string, head: string): Flat | null {
    const mergeBase = this.mergeBase(repo, baseTip, head);
    if (mergeBase === baseTip) return this.treeOfCommit(repo, head);
    return this.merge3(this.treeOfCommit(repo, mergeBase), this.treeOfCommit(repo, baseTip), this.treeOfCommit(repo, head));
  }

  // ---------------------------------------------------------------- labels and issues

  createLabel(repo: Repo, input: { name: unknown; color?: unknown; description?: unknown }): Label {
    const name = typeof input.name === 'string' ? input.name.trim() : '';
    if (name === '') throw new ApiError(422, 'Validation Failed', [{ resource: 'Label', code: 'missing_field', field: 'name' }]);
    if (repo.labels.has(name.toLowerCase())) throw new ApiError(422, 'Validation Failed', [{ resource: 'Label', code: 'already_exists', field: 'name' }]);
    const color = typeof input.color === 'string' && /^#?[0-9a-fA-F]{6}$/.test(input.color) ? input.color.replace(/^#/, '').toLowerCase() : 'ededed';
    const label: Label = { id: this.nextLabelId++, name, color, description: typeof input.description === 'string' ? input.description : null };
    repo.labels.set(name.toLowerCase(), label);
    this.record({ t: 'label', repo: repo.id, label });
    return label;
  }

  /** Label names from a request's `labels` (strings or {name} objects), each of which must exist. */
  labelNames(repo: Repo, value: unknown): string[] {
    if (value === undefined || value === null) return [];
    if (!Array.isArray(value)) throw new ApiError(422, 'Validation Failed', [{ resource: 'Issue', code: 'invalid', field: 'labels' }]);
    const names: string[] = [];
    for (const item of value) {
      const name = typeof item === 'string' ? item : typeof item === 'object' && item !== null && typeof (item as { name?: unknown }).name === 'string' ? (item as { name: string }).name : null;
      if (name === null) throw new ApiError(422, 'Validation Failed', [{ resource: 'Issue', code: 'invalid', field: 'labels' }]);
      const label = repo.labels.get(name.toLowerCase());
      if (label === undefined) {
        throw new ApiError(422, `Validation Failed: the label ${JSON.stringify(name)} does not exist in this repository`, [{ resource: 'Label', code: 'invalid', field: 'labels', value: name }]);
      }
      if (!names.includes(label.name)) names.push(label.name);
    }
    return names;
  }

  private saveIssue(repo: Repo, issue: Issue): void {
    this.record({ t: 'issue', repo: repo.id, issue });
  }

  /** The next issue/pull number. Not journaled on its own: replay derives it from the highest number seen. */
  private allocateNumber(repo: Repo): number {
    return repo.nextNumber++;
  }

  createIssue(repo: Repo, input: { title: unknown; body?: unknown; labels?: unknown }): Issue {
    if (typeof input.title !== 'string' && typeof input.title !== 'number') {
      throw new ApiError(422, 'Validation Failed', [{ resource: 'Issue', code: 'missing_field', field: 'title' }]);
    }
    const title = String(input.title);
    if (title.trim() === '') throw new ApiError(422, 'Validation Failed', [{ resource: 'Issue', code: 'missing_field', field: 'title' }]);
    const labels = this.labelNames(repo, input.labels);
    const now = nowIso();
    const issue: Issue = {
      id: this.nextIssueId++,
      number: this.allocateNumber(repo),
      title,
      body: typeof input.body === 'string' ? input.body : '',
      state: 'open',
      stateReason: null,
      labels,
      comments: [],
      createdAt: now,
      updatedAt: now,
      closedAt: null,
    };
    repo.issues.set(issue.number, issue);
    this.saveIssue(repo, issue);
    return issue;
  }

  updateIssue(repo: Repo, issue: Issue, patch: Record<string, unknown>): Issue {
    if (patch.title !== undefined) {
      if (typeof patch.title !== 'string' || patch.title.trim() === '') throw new ApiError(422, 'Validation Failed', [{ resource: 'Issue', code: 'invalid', field: 'title' }]);
      issue.title = patch.title;
    }
    if (patch.body !== undefined) issue.body = typeof patch.body === 'string' ? patch.body : '';
    if (patch.labels !== undefined) issue.labels = this.labelNames(repo, patch.labels);
    if (patch.state !== undefined) {
      if (patch.state !== 'open' && patch.state !== 'closed') throw new ApiError(422, 'Validation Failed', [{ resource: 'Issue', code: 'invalid', field: 'state' }]);
      if (patch.state !== issue.state) {
        issue.state = patch.state;
        issue.closedAt = patch.state === 'closed' ? nowIso() : null;
        issue.stateReason = patch.state === 'closed' ? (typeof patch.state_reason === 'string' ? patch.state_reason : 'completed') : 'reopened';
      }
    }
    issue.updatedAt = nowIso();
    this.saveIssue(repo, issue);
    return issue;
  }

  addComment(repo: Repo, target: Issue | Pull, body: unknown): Comment {
    if (typeof body !== 'string' || body === '') throw new ApiError(422, 'Validation Failed', [{ resource: 'IssueComment', code: 'missing_field', field: 'body' }]);
    const now = nowIso();
    const comment: Comment = { id: this.nextCommentId++, body, createdAt: now, updatedAt: now };
    target.comments.push(comment);
    target.updatedAt = now;
    if ('head' in target) this.savePull(repo, target);
    else this.saveIssue(repo, target);
    return comment;
  }

  /** Issues filtered and ordered as GitHub's list endpoints do. */
  listIssues(repo: Repo, options: { state?: string; sort?: string; direction?: string; labels?: string[] }): Issue[] {
    const state = options.state ?? 'open';
    let list = [...repo.issues.values()];
    if (state === 'open' || state === 'closed') list = list.filter((issue) => issue.state === state);
    if (options.labels !== undefined && options.labels.length > 0) {
      const wanted = options.labels.map((name) => name.toLowerCase());
      list = list.filter((issue) => wanted.every((name) => issue.labels.some((label) => label.toLowerCase() === name)));
    }
    const sort = options.sort ?? 'created';
    const key = (issue: Issue): string | number => (sort === 'updated' ? issue.updatedAt : sort === 'comments' ? issue.comments.length : issue.createdAt);
    const ascending = options.direction === 'asc';
    list.sort((a, b) => {
      const x = key(a);
      const y = key(b);
      const primary = x < y ? -1 : x > y ? 1 : 0;
      const order = primary !== 0 ? primary : a.number - b.number;
      return ascending ? order : -order;
    });
    return list;
  }

  // ---------------------------------------------------------------- pull requests

  private savePull(repo: Repo, pull: Pull): void {
    this.record({ t: 'pull', repo: repo.id, pull });
  }

  /** The live head and base tips of a pull request (the recorded ones once it is closed). */
  pullTips(repo: Repo, pull: Pull): { head: string; base: string } {
    if (pull.state === 'closed') return { head: pull.headSha, base: pull.baseSha };
    return { head: repo.branchTip(pull.head) ?? pull.headSha, base: repo.branchTip(pull.base) ?? pull.baseSha };
  }

  createPull(repo: Repo, input: { title: unknown; head: unknown; base: unknown; body?: unknown }): Pull {
    const headRaw = typeof input.head === 'string' ? input.head : '';
    const head = headRaw.includes(':') ? headRaw.slice(headRaw.indexOf(':') + 1) : headRaw;
    const base = typeof input.base === 'string' && input.base !== '' ? input.base : '';
    if (typeof input.title !== 'string' || input.title.trim() === '') throw new ApiError(422, 'Validation Failed', [{ resource: 'PullRequest', code: 'missing_field', field: 'title' }]);
    const headTip = repo.branchTip(head);
    const baseTip = repo.branchTip(base);
    if (head === '' || headTip === undefined) throw new ApiError(422, 'Validation Failed', [{ resource: 'PullRequest', code: 'invalid', field: 'head' }]);
    if (base === '' || baseTip === undefined) throw new ApiError(422, 'Validation Failed', [{ resource: 'PullRequest', code: 'invalid', field: 'base' }]);
    for (const other of repo.pulls.values()) {
      if (other.state === 'open' && other.head === head && other.base === base) {
        throw new ApiError(422, 'Validation Failed', [{ resource: 'PullRequest', code: 'custom', message: `A pull request already exists for ${this.user.login}:${head}.` }]);
      }
    }
    if (this.isAncestor(repo, headTip, baseTip)) {
      throw new ApiError(422, 'Validation Failed', [{ resource: 'PullRequest', code: 'custom', message: `No commits between ${base} and ${head}` }]);
    }
    const now = nowIso();
    const pull: Pull = {
      id: this.nextPullId++,
      number: this.allocateNumber(repo),
      title: input.title,
      body: typeof input.body === 'string' ? input.body : '',
      state: 'open',
      head,
      base,
      headSha: headTip,
      baseSha: baseTip,
      merged: false,
      mergedAt: null,
      mergeCommitSha: null,
      comments: [],
      createdAt: now,
      updatedAt: now,
      closedAt: null,
    };
    repo.pulls.set(pull.number, pull);
    this.savePull(repo, pull);
    return pull;
  }

  updatePull(repo: Repo, pull: Pull, patch: Record<string, unknown>): Pull {
    if (typeof patch.title === 'string' && patch.title.trim() !== '') pull.title = patch.title;
    if (patch.body !== undefined) pull.body = typeof patch.body === 'string' ? patch.body : '';
    if (typeof patch.base === 'string' && patch.base !== pull.base) {
      if (repo.branchTip(patch.base) === undefined) throw new ApiError(422, 'Validation Failed', [{ resource: 'PullRequest', code: 'invalid', field: 'base' }]);
      pull.base = patch.base;
    }
    if ((patch.state === 'closed' || patch.state === 'open') && !pull.merged) {
      if (patch.state === 'closed' && pull.state === 'open') {
        const tips = this.pullTips(repo, pull);
        pull.headSha = tips.head;
        pull.baseSha = tips.base;
        pull.closedAt = nowIso();
      }
      if (patch.state === 'open') pull.closedAt = null;
      pull.state = patch.state;
    }
    pull.updatedAt = nowIso();
    this.savePull(repo, pull);
    return pull;
  }

  /** Whether an open pull request merges cleanly into its base right now (merged ones answer null). */
  mergeable(repo: Repo, pull: Pull): boolean | null {
    if (pull.state !== 'open') return null;
    const tips = this.pullTips(repo, pull);
    const key = `${repo.id}:${tips.base}:${tips.head}`;
    let value = this.mergeableCache.get(key);
    if (value === undefined) {
      value = this.mergedTree(repo, tips.base, tips.head) !== null;
      this.mergeableCache.set(key, value);
    }
    return value;
  }

  /** The files a pull request changes: its head against the merge base with its base branch. */
  pullFiles(repo: Repo, pull: Pull): Array<{ filename: string; status: string; additions: number; deletions: number; sha: string; patch: string }> {
    const tips = this.pullTips(repo, pull);
    const mergeBase = pull.merged ? this.mergeBase(repo, pull.baseSha, tips.head) : this.mergeBase(repo, tips.base, tips.head);
    const before = this.treeOfCommit(repo, mergeBase);
    const after = this.treeOfCommit(repo, tips.head);
    const out: Array<{ filename: string; status: string; additions: number; deletions: number; sha: string; patch: string }> = [];
    const paths = [...new Set([...before.keys(), ...after.keys()])].sort();
    for (const path of paths) {
      const b = before.get(path);
      const a = after.get(path);
      if (b !== undefined && a !== undefined && b.oid === a.oid && b.mode === a.mode) continue;
      const oldLines = b === undefined ? [] : linesOf(repo.get(b.oid)?.data);
      const newLines = a === undefined ? [] : linesOf(repo.get(a.oid)?.data);
      const counts = diffCounts(oldLines, newLines);
      out.push({
        filename: path,
        status: b === undefined ? 'added' : a === undefined ? 'removed' : 'modified',
        additions: counts.additions,
        deletions: counts.deletions,
        sha: (a ?? b as FileEntry).oid,
        patch: `@@ -${oldLines.length === 0 ? 0 : 1},${oldLines.length} +${newLines.length === 0 ? 0 : 1},${newLines.length} @@`,
      });
    }
    return out;
  }

  /** The commits a pull request adds over its base, oldest first. */
  pullCommits(repo: Repo, pull: Pull): string[] {
    const tips = this.pullTips(repo, pull);
    return this.log(repo, tips.head, { stop: pull.merged ? pull.baseSha : tips.base }).reverse();
  }

  /** PUT /pulls/{n}/merge: merge, squash or rebase into the base branch. */
  mergePull(repo: Repo, pull: Pull, input: { method?: unknown; title?: unknown; message?: unknown; sha?: unknown }): { sha: string; message: string } {
    if (pull.merged || pull.state !== 'open') throw new ApiError(405, 'Pull Request is not mergeable');
    const method = typeof input.method === 'string' && input.method !== '' ? input.method.toLowerCase() : 'merge';
    if (method !== 'merge' && method !== 'squash' && method !== 'rebase') {
      throw new ApiError(422, 'Validation Failed', [{ resource: 'PullRequest', code: 'invalid', field: 'merge_method' }]);
    }
    const tips = this.pullTips(repo, pull);
    if (typeof input.sha === 'string' && input.sha !== '' && input.sha !== tips.head) throw new ApiError(409, 'Head branch was modified. Review and try the merge again.');
    const merged = this.mergedTree(repo, tips.base, tips.head);
    if (merged === null) throw new ApiError(405, 'Pull Request is not mergeable');
    const title = typeof input.title === 'string' && input.title !== '' ? input.title : method === 'merge' ? `Merge pull request #${pull.number} from ${this.user.login}/${pull.head}` : `${pull.title} (#${pull.number})`;
    const extra = typeof input.message === 'string' && input.message !== '' ? input.message : method === 'merge' ? pull.title : '';
    const message = extra === '' ? title : `${title}\n\n${extra}`;
    let sha: string;
    if (method === 'rebase') {
      sha = tips.base;
      for (const oid of this.log(repo, tips.head, { stop: tips.base }).reverse()) {
        const commit = this.commitOf(repo, oid) as ReturnType<typeof parseCommit>;
        const parent = commit.parents[0] ?? null;
        const replayed = this.merge3(this.treeOfCommit(repo, parent), this.treeOfCommit(repo, sha), this.treeOfCommit(repo, oid));
        if (replayed === null) throw new ApiError(405, 'Pull Request is not mergeable');
        const author = parseSignatureLine(commit.author);
        sha = this.makeCommit(repo, this.buildTree(repo, replayed), [sha], commit.message, author);
      }
    } else {
      const tree = this.buildTree(repo, merged);
      sha = this.makeCommit(repo, tree, method === 'merge' ? [tips.base, tips.head] : [tips.base], message);
    }
    this.setRef(repo, `refs/heads/${pull.base}`, sha);
    const now = nowIso();
    pull.state = 'closed';
    pull.merged = true;
    pull.mergedAt = now;
    pull.closedAt = now;
    pull.updatedAt = now;
    pull.mergeCommitSha = sha;
    pull.headSha = tips.head;
    pull.baseSha = tips.base;
    this.savePull(repo, pull);
    return { sha, message: 'Pull Request successfully merged' };
  }
}

function parseSignatureLine(text: string): Signature {
  const match = /^(.*) <([^>]*)> (\d+) ([+-]\d{4})$/.exec(text);
  if (match === null) return { name: text, email: '', time: Math.floor(Date.now() / 1000), tz: '+0000' };
  return { name: match[1] ?? '', email: match[2] ?? '', time: Number(match[3]), tz: match[4] ?? '+0000' };
}

/** A repository-relative path with no leading, trailing or doubled slashes; refuses `.` and `..`. */
export function normalizePath(path: string): string {
  const segments = path.split('/').filter((segment) => segment !== '');
  if (segments.length === 0 || segments.some((segment) => segment === '.' || segment === '..' || segment === '.git')) {
    throw new ApiError(422, 'Invalid request.\n\npath is invalid.');
  }
  return segments.join('/');
}

function linesOf(data: Buffer | undefined): string[] {
  if (data === undefined || data.length === 0) return [];
  const text = data.toString('utf8');
  const lines = text.split('\n');
  if (lines[lines.length - 1] === '') lines.pop();
  return lines;
}

/** Added and deleted line counts between two versions (an LCS on small files, a multiset count on big ones). */
function diffCounts(before: readonly string[], after: readonly string[]): { additions: number; deletions: number } {
  if (before.length * after.length <= 4_000_000) {
    const width = after.length + 1;
    let previous = new Uint32Array(width);
    let current = new Uint32Array(width);
    for (let i = 1; i <= before.length; i += 1) {
      for (let j = 1; j <= after.length; j += 1) {
        current[j] = before[i - 1] === after[j - 1] ? (previous[j - 1] as number) + 1 : Math.max(previous[j] as number, current[j - 1] as number);
      }
      [previous, current] = [current, previous];
    }
    const common = previous[after.length] as number;
    return { additions: after.length - common, deletions: before.length - common };
  }
  const counts = new Map<string, number>();
  for (const line of before) counts.set(line, (counts.get(line) ?? 0) + 1);
  let common = 0;
  for (const line of after) {
    const left = counts.get(line) ?? 0;
    if (left > 0) {
      common += 1;
      counts.set(line, left - 1);
    }
  }
  return { additions: after.length - common, deletions: before.length - common };
}

export { ZERO_OID };
