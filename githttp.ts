// git smart HTTP: GET /{owner}/{repo}.git/info/refs, POST .../git-upload-pack (protocol v2: ls-refs and
// fetch; protocol v0 as a fallback) and POST .../git-receive-pack (protocol v0 with report-status,
// side-band-64k and delete-refs). Packs are built from the repository's own object store.

import {
  DELIM,
  FLUSH,
  ZERO_OID,
  isOid,
  lineOf,
  parseCommit,
  parseTree,
  pkt,
  readPack,
  readPackets,
  sideband,
  tagTarget,
  writePack,
  type GitObject,
  type Packet,
} from './git.ts';
import type { Forge, Repo } from './forge.ts';

export const AGENT = 'gitforge/0.1';

export interface GitReply {
  status: number;
  contentType: string;
  body: Buffer;
  headers?: Record<string, string>;
}

function reply(status: number, contentType: string, body: Buffer | string): GitReply {
  return { status, contentType, body: typeof body === 'string' ? Buffer.from(body, 'utf8') : body, headers: { 'cache-control': 'no-cache' } };
}

function plain(status: number, text: string): GitReply {
  return reply(status, 'text/plain; charset=utf-8', `${text}\n`);
}

/** Refs in advertisement order: HEAD first (when it resolves), then every ref sorted by name. */
function sortedRefs(repo: Repo): Array<[string, string]> {
  return [...repo.refs.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
}

/** Only what uploadPackV0 implements: no include-tag, deepen-since/-not/-relative or no-done. */
const UPLOAD_V0_CAPS = ['multi_ack_detailed', 'side-band-64k', 'thin-pack', 'ofs-delta', 'shallow', 'no-progress', 'allow-tip-sha1-in-want', 'allow-reachable-sha1-in-want'];
const RECEIVE_CAPS = ['report-status', 'delete-refs', 'side-band-64k', 'quiet', 'ofs-delta', 'push-options'];

/** The v0 ref advertisement lines (without the service preamble). */
function v0Advertisement(forge: Forge, repo: Repo, caps: string[]): Buffer[] {
  const lines: Buffer[] = [];
  const refs = sortedRefs(repo);
  const headRef = forge.headRef(repo);
  const headOid = repo.refs.get(headRef);
  const allCaps = [...caps, `symref=HEAD:${headRef}`, 'object-format=sha1', `agent=${AGENT}`].join(' ');
  let first = true;
  const emit = (oid: string, name: string): void => {
    lines.push(pkt(first ? `${oid} ${name}\0${allCaps}\n` : `${oid} ${name}\n`));
    first = false;
  };
  if (headOid !== undefined) emit(headOid, 'HEAD');
  for (const [name, oid] of refs) {
    emit(oid, name);
    if (name.startsWith('refs/tags/')) {
      const peeled = forge.peel(repo, oid);
      if (peeled !== oid) lines.push(pkt(`${peeled} ${name}^{}\n`));
    }
  }
  if (first) emit(ZERO_OID, 'capabilities^{}');
  return lines;
}

/** GET info/refs for either service. */
export function infoRefs(forge: Forge, repo: Repo, service: string, gitProtocol: string): GitReply {
  if (service === 'git-upload-pack') {
    const type = 'application/x-git-upload-pack-advertisement';
    if (/(^|:)version=2(:|$)/.test(gitProtocol)) {
      const body = Buffer.concat([
        pkt('version 2\n'),
        pkt(`agent=${AGENT}\n`),
        pkt('ls-refs=unborn\n'),
        pkt('fetch=shallow\n'),
        pkt('server-option\n'),
        pkt('object-format=sha1\n'),
        FLUSH,
      ]);
      return reply(200, type, body);
    }
    const body = Buffer.concat([pkt('# service=git-upload-pack\n'), FLUSH, ...v0Advertisement(forge, repo, UPLOAD_V0_CAPS), FLUSH]);
    return reply(200, type, body);
  }
  if (service === 'git-receive-pack') {
    const body = Buffer.concat([pkt('# service=git-receive-pack\n'), FLUSH, ...v0Advertisement(forge, repo, RECEIVE_CAPS), FLUSH]);
    return reply(200, 'application/x-git-receive-pack-advertisement', body);
  }
  return plain(403, 'smart HTTP serves git-upload-pack and git-receive-pack only');
}

// ------------------------------------------------------------------ object selection for a fetch

interface PackPlan {
  oids: string[];
  shallow: string[];
  unshallow: string[];
}

/**
 * Which objects a fetch sends. `haves` (and everything they reach, stopping at the client's own shallow
 * commits) are already on the client. With `depth`, the commit walk from the wants stops after `depth`
 * generations; the commits at the cut that have parents become the new shallow boundary, and a client
 * shallow commit whose parents are now sent is reported unshallow.
 */
function planPack(forge: Forge, repo: Repo, wants: readonly string[], haves: readonly string[], clientShallow: ReadonlySet<string>, depth: number | null): PackPlan {
  const have = new Set<string>();
  const haveStack: string[] = [];
  for (const oid of haves) if (repo.objects.has(oid)) haveStack.push(oid);
  while (haveStack.length > 0) {
    const oid = haveStack.pop() as string;
    if (have.has(oid)) continue;
    const object = repo.get(oid);
    if (object === undefined) continue;
    have.add(oid);
    if (object.type === 'commit') {
      const commit = parseCommit(object.data);
      haveStack.push(commit.tree);
      if (!clientShallow.has(oid)) for (const parent of commit.parents) haveStack.push(parent);
    } else if (object.type === 'tree') {
      for (const entry of parseTree(object.data)) if (entry.mode !== '160000') haveStack.push(entry.oid);
    } else if (object.type === 'tag') {
      haveStack.push(tagTarget(object.data).object);
    }
  }

  // The commits to send, walking from the wants (tags peeled to their commits).
  const commits: string[] = [];
  const extra: string[] = [];
  const commitDepth = new Map<string, number>();
  const shallow = new Set<string>();
  const queue: Array<[string, number]> = [];
  for (const want of wants) {
    let oid = want;
    for (let hop = 0; hop < 10; hop += 1) {
      const object = repo.get(oid);
      if (object === undefined || object.type !== 'tag') break;
      if (!have.has(oid)) extra.push(oid);
      oid = tagTarget(object.data).object;
    }
    const object = repo.get(oid);
    if (object === undefined) continue;
    if (object.type === 'commit') queue.push([oid, 1]);
    else if (!have.has(oid)) extra.push(oid);
  }
  while (queue.length > 0) {
    const [oid, level] = queue.shift() as [string, number];
    const known = commitDepth.get(oid);
    if (known !== undefined && known <= level) continue;
    const firstVisit = known === undefined;
    commitDepth.set(oid, level);
    if (depth === null && have.has(oid)) continue;
    const commit = forge.commitOf(repo, oid);
    if (commit === null) continue;
    if (firstVisit && !have.has(oid)) commits.push(oid);
    if (depth !== null && level >= depth) {
      if (commit.parents.length > 0) shallow.add(oid);
      continue;
    }
    shallow.delete(oid);
    for (const parent of commit.parents) queue.push([parent, level + 1]);
  }

  // A shallow boundary the client already holds is not news; one whose parents now arrive is unshallowed.
  const unshallow: string[] = [];
  if (depth !== null) {
    for (const oid of clientShallow) {
      const level = commitDepth.get(oid);
      if (level !== undefined && level < depth && (forge.commitOf(repo, oid)?.parents.length ?? 0) > 0) unshallow.push(oid);
    }
  }
  const newShallow = [...shallow].filter((oid) => !clientShallow.has(oid));

  // Trees and blobs of every commit sent, minus what the client has.
  const send: string[] = [];
  const queued = new Set<string>();
  const add = (oid: string): void => {
    if (queued.has(oid) || have.has(oid) || !repo.objects.has(oid)) return;
    queued.add(oid);
    send.push(oid);
  };
  for (const oid of extra) add(oid);
  for (const oid of commits) add(oid);
  const treeStack: string[] = [];
  for (const oid of commits) treeStack.push((forge.commitOf(repo, oid) as { tree: string }).tree);
  while (treeStack.length > 0) {
    const oid = treeStack.pop() as string;
    if (queued.has(oid) || have.has(oid)) continue;
    const object = repo.get(oid);
    if (object === undefined) continue;
    add(oid);
    if (object.type === 'tree') {
      for (const entry of parseTree(object.data)) {
        if (entry.mode === '160000') continue;
        if (entry.mode === '40000') treeStack.push(entry.oid);
        else add(entry.oid);
      }
    }
  }
  return { oids: send, shallow: newShallow, unshallow };
}

function buildPack(repo: Repo, oids: readonly string[]): Buffer {
  return writePack(oids.map((oid) => repo.entryFor(oid)));
}

// ------------------------------------------------------------------ upload-pack

interface FetchArgs {
  wants: string[];
  haves: string[];
  shallow: Set<string>;
  depth: number | null;
  done: boolean;
}

function invalidWant(repo: Repo, wants: readonly string[]): string | null {
  for (const want of wants) if (!repo.objects.has(want)) return want;
  return null;
}

function lsRefs(forge: Forge, repo: Repo, args: readonly string[]): Buffer {
  const peel = args.includes('peel');
  const symrefs = args.includes('symrefs');
  const unborn = args.includes('unborn');
  const prefixes = args.filter((arg) => arg.startsWith('ref-prefix ')).map((arg) => arg.slice('ref-prefix '.length));
  const keep = (name: string): boolean => prefixes.length === 0 || prefixes.some((prefix) => name.startsWith(prefix));
  const lines: Buffer[] = [];
  const headRef = forge.headRef(repo);
  if (keep('HEAD')) {
    const headOid = repo.refs.get(headRef);
    if (headOid !== undefined) lines.push(pkt(`${headOid} HEAD${symrefs ? ` symref-target:${headRef}` : ''}\n`));
    else if (unborn) lines.push(pkt(`unborn HEAD${symrefs ? ` symref-target:${headRef}` : ''}\n`));
  }
  for (const [name, oid] of sortedRefs(repo)) {
    if (!keep(name)) continue;
    let line = `${oid} ${name}`;
    if (peel) {
      const peeled = forge.peel(repo, oid);
      if (peeled !== oid) line += ` peeled:${peeled}`;
    }
    lines.push(pkt(`${line}\n`));
  }
  lines.push(FLUSH);
  return Buffer.concat(lines);
}

function fetchV2(forge: Forge, repo: Repo, args: readonly string[]): GitReply {
  const request: FetchArgs = { wants: [], haves: [], shallow: new Set(), depth: null, done: false };
  for (const arg of args) {
    if (arg.startsWith('want ')) request.wants.push(arg.slice(5).trim());
    else if (arg.startsWith('have ')) request.haves.push(arg.slice(5).trim());
    else if (arg.startsWith('shallow ')) request.shallow.add(arg.slice(8).trim());
    else if (arg.startsWith('deepen ')) request.depth = Math.max(1, Number.parseInt(arg.slice(7), 10) || 1);
    else if (arg === 'done') request.done = true;
  }
  const bad = request.wants.find((want) => !isOid(want)) ?? invalidWant(repo, request.wants);
  if (bad !== null && bad !== undefined) {
    return reply(200, 'application/x-git-upload-pack-result', Buffer.concat([pkt(`ERR upload-pack: not our ref ${bad}\n`), FLUSH]));
  }
  const parts: Buffer[] = [];
  const common = request.haves.filter((oid) => repo.objects.has(oid));
  if (!request.done) {
    parts.push(pkt('acknowledgments\n'));
    if (common.length === 0) parts.push(pkt('NAK\n'));
    for (const oid of common) parts.push(pkt(`ACK ${oid}\n`));
    if (request.wants.length === 0) {
      parts.push(FLUSH);
      return reply(200, 'application/x-git-upload-pack-result', Buffer.concat(parts));
    }
    parts.push(pkt('ready\n'), DELIM);
  }
  const plan = planPack(forge, repo, request.wants, common, request.shallow, request.depth);
  if (request.depth !== null || request.shallow.size > 0) {
    parts.push(pkt('shallow-info\n'));
    for (const oid of plan.shallow) parts.push(pkt(`shallow ${oid}\n`));
    for (const oid of plan.unshallow) parts.push(pkt(`unshallow ${oid}\n`));
    parts.push(DELIM);
  }
  parts.push(pkt('packfile\n'));
  parts.push(...sideband(1, buildPack(repo, plan.oids)));
  parts.push(FLUSH);
  return reply(200, 'application/x-git-upload-pack-result', Buffer.concat(parts));
}

function uploadPackV2(forge: Forge, repo: Repo, packets: readonly Packet[]): GitReply {
  let command = '';
  const args: string[] = [];
  let inArgs = false;
  for (const packet of packets) {
    if (packet.kind === 'flush') break;
    if (packet.kind === 'delim') {
      inArgs = true;
      continue;
    }
    if (packet.kind !== 'data') continue;
    const line = lineOf(packet);
    if (!inArgs) {
      if (line.startsWith('command=')) command = line.slice('command='.length);
    } else {
      args.push(line);
    }
  }
  if (command === 'ls-refs') return reply(200, 'application/x-git-upload-pack-result', lsRefs(forge, repo, args));
  if (command === 'fetch') return fetchV2(forge, repo, args);
  return reply(200, 'application/x-git-upload-pack-result', Buffer.concat([pkt(`ERR unknown command ${JSON.stringify(command)}\n`), FLUSH]));
}

function uploadPackV0(forge: Forge, repo: Repo, packets: readonly Packet[]): GitReply {
  const request: FetchArgs = { wants: [], haves: [], shallow: new Set(), depth: null, done: false };
  let caps = new Set<string>();
  for (const packet of packets) {
    if (packet.kind !== 'data') continue;
    const line = lineOf(packet);
    if (line.startsWith('want ')) {
      const rest = line.slice(5).split(' ');
      request.wants.push(rest[0] ?? '');
      if (rest.length > 1 && caps.size === 0) caps = new Set(rest.slice(1));
    } else if (line.startsWith('have ')) request.haves.push(line.slice(5).trim());
    else if (line.startsWith('shallow ')) request.shallow.add(line.slice(8).trim());
    else if (line.startsWith('deepen ')) request.depth = Math.max(1, Number.parseInt(line.slice(7), 10) || 1);
    else if (line === 'done') request.done = true;
  }
  const bad = request.wants.find((want) => !isOid(want)) ?? invalidWant(repo, request.wants);
  if (bad !== null && bad !== undefined) {
    return reply(200, 'application/x-git-upload-pack-result', Buffer.concat([pkt(`ERR upload-pack: not our ref ${bad}\n`), FLUSH]));
  }
  const parts: Buffer[] = [];
  const common = request.haves.filter((oid) => repo.objects.has(oid));
  const multiAck = caps.has('multi_ack_detailed') || caps.has('multi_ack');
  if (!request.done && request.haves.length > 0) {
    // A negotiation round of stateless v0 (no `done` yet): acknowledge what is common and send no pack;
    // the client sends `done` in a later round.
    if (multiAck) for (const oid of common) parts.push(pkt(`ACK ${oid} common\n`));
    parts.push(pkt('NAK\n'));
    return reply(200, 'application/x-git-upload-pack-result', Buffer.concat(parts));
  }
  const plan = planPack(forge, repo, request.wants, common, request.shallow, request.depth);
  if (request.depth !== null || request.shallow.size > 0) {
    for (const oid of plan.shallow) parts.push(pkt(`shallow ${oid}\n`));
    for (const oid of plan.unshallow) parts.push(pkt(`unshallow ${oid}\n`));
    parts.push(FLUSH);
  }
  if (common.length === 0) parts.push(pkt('NAK\n'));
  else if (multiAck) {
    for (const oid of common) parts.push(pkt(`ACK ${oid} common\n`));
    parts.push(pkt(`ACK ${common[common.length - 1] as string}\n`));
  } else parts.push(pkt(`ACK ${common[0] as string}\n`));
  const pack = buildPack(repo, plan.oids);
  if (caps.has('side-band-64k') || caps.has('side-band')) {
    parts.push(...sideband(1, pack), FLUSH);
  } else {
    parts.push(pack);
  }
  return reply(200, 'application/x-git-upload-pack-result', Buffer.concat(parts));
}

export function uploadPack(forge: Forge, repo: Repo, body: Buffer, gitProtocol: string): GitReply {
  let packets: Packet[];
  try {
    packets = readPackets(body).packets;
  } catch (err) {
    return plain(400, `malformed upload-pack request: ${err instanceof Error ? err.message : String(err)}`);
  }
  const first = packets.find((packet) => packet.kind === 'data');
  const v2 = /(^|:)version=2(:|$)/.test(gitProtocol) || (first !== undefined && lineOf(first).startsWith('command='));
  try {
    return v2 ? uploadPackV2(forge, repo, packets) : uploadPackV0(forge, repo, packets);
  } catch (err) {
    return reply(200, 'application/x-git-upload-pack-result', Buffer.concat([pkt(`ERR ${err instanceof Error ? err.message : String(err)}\n`), FLUSH]));
  }
}

// ------------------------------------------------------------------ receive-pack

interface Command {
  oldOid: string;
  newOid: string;
  ref: string;
}

function validRefName(ref: string): boolean {
  if (!ref.startsWith('refs/') || ref.endsWith('/') || ref.endsWith('.lock') || ref.endsWith('.')) return false;
  if (/\.\.|[\x00-\x20~^:?*[\\\x7f]|@\{|\/\/|\/\./.test(ref)) return false;
  return true;
}

/** Every object `tip` reaches is in the store (stopping at objects reachable from the existing refs). */
function connected(repo: Repo, tip: string, objects: ReadonlyMap<string, GitObject>): boolean {
  const known = (oid: string): GitObject | undefined => repo.get(oid) ?? objects.get(oid);
  const seen = new Set<string>();
  const stack = [tip];
  while (stack.length > 0) {
    const oid = stack.pop() as string;
    if (seen.has(oid)) continue;
    seen.add(oid);
    const object = known(oid);
    if (object === undefined) return false;
    // An object the repository already held was connected when it arrived.
    if (repo.objects.has(oid) && !objects.has(oid)) continue;
    if (object.type === 'commit') {
      const commit = parseCommit(object.data);
      stack.push(commit.tree, ...commit.parents);
    } else if (object.type === 'tree') {
      for (const entry of parseTree(object.data)) if (entry.mode !== '160000') stack.push(entry.oid);
    } else if (object.type === 'tag') {
      stack.push(tagTarget(object.data).object);
    }
  }
  return true;
}

export function receivePack(forge: Forge, repo: Repo, body: Buffer): GitReply {
  let packets: Packet[];
  let next: number;
  try {
    ({ packets, next } = readPackets(body, 0, true));
  } catch (err) {
    return plain(400, `malformed receive-pack request: ${err instanceof Error ? err.message : String(err)}`);
  }
  const commands: Command[] = [];
  let caps = new Set<string>();
  for (const packet of packets) {
    if (packet.kind !== 'data') continue;
    let line = lineOf(packet);
    const nul = line.indexOf('\0');
    if (nul >= 0) {
      if (commands.length === 0) caps = new Set(line.slice(nul + 1).split(' ').filter(Boolean));
      line = line.slice(0, nul);
    }
    if (line.startsWith('shallow ')) continue;
    const [oldOid, newOid, ref] = line.split(' ');
    if (!isOid(oldOid) || !isOid(newOid) || ref === undefined) return plain(400, `malformed receive-pack command ${JSON.stringify(line.slice(0, 120))}`);
    commands.push({ oldOid, newOid, ref });
  }
  const pack = body.subarray(next);
  let unpack = 'ok';
  let incoming = new Map<string, GitObject>();
  if (pack.length > 0) {
    try {
      incoming = readPack(pack, (oid) => repo.get(oid));
    } catch (err) {
      unpack = err instanceof Error ? err.message : String(err);
    }
  }

  const results: string[] = [];
  const createdBranches: string[] = [];
  if (unpack === 'ok') forge.putObjects(repo, incoming);
  for (const command of commands) {
    const { ref, oldOid, newOid } = command;
    if (unpack !== 'ok') {
      results.push(`ng ${ref} unpacker error`);
      continue;
    }
    if (!validRefName(ref)) {
      results.push(`ng ${ref} funny refname`);
      continue;
    }
    const current = repo.refs.get(ref);
    if (newOid === ZERO_OID) {
      if (current === undefined) results.push(`ng ${ref} reference does not exist`);
      else if (oldOid !== ZERO_OID && oldOid !== current) results.push(`ng ${ref} stale info`);
      else {
        forge.setRef(repo, ref, null);
        results.push(`ok ${ref}`);
      }
      continue;
    }
    if (!repo.objects.has(newOid)) {
      results.push(`ng ${ref} missing necessary objects`);
      continue;
    }
    if (!connected(repo, newOid, incoming)) {
      results.push(`ng ${ref} missing necessary objects`);
      continue;
    }
    if (current === undefined) {
      if (oldOid !== ZERO_OID) {
        results.push(`ng ${ref} stale info`);
        continue;
      }
      if (ref.startsWith('refs/heads/') && repo.get(newOid)?.type !== 'commit') {
        results.push(`ng ${ref} not a commit`);
        continue;
      }
      forge.setRef(repo, ref, newOid);
      if (ref.startsWith('refs/heads/')) createdBranches.push(ref.slice('refs/heads/'.length));
      results.push(`ok ${ref}`);
      continue;
    }
    if (oldOid !== current) {
      // The client's view of the ref is not the server's: refusing keeps a racing update from being lost.
      const fastForward = repo.get(newOid)?.type === 'commit' && forge.isAncestor(repo, current, newOid);
      results.push(fastForward ? `ng ${ref} stale info` : `ng ${ref} non-fast-forward`);
      continue;
    }
    const oldCommit = forge.peel(repo, current);
    const newCommit = forge.peel(repo, newOid);
    const bothCommits = repo.get(oldCommit)?.type === 'commit' && repo.get(newCommit)?.type === 'commit';
    if (ref.startsWith('refs/tags/') || !bothCommits || !forge.isAncestor(repo, oldCommit, newCommit)) {
      results.push(`ng ${ref} non-fast-forward`);
      continue;
    }
    forge.setRef(repo, ref, newOid);
    results.push(`ok ${ref}`);
  }
  forge.adoptDefaultBranch(repo, createdBranches);

  const reportStatus = caps.has('report-status') || caps.has('report-status-v2');
  if (!reportStatus) return reply(200, 'application/x-git-receive-pack-result', Buffer.alloc(0));
  const report = Buffer.concat([pkt(`unpack ${unpack === 'ok' ? 'ok' : unpack.replace(/\n/g, ' ')}\n`), ...results.map((line) => pkt(`${line}\n`)), FLUSH]);
  if (caps.has('side-band-64k') || caps.has('side-band')) {
    return reply(200, 'application/x-git-receive-pack-result', Buffer.concat([...sideband(1, report), FLUSH]));
  }
  return reply(200, 'application/x-git-receive-pack-result', report);
}
