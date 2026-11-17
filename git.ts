// The git object layer, written from the format documentation: SHA-1 object ids, the tree/commit/tag
// encodings, pkt-line framing, and packfile reading (with OFS_DELTA and REF_DELTA) and writing. No git
// binary and no dependency: node:crypto for SHA-1 and node:zlib for the deflate streams.

import { createHash } from 'node:crypto';
import { deflateSync, inflateSync } from 'node:zlib';

export type ObjType = 'commit' | 'tree' | 'blob' | 'tag';

export interface GitObject {
  readonly type: ObjType;
  readonly data: Buffer;
}

export const ZERO_OID = '0'.repeat(40);
export const PKT_MAX_PAYLOAD = 65516;
/** A side-band-64k data packet carries one band byte, so its data is at most this many bytes. */
export const SIDEBAND_MAX_DATA = 65515;

export const TYPE_CODE: Readonly<Record<ObjType, number>> = { commit: 1, tree: 2, blob: 3, tag: 4 };
const CODE_TYPE: Readonly<Record<number, ObjType>> = { 1: 'commit', 2: 'tree', 3: 'blob', 4: 'tag' };

export function isOid(value: unknown): value is string {
  return typeof value === 'string' && /^[0-9a-f]{40}$/.test(value);
}

export function hashObject(type: ObjType, data: Buffer): string {
  const hash = createHash('sha1');
  hash.update(`${type} ${data.length}\0`);
  hash.update(data);
  return hash.digest('hex');
}

// ------------------------------------------------------------------ pkt-line

export function pkt(payload: string | Buffer): Buffer {
  const data = typeof payload === 'string' ? Buffer.from(payload, 'utf8') : payload;
  if (data.length > PKT_MAX_PAYLOAD) throw new Error(`pkt-line payload of ${data.length} bytes is too long`);
  const header = Buffer.from((data.length + 4).toString(16).padStart(4, '0'), 'ascii');
  return Buffer.concat([header, data]);
}

export const FLUSH = Buffer.from('0000', 'ascii');
export const DELIM = Buffer.from('0001', 'ascii');

export type Packet = { kind: 'data'; payload: Buffer } | { kind: 'flush' } | { kind: 'delim' } | { kind: 'end' };

export class PktError extends Error {}

/**
 * Read pkt-lines from `buf` starting at `offset` until `stop` says so or the buffer ends. Returns the packets
 * and the offset just past the last one read, so a caller can take the raw bytes that follow (a pushed pack).
 */
export function readPackets(buf: Buffer, offset = 0, stopAtFlush = false): { packets: Packet[]; next: number } {
  const packets: Packet[] = [];
  let pos = offset;
  while (pos < buf.length) {
    if (pos + 4 > buf.length) throw new PktError('truncated pkt-line header');
    const head = buf.toString('ascii', pos, pos + 4);
    if (!/^[0-9a-fA-F]{4}$/.test(head)) throw new PktError(`invalid pkt-line length ${JSON.stringify(head)}`);
    const length = parseInt(head, 16);
    if (length === 0) {
      packets.push({ kind: 'flush' });
      pos += 4;
      if (stopAtFlush) break;
      continue;
    }
    if (length === 1) {
      packets.push({ kind: 'delim' });
      pos += 4;
      continue;
    }
    if (length === 2) {
      packets.push({ kind: 'end' });
      pos += 4;
      continue;
    }
    if (length < 4) throw new PktError(`invalid pkt-line length ${length}`);
    if (pos + length > buf.length) throw new PktError('truncated pkt-line');
    packets.push({ kind: 'data', payload: buf.subarray(pos + 4, pos + length) });
    pos += length;
  }
  return { packets, next: pos };
}

/** The text of a data packet without its trailing newline. */
export function lineOf(packet: Packet): string {
  if (packet.kind !== 'data') return '';
  const text = packet.payload.toString('utf8');
  return text.endsWith('\n') ? text.slice(0, -1) : text;
}

/** Frame `data` as side-band packets on `band`, each at most SIDEBAND_MAX_DATA bytes of data. */
export function sideband(band: 1 | 2 | 3, data: Buffer): Buffer[] {
  const out: Buffer[] = [];
  for (let pos = 0; pos < data.length; pos += SIDEBAND_MAX_DATA) {
    const chunk = data.subarray(pos, Math.min(data.length, pos + SIDEBAND_MAX_DATA));
    out.push(pkt(Buffer.concat([Buffer.from([band]), chunk])));
  }
  return out;
}

// ------------------------------------------------------------------ trees, commits, tags

export interface TreeEntry {
  readonly mode: string;
  readonly name: string;
  readonly oid: string;
}

export function parseTree(data: Buffer): TreeEntry[] {
  const entries: TreeEntry[] = [];
  let pos = 0;
  while (pos < data.length) {
    const space = data.indexOf(0x20, pos);
    const nul = data.indexOf(0x00, space + 1);
    if (space < 0 || nul < 0 || nul + 21 > data.length) throw new Error('malformed tree object');
    entries.push({
      mode: data.toString('ascii', pos, space),
      name: data.toString('utf8', space + 1, nul),
      oid: data.toString('hex', nul + 1, nul + 21),
    });
    pos = nul + 21;
  }
  return entries;
}

/** git's tree order: byte order of the name, a directory compared as if its name ended in '/'. */
function treeSortKey(entry: TreeEntry): Buffer {
  return Buffer.from(entry.mode === '40000' ? `${entry.name}/` : entry.name, 'utf8');
}

export function encodeTree(entries: readonly TreeEntry[]): Buffer {
  const sorted = [...entries].sort((a, b) => Buffer.compare(treeSortKey(a), treeSortKey(b)));
  const parts: Buffer[] = [];
  for (const entry of sorted) {
    parts.push(Buffer.from(`${entry.mode} ${entry.name}\0`, 'utf8'), Buffer.from(entry.oid, 'hex'));
  }
  return Buffer.concat(parts);
}

export interface Commit {
  readonly tree: string;
  readonly parents: string[];
  readonly author: string;
  readonly committer: string;
  readonly message: string;
}

export function parseCommit(data: Buffer): Commit {
  const text = data.toString('utf8');
  const blank = text.indexOf('\n\n');
  const header = blank < 0 ? text : text.slice(0, blank);
  const message = blank < 0 ? '' : text.slice(blank + 2);
  let tree = '';
  const parents: string[] = [];
  let author = '';
  let committer = '';
  for (const line of header.split('\n')) {
    if (line.startsWith('tree ')) tree = line.slice(5);
    else if (line.startsWith('parent ')) parents.push(line.slice(7));
    else if (line.startsWith('author ')) author = line.slice(7);
    else if (line.startsWith('committer ')) committer = line.slice(10);
  }
  return { tree, parents, author, committer, message };
}

export interface Signature {
  readonly name: string;
  readonly email: string;
  /** Unix seconds. */
  readonly time: number;
  readonly tz: string;
}

export function parseSignature(text: string): Signature {
  const match = /^(.*) <([^>]*)> (\d+) ([+-]\d{4})$/.exec(text);
  if (match === null) return { name: text, email: '', time: 0, tz: '+0000' };
  return { name: match[1] ?? '', email: match[2] ?? '', time: Number(match[3]), tz: match[4] ?? '+0000' };
}

export function formatSignature(sig: Signature): string {
  return `${sig.name} <${sig.email}> ${sig.time} ${sig.tz}`;
}

export function encodeCommit(fields: { tree: string; parents: readonly string[]; author: Signature; committer: Signature; message: string }): Buffer {
  const lines = [`tree ${fields.tree}`];
  for (const parent of fields.parents) lines.push(`parent ${parent}`);
  lines.push(`author ${formatSignature(fields.author)}`, `committer ${formatSignature(fields.committer)}`);
  const message = fields.message.endsWith('\n') ? fields.message : `${fields.message}\n`;
  return Buffer.from(`${lines.join('\n')}\n\n${message}`, 'utf8');
}

/** The object an annotated tag points at (its `object` header). */
export function tagTarget(data: Buffer): { object: string; type: string } {
  const text = data.toString('utf8');
  const object = /^object ([0-9a-f]{40})$/m.exec(text)?.[1] ?? '';
  const type = /^type (\w+)$/m.exec(text)?.[1] ?? '';
  return { object, type };
}

// ------------------------------------------------------------------ packfiles

class MissingBase extends Error {
  constructor(oid: string) {
    super(`ref-delta base ${oid} is missing`);
  }
}

function inflateAt(buf: Buffer, offset: number): { data: Buffer; next: number } {
  const result = inflateSync(buf.subarray(offset), { info: true }) as unknown as { buffer: Buffer; engine: { bytesWritten: number } };
  return { data: result.buffer, next: offset + result.engine.bytesWritten };
}

function applyDelta(base: Buffer, delta: Buffer): Buffer {
  let pos = 0;
  const varint = (): number => {
    let value = 0;
    let shift = 0;
    for (;;) {
      if (pos >= delta.length) throw new Error('truncated delta header');
      const byte = delta[pos++] as number;
      value += (byte & 0x7f) * 2 ** shift;
      shift += 7;
      if ((byte & 0x80) === 0) return value;
    }
  };
  const sourceSize = varint();
  const targetSize = varint();
  if (sourceSize !== base.length) throw new Error(`delta base is ${base.length} bytes, the delta expects ${sourceSize}`);
  const out = Buffer.allocUnsafe(targetSize);
  let written = 0;
  while (pos < delta.length) {
    const op = delta[pos++] as number;
    if (op & 0x80) {
      let offset = 0;
      let size = 0;
      for (let bit = 0; bit < 4; bit += 1) if (op & (1 << bit)) offset += (delta[pos++] as number) * 2 ** (8 * bit);
      for (let bit = 0; bit < 3; bit += 1) if (op & (1 << (4 + bit))) size += (delta[pos++] as number) * 2 ** (8 * bit);
      if (size === 0) size = 0x10000;
      if (offset + size > base.length || written + size > targetSize) throw new Error('delta copy out of range');
      base.copy(out, written, offset, offset + size);
      written += size;
    } else if (op !== 0) {
      if (pos + op > delta.length || written + op > targetSize) throw new Error('delta insert out of range');
      delta.copy(out, written, pos, pos + op);
      pos += op;
      written += op;
    } else {
      throw new Error('delta opcode 0 is reserved');
    }
  }
  if (written !== targetSize) throw new Error('delta produced the wrong size');
  return out;
}

/**
 * Read a version 2 or 3 pack and resolve every object. `external` supplies a REF_DELTA base the pack leaves
 * out (a thin pack whose base the repository already holds). Every object's id is computed from its bytes.
 */
export function readPack(pack: Buffer, external: (oid: string) => GitObject | undefined): Map<string, GitObject> {
  if (pack.length < 32 || pack.toString('ascii', 0, 4) !== 'PACK') throw new Error('not a packfile');
  const version = pack.readUInt32BE(4);
  if (version !== 2 && version !== 3) throw new Error(`unsupported pack version ${version}`);
  const count = pack.readUInt32BE(8);
  const end = pack.length - 20;
  const trailer = pack.toString('hex', end);
  const computed = createHash('sha1').update(pack.subarray(0, end)).digest('hex');
  if (trailer !== computed) throw new Error('pack checksum mismatch');

  interface Raw {
    offset: number;
    code: number;
    data: Buffer;
    baseOffset?: number;
    baseOid?: string;
    resolved?: GitObject;
  }
  const raws: Raw[] = [];
  const byOffset = new Map<number, Raw>();
  let pos = 12;
  const body = pack.subarray(0, end);
  for (let index = 0; index < count; index += 1) {
    const start = pos;
    let byte = body[pos++] as number;
    const code = (byte >> 4) & 7;
    let size = byte & 0x0f;
    let shift = 4;
    while (byte & 0x80) {
      byte = body[pos++] as number;
      size += (byte & 0x7f) * 2 ** shift;
      shift += 7;
    }
    const raw: Raw = { offset: start, code, data: Buffer.alloc(0) };
    if (code === 6) {
      byte = body[pos++] as number;
      let value = byte & 0x7f;
      while (byte & 0x80) {
        byte = body[pos++] as number;
        value = (value + 1) * 128 + (byte & 0x7f);
      }
      raw.baseOffset = start - value;
    } else if (code === 7) {
      raw.baseOid = body.toString('hex', pos, pos + 20);
      pos += 20;
    } else if (CODE_TYPE[code] === undefined) {
      throw new Error(`pack entry ${index} has unknown type ${code}`);
    }
    const inflated = inflateAt(body, pos);
    if (inflated.data.length !== size) throw new Error(`pack entry ${index} inflated to the wrong size`);
    raw.data = inflated.data;
    pos = inflated.next;
    raws.push(raw);
    byOffset.set(start, raw);
  }
  if (pos !== end) throw new Error('pack has trailing bytes before its checksum');

  const objects = new Map<string, GitObject>();
  const resolving = new Set<Raw>();
  const resolve = (raw: Raw): GitObject => {
    if (raw.resolved !== undefined) return raw.resolved;
    if (resolving.has(raw)) throw new Error('delta cycle in pack');
    resolving.add(raw);
    let object: GitObject;
    if (raw.code === 6) {
      const base = byOffset.get(raw.baseOffset as number);
      if (base === undefined) throw new Error('ofs-delta base is not in the pack');
      const resolvedBase = resolve(base);
      object = { type: resolvedBase.type, data: applyDelta(resolvedBase.data, raw.data) };
    } else if (raw.code === 7) {
      const oid = raw.baseOid as string;
      const base = objects.get(oid) ?? external(oid);
      if (base === undefined) throw new MissingBase(oid);
      object = { type: base.type, data: applyDelta(base.data, raw.data) };
    } else {
      object = { type: CODE_TYPE[raw.code] as ObjType, data: raw.data };
    }
    raw.resolved = object;
    resolving.delete(raw);
    objects.set(hashObject(object.type, object.data), object);
    return object;
  };
  // Plain objects and offset deltas first; a REF_DELTA whose base is another delta in the pack is retried
  // until a pass makes no progress, so no base is searched for by hashing every candidate.
  let pending: Raw[] = [];
  for (const raw of raws) {
    if (raw.code === 6 || raw.code === 7) pending.push(raw);
    else resolve(raw);
  }
  while (pending.length > 0) {
    const retry: Raw[] = [];
    for (const raw of pending) {
      try {
        resolve(raw);
      } catch (err) {
        if (!(err instanceof MissingBase)) throw err;
        resolving.clear();
        retry.push(raw);
      }
    }
    if (retry.length === pending.length) throw new Error(`ref-delta base ${retry[0]?.baseOid ?? '?'} is missing`);
    pending = retry;
  }
  return objects;
}

/** One object's pack entry (type/size header + zlib stream), reusable across packs. */
export function packEntry(object: GitObject): Buffer {
  const bytes: number[] = [];
  let size = object.data.length;
  let first = (TYPE_CODE[object.type] << 4) | (size & 0x0f);
  size = Math.floor(size / 16);
  while (size > 0) {
    bytes.push(first | 0x80);
    first = size & 0x7f;
    size = Math.floor(size / 128);
  }
  bytes.push(first);
  return Buffer.concat([Buffer.from(bytes), deflateSync(object.data, { level: 1 })]);
}

/** A version 2 pack from prebuilt entries, with its SHA-1 trailer. */
export function writePack(entries: readonly Buffer[]): Buffer {
  const header = Buffer.alloc(12);
  header.write('PACK', 0, 'ascii');
  header.writeUInt32BE(2, 4);
  header.writeUInt32BE(entries.length, 8);
  const hash = createHash('sha1');
  hash.update(header);
  for (const entry of entries) hash.update(entry);
  return Buffer.concat([header, ...entries, hash.digest()]);
}
