// GraphQL v4 at /api/graphql: a document parser (operations, variables, aliases, fragments, inline
// fragments, @skip/@include), a typed schema that drives both validation and introspection, and an
// executor over the same forge the REST and git surfaces use. A document naming a field its type does not
// have is answered 200 with an `errors` array and no data, as GitHub does; an array body is a batch.

import { ApiError, type Comment, type Forge, type Issue, type Label, type Pull, type Repo } from './forge.ts';
import { parseSignature } from './git.ts';
import { nodeId, parseNodeId } from './rest.ts';

// ------------------------------------------------------------------ lexer and parser

interface Token {
  kind: 'punct' | 'name' | 'int' | 'float' | 'string' | 'eof';
  value: string;
  line: number;
  column: number;
}

export class GraphQLSyntaxError extends Error {
  readonly line: number;
  readonly column: number;
  constructor(message: string, line: number, column: number) {
    super(message);
    this.line = line;
    this.column = column;
  }
}

function lex(source: string): Token[] {
  const tokens: Token[] = [];
  let pos = 0;
  let line = 1;
  let lineStart = 0;
  const at = (): { line: number; column: number } => ({ line, column: pos - lineStart + 1 });
  while (pos < source.length) {
    const ch = source[pos] as string;
    if (ch === '\n') {
      pos += 1;
      line += 1;
      lineStart = pos;
      continue;
    }
    if (ch === ' ' || ch === '\t' || ch === '\r' || ch === ',' || ch === '﻿') {
      pos += 1;
      continue;
    }
    if (ch === '#') {
      while (pos < source.length && source[pos] !== '\n') pos += 1;
      continue;
    }
    const where = at();
    if (source.startsWith('...', pos)) {
      tokens.push({ kind: 'punct', value: '...', ...where });
      pos += 3;
      continue;
    }
    if ('!$&()):=@[]{}|'.includes(ch)) {
      tokens.push({ kind: 'punct', value: ch, ...where });
      pos += 1;
      continue;
    }
    if (/[A-Za-z_]/.test(ch)) {
      const start = pos;
      while (pos < source.length && /[A-Za-z0-9_]/.test(source[pos] as string)) pos += 1;
      tokens.push({ kind: 'name', value: source.slice(start, pos), ...where });
      continue;
    }
    if (/[-0-9]/.test(ch)) {
      const match = /^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?/.exec(source.slice(pos));
      if (match === null) throw new GraphQLSyntaxError(`Syntax Error: Invalid number, unexpected ${JSON.stringify(ch)}.`, where.line, where.column);
      tokens.push({ kind: match[2] !== undefined || match[3] !== undefined ? 'float' : 'int', value: match[0], ...where });
      pos += match[0].length;
      continue;
    }
    if (source.startsWith('"""', pos)) {
      const end = source.indexOf('"""', pos + 3);
      if (end < 0) throw new GraphQLSyntaxError('Syntax Error: Unterminated string.', where.line, where.column);
      const raw = source.slice(pos + 3, end).replace(/\\"""/g, '"""');
      for (const c of raw) if (c === '\n') line += 1;
      tokens.push({ kind: 'string', value: blockString(raw), ...where });
      pos = end + 3;
      continue;
    }
    if (ch === '"') {
      let value = '';
      pos += 1;
      for (;;) {
        if (pos >= source.length || source[pos] === '\n') throw new GraphQLSyntaxError('Syntax Error: Unterminated string.', where.line, where.column);
        const c = source[pos] as string;
        if (c === '"') {
          pos += 1;
          break;
        }
        if (c === '\\') {
          const next = source[pos + 1];
          const simple: Record<string, string> = { '"': '"', '\\': '\\', '/': '/', b: '\b', f: '\f', n: '\n', r: '\r', t: '\t' };
          if (next !== undefined && simple[next] !== undefined) {
            value += simple[next];
            pos += 2;
          } else if (next === 'u') {
            value += String.fromCharCode(Number.parseInt(source.slice(pos + 2, pos + 6), 16));
            pos += 6;
          } else {
            throw new GraphQLSyntaxError('Syntax Error: Invalid character escape sequence.', where.line, where.column);
          }
          continue;
        }
        value += c;
        pos += 1;
      }
      tokens.push({ kind: 'string', value, ...where });
      continue;
    }
    throw new GraphQLSyntaxError(`Syntax Error: Unexpected character: ${JSON.stringify(ch)}.`, where.line, where.column);
  }
  tokens.push({ kind: 'eof', value: '', line, column: pos - lineStart + 1 });
  return tokens;
}

function blockString(raw: string): string {
  const lines = raw.split(/\r\n|\n|\r/);
  let common: number | null = null;
  for (const text of lines.slice(1)) {
    const indent = text.length - text.trimStart().length;
    if (indent < text.length && (common === null || indent < common)) common = indent;
  }
  const out = lines.map((text, index) => (index === 0 || common === null ? text : text.slice(common)));
  while (out.length > 0 && (out[0] as string).trim() === '') out.shift();
  while (out.length > 0 && (out[out.length - 1] as string).trim() === '') out.pop();
  return out.join('\n');
}

type ValueNode =
  | { kind: 'var'; name: string }
  | { kind: 'int' | 'float' | 'string' | 'boolean' | 'enum'; value: string }
  | { kind: 'null' }
  | { kind: 'list'; values: ValueNode[] }
  | { kind: 'object'; fields: Array<{ name: string; value: ValueNode }> };

interface Directive {
  name: string;
  args: Array<{ name: string; value: ValueNode }>;
}

interface FieldNode {
  kind: 'field';
  alias: string | null;
  name: string;
  args: Array<{ name: string; value: ValueNode }>;
  directives: Directive[];
  selections: Selection[] | null;
  line: number;
  column: number;
}

interface SpreadNode {
  kind: 'spread';
  name: string;
  directives: Directive[];
  line: number;
  column: number;
}

interface InlineNode {
  kind: 'inline';
  typeCondition: string | null;
  directives: Directive[];
  selections: Selection[];
}

type Selection = FieldNode | SpreadNode | InlineNode;

interface OperationNode {
  kind: 'operation';
  operation: 'query' | 'mutation' | 'subscription';
  name: string | null;
  variables: Array<{ name: string; type: string; defaultValue: ValueNode | null }>;
  selections: Selection[];
}

interface FragmentNode {
  kind: 'fragment';
  name: string;
  typeCondition: string;
  selections: Selection[];
}

type Definition = OperationNode | FragmentNode;

class Parser {
  private readonly tokens: Token[];
  private pos = 0;

  constructor(source: string) {
    this.tokens = lex(source);
  }

  private peek(): Token {
    return this.tokens[this.pos] as Token;
  }

  private next(): Token {
    return this.tokens[this.pos++] as Token;
  }

  private fail(token: Token, expected: string): never {
    const what = token.kind === 'eof' ? '<EOF>' : JSON.stringify(token.value);
    throw new GraphQLSyntaxError(`Syntax Error: Expected ${expected}, found ${what}.`, token.line, token.column);
  }

  private isPunct(value: string): boolean {
    const token = this.peek();
    return token.kind === 'punct' && token.value === value;
  }

  private expect(value: string): Token {
    const token = this.next();
    if (token.kind !== 'punct' || token.value !== value) this.fail(token, JSON.stringify(value));
    return token;
  }

  private name(): string {
    const token = this.next();
    if (token.kind !== 'name') this.fail(token, 'Name');
    return token.value;
  }

  document(): Definition[] {
    const definitions: Definition[] = [];
    while (this.peek().kind !== 'eof') definitions.push(this.definition());
    if (definitions.length === 0) this.fail(this.peek(), 'a definition');
    return definitions;
  }

  private definition(): Definition {
    const token = this.peek();
    if (this.isPunct('{')) return { kind: 'operation', operation: 'query', name: null, variables: [], selections: this.selectionSet() };
    if (token.kind === 'name' && (token.value === 'query' || token.value === 'mutation' || token.value === 'subscription')) {
      this.next();
      const name = this.peek().kind === 'name' ? this.name() : null;
      const variables: OperationNode['variables'] = [];
      if (this.isPunct('(')) {
        this.next();
        while (!this.isPunct(')')) {
          this.expect('$');
          const varName = this.name();
          this.expect(':');
          const type = this.typeRef();
          let defaultValue: ValueNode | null = null;
          if (this.isPunct('=')) {
            this.next();
            defaultValue = this.value(true);
          }
          this.directives();
          variables.push({ name: varName, type, defaultValue });
        }
        this.expect(')');
      }
      this.directives();
      return { kind: 'operation', operation: token.value as OperationNode['operation'], name, variables, selections: this.selectionSet() };
    }
    if (token.kind === 'name' && token.value === 'fragment') {
      this.next();
      const name = this.name();
      const on = this.name();
      if (on !== 'on') this.fail(this.tokens[this.pos - 1] as Token, '"on"');
      const typeCondition = this.name();
      this.directives();
      return { kind: 'fragment', name, typeCondition, selections: this.selectionSet() };
    }
    return this.fail(token, 'a definition');
  }

  private typeRef(): string {
    let text: string;
    if (this.isPunct('[')) {
      this.next();
      text = `[${this.typeRef()}]`;
      this.expect(']');
    } else {
      text = this.name();
    }
    if (this.isPunct('!')) {
      this.next();
      text += '!';
    }
    return text;
  }

  private directives(): Directive[] {
    const out: Directive[] = [];
    while (this.isPunct('@')) {
      this.next();
      out.push({ name: this.name(), args: this.isPunct('(') ? this.arguments() : [] });
    }
    return out;
  }

  private arguments(): Array<{ name: string; value: ValueNode }> {
    this.expect('(');
    const args: Array<{ name: string; value: ValueNode }> = [];
    while (!this.isPunct(')')) {
      const name = this.name();
      this.expect(':');
      args.push({ name, value: this.value(false) });
    }
    this.expect(')');
    return args;
  }

  private value(isConst: boolean): ValueNode {
    const token = this.next();
    if (token.kind === 'punct' && token.value === '$' && !isConst) return { kind: 'var', name: this.name() };
    if (token.kind === 'int') return { kind: 'int', value: token.value };
    if (token.kind === 'float') return { kind: 'float', value: token.value };
    if (token.kind === 'string') return { kind: 'string', value: token.value };
    if (token.kind === 'name') {
      if (token.value === 'true' || token.value === 'false') return { kind: 'boolean', value: token.value };
      if (token.value === 'null') return { kind: 'null' };
      return { kind: 'enum', value: token.value };
    }
    if (token.kind === 'punct' && token.value === '[') {
      const values: ValueNode[] = [];
      while (!this.isPunct(']')) values.push(this.value(isConst));
      this.expect(']');
      return { kind: 'list', values };
    }
    if (token.kind === 'punct' && token.value === '{') {
      const fields: Array<{ name: string; value: ValueNode }> = [];
      while (!this.isPunct('}')) {
        const name = this.name();
        this.expect(':');
        fields.push({ name, value: this.value(isConst) });
      }
      this.expect('}');
      return { kind: 'object', fields };
    }
    return this.fail(token, 'a value');
  }

  private selectionSet(): Selection[] {
    this.expect('{');
    const selections: Selection[] = [];
    while (!this.isPunct('}')) selections.push(this.selection());
    this.expect('}');
    if (selections.length === 0) this.fail(this.peek(), 'a selection');
    return selections;
  }

  private selection(): Selection {
    if (this.isPunct('...')) {
      const start = this.next();
      const token = this.peek();
      if (token.kind === 'name' && token.value !== 'on') {
        const name = this.name();
        return { kind: 'spread', name, directives: this.directives(), line: start.line, column: start.column };
      }
      let typeCondition: string | null = null;
      if (token.kind === 'name' && token.value === 'on') {
        this.next();
        typeCondition = this.name();
      }
      const directives = this.directives();
      return { kind: 'inline', typeCondition, directives, selections: this.selectionSet() };
    }
    const first = this.peek();
    let name = this.name();
    let alias: string | null = null;
    if (this.isPunct(':')) {
      this.next();
      alias = name;
      name = this.name();
    }
    const args = this.isPunct('(') ? this.arguments() : [];
    const directives = this.directives();
    const selections = this.isPunct('{') ? this.selectionSet() : null;
    return { kind: 'field', alias, name, args, directives, selections, line: first.line, column: first.column };
  }
}

// ------------------------------------------------------------------ schema

interface Ctx {
  readonly forge: Forge;
  readonly origin: string;
}

type Resolver = (parent: any, args: Record<string, any>, ctx: Ctx) => unknown;

interface FieldDef {
  readonly type: string;
  readonly args?: Readonly<Record<string, string>>;
  readonly resolve?: Resolver;
  readonly description?: string;
}

interface TypeDef {
  readonly kind: 'OBJECT' | 'INTERFACE' | 'ENUM' | 'INPUT_OBJECT' | 'SCALAR';
  readonly description?: string;
  readonly fields?: Readonly<Record<string, FieldDef>>;
  readonly interfaces?: readonly string[];
  readonly values?: readonly string[];
  readonly inputFields?: Readonly<Record<string, string>>;
}

function f(type: string, resolve?: Resolver, args?: Record<string, string>): FieldDef {
  return { type, resolve, args };
}

const PAGE_ARGS = { first: 'Int', last: 'Int', after: 'String', before: 'String' } as const;

/** A Relay connection over `items` with first/after/last/before, opaque cursors and pageInfo. */
function connection(typename: string, items: readonly unknown[], args: Record<string, any>): Record<string, unknown> {
  const decodeCursor = (cursor: unknown): number | null => {
    if (typeof cursor !== 'string' || cursor === '') return null;
    const text = Buffer.from(cursor, 'base64').toString('utf8');
    const match = /^cursor:v2:(\d+)$/.exec(text);
    if (match === null) throw new ApiError(422, `\`${cursor}\` does not appear to be a valid cursor.`);
    return Number(match[1]);
  };
  let start = 0;
  let end = items.length;
  const after = decodeCursor(args.after);
  const before = decodeCursor(args.before);
  if (after !== null) start = Math.max(start, after + 1);
  if (before !== null) end = Math.min(end, before);
  if (typeof args.first === 'number') {
    if (args.first < 0) throw new ApiError(422, '`first` must be a non-negative integer');
    end = Math.min(end, start + args.first);
  }
  if (typeof args.last === 'number') {
    if (args.last < 0) throw new ApiError(422, '`last` must be a non-negative integer');
    start = Math.max(start, end - args.last);
  }
  const cursor = (index: number): string => Buffer.from(`cursor:v2:${index}`, 'utf8').toString('base64');
  const slice = items.slice(start, Math.max(start, end));
  return {
    __typename: typename,
    nodes: slice,
    edges: slice.map((node, offset) => ({ __typename: `${typename.replace(/Connection$/, '')}Edge`, node, cursor: cursor(start + offset) })),
    totalCount: items.length,
    pageInfo: {
      __typename: 'PageInfo',
      hasNextPage: end < items.length,
      hasPreviousPage: start > 0,
      startCursor: slice.length > 0 ? cursor(start) : null,
      endCursor: slice.length > 0 ? cursor(start + slice.length - 1) : null,
    },
  };
}

// The values the resolvers pass around: each names its GraphQL type and carries the domain object.
const userValue = (): Record<string, unknown> => ({ __typename: 'User' });
const repoValue = (repo: Repo): Record<string, unknown> => ({ __typename: 'Repository', repo });
const issueValue = (repo: Repo, issue: Issue): Record<string, unknown> => ({ __typename: 'Issue', repo, issue });
const pullValue = (repo: Repo, pull: Pull): Record<string, unknown> => ({ __typename: 'PullRequest', repo, pull });
const commentValue = (repo: Repo, number: number, comment: Comment): Record<string, unknown> => ({ __typename: 'IssueComment', repo, number, comment });
const labelValue = (repo: Repo, label: Label): Record<string, unknown> => ({ __typename: 'Label', repo, label });
const commitValue = (repo: Repo, oid: string): Record<string, unknown> => ({ __typename: 'Commit', repo, oid });
const refValue = (repo: Repo, name: string, oid: string): Record<string, unknown> => ({ __typename: 'Ref', repo, name, oid });

function issueStateEnum(issue: Issue): string {
  return issue.state === 'open' ? 'OPEN' : 'CLOSED';
}

function pullStateEnum(pull: Pull): string {
  return pull.merged ? 'MERGED' : pull.state === 'open' ? 'OPEN' : 'CLOSED';
}

function sortByOrder<T extends { number: number; createdAt: string; updatedAt: string; comments: Comment[] }>(items: T[], orderBy: unknown): T[] {
  const order = (orderBy ?? {}) as { field?: string; direction?: string };
  const field = order.field ?? 'CREATED_AT';
  const descending = order.direction === 'DESC';
  const key = (item: T): string | number => (field === 'UPDATED_AT' ? item.updatedAt : field === 'COMMENTS' ? item.comments.length : item.createdAt);
  return [...items].sort((a, b) => {
    const x = key(a);
    const y = key(b);
    const primary = x < y ? -1 : x > y ? 1 : 0;
    const value = primary !== 0 ? primary : a.number - b.number;
    return descending ? -value : value;
  });
}

function isoOfSignature(text: string): string {
  return new Date(parseSignature(text).time * 1000).toISOString().replace(/\.\d{3}Z$/, 'Z');
}

/** The repository a global id names (Repository ids only). */
function repoFromId(ctx: Ctx, id: unknown): Repo {
  const parsed = typeof id === 'string' ? parseNodeId(id) : null;
  const repo = parsed !== null && parsed.prefix === 'R' ? ctx.forge.repoById(Number(parsed.key)) : null;
  if (repo === null) throw new ApiError(404, `Could not resolve to a node with the global id of '${String(id)}'`);
  return repo;
}

/** Any object a global id names, as a resolver value, or null. */
function nodeFromId(ctx: Ctx, id: unknown): Record<string, unknown> | null {
  const parsed = typeof id === 'string' ? parseNodeId(id) : null;
  if (parsed === null) return null;
  const forge = ctx.forge;
  if (parsed.prefix === 'U') return Number(parsed.key) === forge.user.id ? userValue() : null;
  if (parsed.prefix === 'R') {
    const repo = forge.repoById(Number(parsed.key));
    return repo === null ? null : repoValue(repo);
  }
  const key = Number(parsed.key);
  for (const repo of forge.listRepos()) {
    if (parsed.prefix === 'I') for (const issue of repo.issues.values()) if (issue.id === key) return issueValue(repo, issue);
    if (parsed.prefix === 'PR') for (const pull of repo.pulls.values()) if (pull.id === key) return pullValue(repo, pull);
    if (parsed.prefix === 'IC') {
      for (const issue of repo.issues.values()) for (const comment of issue.comments) if (comment.id === key) return commentValue(repo, issue.number, comment);
      for (const pull of repo.pulls.values()) for (const comment of pull.comments) if (comment.id === key) return commentValue(repo, pull.number, comment);
    }
    if (parsed.prefix === 'LA') for (const label of repo.labels.values()) if (label.id === key) return labelValue(repo, label);
  }
  return null;
}

function requireNode(ctx: Ctx, id: unknown, typename: string): Record<string, any> {
  const node = nodeFromId(ctx, id);
  if (node === null || node.__typename !== typename) throw new ApiError(404, `Could not resolve to a node with the global id of '${String(id)}'`);
  return node;
}

const SCHEMA: Readonly<Record<string, TypeDef>> = {
  Query: {
    kind: 'OBJECT',
    fields: {
      viewer: f('User!', () => userValue()),
      user: f('User', (_p, a, ctx) => (ctx.forge.isViewer(String(a.login)) ? userValue() : null), { login: 'String!' }),
      repositoryOwner: f('User', (_p, a, ctx) => (ctx.forge.isViewer(String(a.login)) ? userValue() : null), { login: 'String!' }),
      repository: f(
        'Repository',
        (_p, a, ctx) => {
          const repo = ctx.forge.repoByName(String(a.owner), String(a.name));
          return repo === null ? null : repoValue(repo);
        },
        { owner: 'String!', name: 'String!', followRenames: 'Boolean' },
      ),
      node: f('Node', (_p, a, ctx) => nodeFromId(ctx, a.id), { id: 'ID!' }),
      nodes: f('[Node]!', (_p, a, ctx) => (Array.isArray(a.ids) ? a.ids.map((id: unknown) => nodeFromId(ctx, id)) : []), { ids: '[ID!]!' }),
      rateLimit: f('RateLimit', () => ({ __typename: 'RateLimit', limit: 5000, cost: 1, remaining: 4999, used: 1, resetAt: '1970-01-01T00:00:00Z', nodeCount: 0 })),
      __type: f('__Type', (_p, a) => typeValue(String(a.name)), { name: 'String!' }),
      __schema: f('__Schema!', () => ({ __typename: '__Schema' })),
    },
  },
  Mutation: {
    kind: 'OBJECT',
    fields: {
      createIssue: f(
        'CreateIssuePayload',
        (_p, a, ctx) => {
          const input = a.input ?? {};
          const repo = repoFromId(ctx, input.repositoryId);
          const labels: string[] = [];
          for (const id of Array.isArray(input.labelIds) ? input.labelIds : []) {
            const node = requireNode(ctx, id, 'Label');
            labels.push((node.label as Label).name);
          }
          const issue = ctx.forge.createIssue(repo, { title: input.title, body: input.body, labels });
          return { __typename: 'CreateIssuePayload', issue: issueValue(repo, issue), clientMutationId: input.clientMutationId ?? null };
        },
        { input: 'CreateIssueInput!' },
      ),
      closeIssue: f(
        'CloseIssuePayload',
        (_p, a, ctx) => {
          const input = a.input ?? {};
          const node = requireNode(ctx, input.issueId, 'Issue');
          const reason = typeof input.stateReason === 'string' ? input.stateReason.toLowerCase() : 'completed';
          ctx.forge.updateIssue(node.repo, node.issue, { state: 'closed', state_reason: reason });
          return { __typename: 'CloseIssuePayload', issue: node, clientMutationId: input.clientMutationId ?? null };
        },
        { input: 'CloseIssueInput!' },
      ),
      reopenIssue: f(
        'ReopenIssuePayload',
        (_p, a, ctx) => {
          const input = a.input ?? {};
          const node = requireNode(ctx, input.issueId, 'Issue');
          ctx.forge.updateIssue(node.repo, node.issue, { state: 'open' });
          return { __typename: 'ReopenIssuePayload', issue: node, clientMutationId: input.clientMutationId ?? null };
        },
        { input: 'ReopenIssueInput!' },
      ),
      updateIssue: f(
        'UpdateIssuePayload',
        (_p, a, ctx) => {
          const input = a.input ?? {};
          const node = requireNode(ctx, input.id, 'Issue');
          const patch: Record<string, unknown> = {};
          if (input.title !== undefined) patch.title = input.title;
          if (input.body !== undefined) patch.body = input.body;
          if (input.state === 'OPEN' || input.state === 'CLOSED') patch.state = input.state.toLowerCase();
          ctx.forge.updateIssue(node.repo, node.issue, patch);
          return { __typename: 'UpdateIssuePayload', issue: node, clientMutationId: input.clientMutationId ?? null };
        },
        { input: 'UpdateIssueInput!' },
      ),
      addComment: f(
        'AddCommentPayload',
        (_p, a, ctx) => {
          const input = a.input ?? {};
          const node = nodeFromId(ctx, input.subjectId) as Record<string, any> | null;
          if (node === null || (node.__typename !== 'Issue' && node.__typename !== 'PullRequest')) {
            throw new ApiError(404, `Could not resolve to a node with the global id of '${String(input.subjectId)}'`);
          }
          const target = (node.issue ?? node.pull) as Issue | Pull;
          const comment = ctx.forge.addComment(node.repo, target, input.body);
          const value = commentValue(node.repo, target.number, comment);
          return {
            __typename: 'AddCommentPayload',
            commentEdge: { __typename: 'IssueCommentEdge', node: value, cursor: Buffer.from(`cursor:v2:${target.comments.length - 1}`).toString('base64') },
            subject: node,
            clientMutationId: input.clientMutationId ?? null,
          };
        },
        { input: 'AddCommentInput!' },
      ),
      createPullRequest: f(
        'CreatePullRequestPayload',
        (_p, a, ctx) => {
          const input = a.input ?? {};
          const repo = repoFromId(ctx, input.repositoryId);
          const pull = ctx.forge.createPull(repo, { title: input.title, head: input.headRefName, base: input.baseRefName, body: input.body });
          return { __typename: 'CreatePullRequestPayload', pullRequest: pullValue(repo, pull), clientMutationId: input.clientMutationId ?? null };
        },
        { input: 'CreatePullRequestInput!' },
      ),
      mergePullRequest: f(
        'MergePullRequestPayload',
        (_p, a, ctx) => {
          const input = a.input ?? {};
          const node = requireNode(ctx, input.pullRequestId, 'PullRequest');
          ctx.forge.mergePull(node.repo, node.pull, {
            method: typeof input.mergeMethod === 'string' ? input.mergeMethod.toLowerCase() : 'merge',
            title: input.commitHeadline,
            message: input.commitBody,
            sha: input.expectedHeadOid,
          });
          return { __typename: 'MergePullRequestPayload', pullRequest: node, clientMutationId: input.clientMutationId ?? null };
        },
        { input: 'MergePullRequestInput!' },
      ),
      closePullRequest: f(
        'ClosePullRequestPayload',
        (_p, a, ctx) => {
          const input = a.input ?? {};
          const node = requireNode(ctx, input.pullRequestId, 'PullRequest');
          ctx.forge.updatePull(node.repo, node.pull, { state: 'closed' });
          return { __typename: 'ClosePullRequestPayload', pullRequest: node, clientMutationId: input.clientMutationId ?? null };
        },
        { input: 'ClosePullRequestInput!' },
      ),
      createRepository: f(
        'CreateRepositoryPayload',
        (_p, a, ctx) => {
          const input = a.input ?? {};
          const repo = ctx.forge.createRepo({ name: input.name, description: input.description, private: input.visibility === 'PRIVATE' });
          return { __typename: 'CreateRepositoryPayload', repository: repoValue(repo), clientMutationId: input.clientMutationId ?? null };
        },
        { input: 'CreateRepositoryInput!' },
      ),
    },
  },
  Node: { kind: 'INTERFACE', fields: { id: f('ID!') } },
  GitObject: { kind: 'INTERFACE', fields: { id: f('ID!'), oid: f('GitObjectID!'), abbreviatedOid: f('String!') } },
  User: {
    kind: 'OBJECT',
    interfaces: ['Node'],
    fields: {
      id: f('ID!', (_p, _a, ctx) => nodeId('U', ctx.forge.user.id)),
      databaseId: f('Int', (_p, _a, ctx) => ctx.forge.user.id),
      login: f('String!', (_p, _a, ctx) => ctx.forge.user.login),
      name: f('String', (_p, _a, ctx) => ctx.forge.user.name),
      email: f('String!', () => ''),
      url: f('URI!', (_p, _a, ctx) => `${ctx.origin}/${ctx.forge.user.login}`),
      avatarUrl: f('URI!', (_p, _a, ctx) => `${ctx.origin}/avatars/${ctx.forge.user.login}`, { size: 'Int' }),
    },
  },
  Repository: {
    kind: 'OBJECT',
    interfaces: ['Node'],
    fields: {
      id: f('ID!', (p) => nodeId('R', p.repo.id)),
      databaseId: f('Int', (p) => p.repo.id),
      name: f('String!', (p) => p.repo.name),
      nameWithOwner: f('String!', (p, _a, ctx) => `${ctx.forge.user.login}/${p.repo.name}`),
      owner: f('User!', () => userValue()),
      description: f('String', (p) => p.repo.description),
      url: f('URI!', (p, _a, ctx) => `${ctx.origin}/${ctx.forge.user.login}/${p.repo.name}`),
      sshUrl: f('String!', (p, _a, ctx) => `${ctx.origin}/${ctx.forge.user.login}/${p.repo.name}.git`),
      isPrivate: f('Boolean!', (p) => p.repo.isPrivate),
      visibility: f('RepositoryVisibility!', (p) => (p.repo.isPrivate ? 'PRIVATE' : 'PUBLIC')),
      isFork: f('Boolean!', () => false),
      isArchived: f('Boolean!', () => false),
      isEmpty: f('Boolean!', (p) => p.repo.refs.size === 0),
      createdAt: f('DateTime!', (p) => p.repo.createdAt),
      updatedAt: f('DateTime!', (p) => p.repo.updatedAt),
      pushedAt: f('DateTime', (p) => p.repo.pushedAt),
      stargazerCount: f('Int!', () => 0),
      forkCount: f('Int!', () => 0),
      viewerPermission: f('RepositoryPermission', () => 'ADMIN'),
      viewerCanAdminister: f('Boolean!', () => true),
      hasIssuesEnabled: f('Boolean!', (p) => p.repo.hasIssues),
      hasWikiEnabled: f('Boolean!', () => false),
      mergeCommitAllowed: f('Boolean!', () => true),
      squashMergeAllowed: f('Boolean!', () => true),
      rebaseMergeAllowed: f('Boolean!', () => true),
      defaultBranchRef: f('Ref', (p, _a, ctx) => {
        const repo = p.repo as Repo;
        const oid = repo.branchTip(repo.defaultBranch);
        return oid === undefined ? null : refValue(repo, ctx.forge.headRef(repo), oid);
      }),
      ref: f('Ref', (p, a) => {
        const repo = p.repo as Repo;
        const name = String(a.qualifiedName);
        const full = name.startsWith('refs/') ? name : `refs/heads/${name}`;
        const oid = repo.refs.get(full);
        return oid === undefined ? null : refValue(repo, full, oid);
      }, { qualifiedName: 'String!' }),
      refs: f('RefConnection', (p, a) => {
        const repo = p.repo as Repo;
        const prefix = String(a.refPrefix ?? 'refs/');
        const refs = [...repo.refs.entries()].filter(([name]) => name.startsWith(prefix)).sort(([x], [y]) => (x < y ? -1 : 1)).map(([name, oid]) => refValue(repo, name, oid));
        return connection('RefConnection', refs, a);
      }, { ...PAGE_ARGS, refPrefix: 'String!', query: 'String' }),
      issue: f('Issue', (p, a) => {
        const issue = (p.repo as Repo).issues.get(Number(a.number));
        return issue === undefined ? null : issueValue(p.repo, issue);
      }, { number: 'Int!' }),
      issueOrPullRequest: f('Node', (p, a) => {
        const repo = p.repo as Repo;
        const issue = repo.issues.get(Number(a.number));
        if (issue !== undefined) return issueValue(repo, issue);
        const pull = repo.pulls.get(Number(a.number));
        return pull === undefined ? null : pullValue(repo, pull);
      }, { number: 'Int!' }),
      issues: f('IssueConnection!', (p, a) => {
        const repo = p.repo as Repo;
        let issues = [...repo.issues.values()];
        const states = Array.isArray(a.states) ? (a.states as string[]) : null;
        if (states !== null && states.length > 0) issues = issues.filter((issue) => states.includes(issueStateEnum(issue)));
        const labels = Array.isArray(a.labels) ? (a.labels as string[]).map((name) => name.toLowerCase()) : null;
        if (labels !== null && labels.length > 0) issues = issues.filter((issue) => issue.labels.some((name) => labels.includes(name.toLowerCase())));
        const filter = (a.filterBy ?? {}) as { states?: string[]; labels?: string[] };
        if (Array.isArray(filter.states) && filter.states.length > 0) issues = issues.filter((issue) => (filter.states as string[]).includes(issueStateEnum(issue)));
        return connection('IssueConnection', sortByOrder(issues, a.orderBy).map((issue) => issueValue(repo, issue)), a);
      }, { ...PAGE_ARGS, states: '[IssueState!]', labels: '[String!]', orderBy: 'IssueOrder', filterBy: 'IssueFilters' }),
      pullRequest: f('PullRequest', (p, a) => {
        const pull = (p.repo as Repo).pulls.get(Number(a.number));
        return pull === undefined ? null : pullValue(p.repo, pull);
      }, { number: 'Int!' }),
      pullRequests: f('PullRequestConnection!', (p, a) => {
        const repo = p.repo as Repo;
        let pulls = [...repo.pulls.values()];
        const states = Array.isArray(a.states) ? (a.states as string[]) : null;
        if (states !== null && states.length > 0) pulls = pulls.filter((pull) => states.includes(pullStateEnum(pull)));
        if (typeof a.headRefName === 'string') pulls = pulls.filter((pull) => pull.head === a.headRefName);
        if (typeof a.baseRefName === 'string') pulls = pulls.filter((pull) => pull.base === a.baseRefName);
        return connection('PullRequestConnection', sortByOrder(pulls, a.orderBy).map((pull) => pullValue(repo, pull)), a);
      }, { ...PAGE_ARGS, states: '[PullRequestState!]', headRefName: 'String', baseRefName: 'String', labels: '[String!]', orderBy: 'IssueOrder' }),
      labels: f('LabelConnection', (p, a) => {
        const repo = p.repo as Repo;
        let labels = [...repo.labels.values()].sort((x, y) => x.id - y.id);
        if (typeof a.query === 'string' && a.query !== '') labels = labels.filter((label) => label.name.toLowerCase().includes(String(a.query).toLowerCase()));
        return connection('LabelConnection', labels.map((label) => labelValue(repo, label)), a);
      }, { ...PAGE_ARGS, query: 'String', orderBy: 'LabelOrder' }),
      label: f('Label', (p, a) => {
        const label = (p.repo as Repo).labels.get(String(a.name).toLowerCase());
        return label === undefined ? null : labelValue(p.repo, label);
      }, { name: 'String!' }),
    },
  },
  Ref: {
    kind: 'OBJECT',
    interfaces: ['Node'],
    fields: {
      id: f('ID!', (p) => nodeId('REF', `${p.repo.id}:${p.name}`)),
      name: f('String!', (p) => String(p.name).replace(/^refs\/(heads|tags)\//, '')),
      prefix: f('String!', (p) => /^refs\/(heads|tags)\//.exec(String(p.name))?.[0] ?? 'refs/'),
      target: f('GitObject', (p) => commitValue(p.repo, p.oid)),
      repository: f('Repository!', (p) => repoValue(p.repo)),
    },
  },
  Commit: {
    kind: 'OBJECT',
    interfaces: ['Node', 'GitObject'],
    fields: {
      id: f('ID!', (p) => nodeId('C', p.oid)),
      oid: f('GitObjectID!', (p) => p.oid),
      abbreviatedOid: f('String!', (p) => String(p.oid).slice(0, 7)),
      message: f('String!', (p, _a, ctx) => (ctx.forge.commitOf(p.repo, p.oid)?.message ?? '').replace(/\n$/, '')),
      messageHeadline: f('String!', (p, _a, ctx) => (ctx.forge.commitOf(p.repo, p.oid)?.message ?? '').split('\n')[0] ?? ''),
      committedDate: f('DateTime!', (p, _a, ctx) => isoOfSignature(ctx.forge.commitOf(p.repo, p.oid)?.committer ?? '')),
      authoredDate: f('DateTime!', (p, _a, ctx) => isoOfSignature(ctx.forge.commitOf(p.repo, p.oid)?.author ?? '')),
      url: f('URI!', (p, _a, ctx) => `${ctx.origin}/${ctx.forge.user.login}/${p.repo.name}/commit/${p.oid}`),
    },
  },
  Issue: {
    kind: 'OBJECT',
    interfaces: ['Node'],
    fields: {
      id: f('ID!', (p) => nodeId('I', p.issue.id)),
      databaseId: f('Int', (p) => p.issue.id),
      number: f('Int!', (p) => p.issue.number),
      title: f('String!', (p) => p.issue.title),
      body: f('String!', (p) => p.issue.body),
      bodyText: f('String!', (p) => p.issue.body),
      state: f('IssueState!', (p) => issueStateEnum(p.issue)),
      stateReason: f('IssueStateReason', (p) => (p.issue.stateReason === null ? null : String(p.issue.stateReason).toUpperCase())),
      closed: f('Boolean!', (p) => p.issue.state === 'closed'),
      closedAt: f('DateTime', (p) => p.issue.closedAt),
      createdAt: f('DateTime!', (p) => p.issue.createdAt),
      updatedAt: f('DateTime!', (p) => p.issue.updatedAt),
      locked: f('Boolean!', () => false),
      url: f('URI!', (p, _a, ctx) => `${ctx.origin}/${ctx.forge.user.login}/${p.repo.name}/issues/${p.issue.number}`),
      author: f('User', () => userValue()),
      repository: f('Repository!', (p) => repoValue(p.repo)),
      labels: f('LabelConnection', (p, a) => connection('LabelConnection', (p.issue as Issue).labels.map((name) => p.repo.labels.get(name.toLowerCase())).filter((label: Label | undefined) => label !== undefined).map((label: Label) => labelValue(p.repo, label)), a), { ...PAGE_ARGS }),
      comments: f('IssueCommentConnection!', (p, a) => connection('IssueCommentConnection', (p.issue as Issue).comments.map((comment) => commentValue(p.repo, p.issue.number, comment)), a), { ...PAGE_ARGS }),
      assignees: f('UserConnection!', (_p, a) => connection('UserConnection', [], a), { ...PAGE_ARGS }),
    },
  },
  IssueComment: {
    kind: 'OBJECT',
    interfaces: ['Node'],
    fields: {
      id: f('ID!', (p) => nodeId('IC', p.comment.id)),
      databaseId: f('Int', (p) => p.comment.id),
      body: f('String!', (p) => p.comment.body),
      createdAt: f('DateTime!', (p) => p.comment.createdAt),
      updatedAt: f('DateTime!', (p) => p.comment.updatedAt),
      url: f('URI!', (p, _a, ctx) => `${ctx.origin}/${ctx.forge.user.login}/${p.repo.name}/issues/${p.number}#issuecomment-${p.comment.id}`),
      author: f('User', () => userValue()),
    },
  },
  Label: {
    kind: 'OBJECT',
    interfaces: ['Node'],
    fields: {
      id: f('ID!', (p) => nodeId('LA', p.label.id)),
      name: f('String!', (p) => p.label.name),
      color: f('String!', (p) => p.label.color),
      description: f('String', (p) => p.label.description),
    },
  },
  PullRequest: {
    kind: 'OBJECT',
    interfaces: ['Node'],
    fields: {
      id: f('ID!', (p) => nodeId('PR', p.pull.id)),
      databaseId: f('Int', (p) => p.pull.id),
      number: f('Int!', (p) => p.pull.number),
      title: f('String!', (p) => p.pull.title),
      body: f('String!', (p) => p.pull.body),
      state: f('PullRequestState!', (p) => pullStateEnum(p.pull)),
      merged: f('Boolean!', (p) => p.pull.merged),
      mergedAt: f('DateTime', (p) => p.pull.mergedAt),
      closed: f('Boolean!', (p) => p.pull.state === 'closed'),
      closedAt: f('DateTime', (p) => p.pull.closedAt),
      createdAt: f('DateTime!', (p) => p.pull.createdAt),
      updatedAt: f('DateTime!', (p) => p.pull.updatedAt),
      isDraft: f('Boolean!', () => false),
      url: f('URI!', (p, _a, ctx) => `${ctx.origin}/${ctx.forge.user.login}/${p.repo.name}/pull/${p.pull.number}`),
      author: f('User', () => userValue()),
      repository: f('Repository!', (p) => repoValue(p.repo)),
      headRefName: f('String!', (p) => p.pull.head),
      baseRefName: f('String!', (p) => p.pull.base),
      headRefOid: f('GitObjectID!', (p, _a, ctx) => ctx.forge.pullTips(p.repo, p.pull).head),
      baseRefOid: f('GitObjectID!', (p, _a, ctx) => ctx.forge.pullTips(p.repo, p.pull).base),
      headRef: f('Ref', (p) => {
        const oid = (p.repo as Repo).branchTip(p.pull.head);
        return oid === undefined ? null : refValue(p.repo, `refs/heads/${p.pull.head}`, oid);
      }),
      baseRef: f('Ref', (p) => {
        const oid = (p.repo as Repo).branchTip(p.pull.base);
        return oid === undefined ? null : refValue(p.repo, `refs/heads/${p.pull.base}`, oid);
      }),
      mergeable: f('MergeableState!', (p, _a, ctx) => {
        const value = ctx.forge.mergeable(p.repo, p.pull);
        return value === null ? 'UNKNOWN' : value ? 'MERGEABLE' : 'CONFLICTING';
      }),
      mergeCommit: f('Commit', (p) => (p.pull.mergeCommitSha === null ? null : commitValue(p.repo, p.pull.mergeCommitSha))),
      additions: f('Int!', (p, _a, ctx) => ctx.forge.pullFiles(p.repo, p.pull).reduce((sum, file) => sum + file.additions, 0)),
      deletions: f('Int!', (p, _a, ctx) => ctx.forge.pullFiles(p.repo, p.pull).reduce((sum, file) => sum + file.deletions, 0)),
      changedFiles: f('Int!', (p, _a, ctx) => ctx.forge.pullFiles(p.repo, p.pull).length),
      files: f('PullRequestChangedFileConnection', (p, a, ctx) =>
        connection(
          'PullRequestChangedFileConnection',
          ctx.forge.pullFiles(p.repo, p.pull).map((file) => ({
            __typename: 'PullRequestChangedFile',
            path: file.filename,
            additions: file.additions,
            deletions: file.deletions,
            changeType: file.status === 'added' ? 'ADDED' : file.status === 'removed' ? 'DELETED' : 'MODIFIED',
          })),
          a,
        ), { ...PAGE_ARGS }),
      commits: f('PullRequestCommitConnection!', (p, a, ctx) =>
        connection('PullRequestCommitConnection', ctx.forge.pullCommits(p.repo, p.pull).map((oid) => ({ __typename: 'PullRequestCommit', commit: commitValue(p.repo, oid) })), a), { ...PAGE_ARGS }),
      comments: f('IssueCommentConnection!', (p, a) => connection('IssueCommentConnection', (p.pull as Pull).comments.map((comment) => commentValue(p.repo, p.pull.number, comment)), a), { ...PAGE_ARGS }),
      labels: f('LabelConnection', (_p, a) => connection('LabelConnection', [], a), { ...PAGE_ARGS }),
    },
  },
  PullRequestChangedFile: { kind: 'OBJECT', fields: { path: f('String!'), additions: f('Int!'), deletions: f('Int!'), changeType: f('PatchStatus!') } },
  PullRequestCommit: { kind: 'OBJECT', fields: { commit: f('Commit!') } },
  PageInfo: { kind: 'OBJECT', fields: { hasNextPage: f('Boolean!'), hasPreviousPage: f('Boolean!'), startCursor: f('String'), endCursor: f('String') } },
  RateLimit: { kind: 'OBJECT', fields: { limit: f('Int!'), cost: f('Int!'), remaining: f('Int!'), used: f('Int!'), resetAt: f('DateTime!'), nodeCount: f('Int!') } },
  CreateIssuePayload: { kind: 'OBJECT', fields: { issue: f('Issue'), clientMutationId: f('String') } },
  CloseIssuePayload: { kind: 'OBJECT', fields: { issue: f('Issue'), clientMutationId: f('String') } },
  ReopenIssuePayload: { kind: 'OBJECT', fields: { issue: f('Issue'), clientMutationId: f('String') } },
  UpdateIssuePayload: { kind: 'OBJECT', fields: { issue: f('Issue'), clientMutationId: f('String') } },
  AddCommentPayload: { kind: 'OBJECT', fields: { commentEdge: f('IssueCommentEdge'), subject: f('Node'), clientMutationId: f('String') } },
  CreatePullRequestPayload: { kind: 'OBJECT', fields: { pullRequest: f('PullRequest'), clientMutationId: f('String') } },
  MergePullRequestPayload: { kind: 'OBJECT', fields: { pullRequest: f('PullRequest'), clientMutationId: f('String') } },
  ClosePullRequestPayload: { kind: 'OBJECT', fields: { pullRequest: f('PullRequest'), clientMutationId: f('String') } },
  CreateRepositoryPayload: { kind: 'OBJECT', fields: { repository: f('Repository'), clientMutationId: f('String') } },
  IssueState: { kind: 'ENUM', values: ['OPEN', 'CLOSED'] },
  IssueStateReason: { kind: 'ENUM', values: ['COMPLETED', 'NOT_PLANNED', 'REOPENED', 'DUPLICATE'] },
  PullRequestState: { kind: 'ENUM', values: ['OPEN', 'CLOSED', 'MERGED'] },
  MergeableState: { kind: 'ENUM', values: ['MERGEABLE', 'CONFLICTING', 'UNKNOWN'] },
  PullRequestMergeMethod: { kind: 'ENUM', values: ['MERGE', 'SQUASH', 'REBASE'] },
  RepositoryPermission: { kind: 'ENUM', values: ['ADMIN', 'MAINTAIN', 'WRITE', 'TRIAGE', 'READ'] },
  RepositoryVisibility: { kind: 'ENUM', values: ['PRIVATE', 'PUBLIC', 'INTERNAL'] },
  SearchType: { kind: 'ENUM', values: ['ISSUE', 'ISSUE_ADVANCED', 'REPOSITORY', 'USER', 'DISCUSSION'] },
  IssueOrderField: { kind: 'ENUM', values: ['CREATED_AT', 'UPDATED_AT', 'COMMENTS'] },
  LabelOrderField: { kind: 'ENUM', values: ['NAME', 'CREATED_AT'] },
  OrderDirection: { kind: 'ENUM', values: ['ASC', 'DESC'] },
  PatchStatus: { kind: 'ENUM', values: ['ADDED', 'DELETED', 'RENAMED', 'COPIED', 'MODIFIED', 'CHANGED'] },
  __TypeKind: { kind: 'ENUM', values: ['SCALAR', 'OBJECT', 'INTERFACE', 'UNION', 'ENUM', 'INPUT_OBJECT', 'LIST', 'NON_NULL'] },
  IssueOrder: { kind: 'INPUT_OBJECT', inputFields: { field: 'IssueOrderField!', direction: 'OrderDirection!' } },
  LabelOrder: { kind: 'INPUT_OBJECT', inputFields: { field: 'LabelOrderField!', direction: 'OrderDirection!' } },
  IssueFilters: { kind: 'INPUT_OBJECT', inputFields: { states: '[IssueState!]', labels: '[String!]', assignee: 'String', createdBy: 'String' } },
  CreateIssueInput: { kind: 'INPUT_OBJECT', inputFields: { repositoryId: 'ID!', title: 'String!', body: 'String', labelIds: '[ID!]', assigneeIds: '[ID!]', clientMutationId: 'String' } },
  CloseIssueInput: { kind: 'INPUT_OBJECT', inputFields: { issueId: 'ID!', stateReason: 'IssueStateReason', clientMutationId: 'String' } },
  ReopenIssueInput: { kind: 'INPUT_OBJECT', inputFields: { issueId: 'ID!', clientMutationId: 'String' } },
  UpdateIssueInput: { kind: 'INPUT_OBJECT', inputFields: { id: 'ID!', title: 'String', body: 'String', state: 'IssueState', clientMutationId: 'String' } },
  AddCommentInput: { kind: 'INPUT_OBJECT', inputFields: { subjectId: 'ID!', body: 'String!', clientMutationId: 'String' } },
  CreatePullRequestInput: { kind: 'INPUT_OBJECT', inputFields: { repositoryId: 'ID!', baseRefName: 'String!', headRefName: 'String!', headRepositoryId: 'ID', title: 'String!', body: 'String', draft: 'Boolean', maintainerCanModify: 'Boolean', clientMutationId: 'String' } },
  MergePullRequestInput: { kind: 'INPUT_OBJECT', inputFields: { pullRequestId: 'ID!', mergeMethod: 'PullRequestMergeMethod', commitHeadline: 'String', commitBody: 'String', expectedHeadOid: 'GitObjectID', authorEmail: 'String', clientMutationId: 'String' } },
  ClosePullRequestInput: { kind: 'INPUT_OBJECT', inputFields: { pullRequestId: 'ID!', clientMutationId: 'String' } },
  CreateRepositoryInput: { kind: 'INPUT_OBJECT', inputFields: { name: 'String!', description: 'String', visibility: 'RepositoryVisibility!', clientMutationId: 'String' } },
  String: { kind: 'SCALAR' },
  Int: { kind: 'SCALAR' },
  Float: { kind: 'SCALAR' },
  Boolean: { kind: 'SCALAR' },
  ID: { kind: 'SCALAR' },
  DateTime: { kind: 'SCALAR' },
  URI: { kind: 'SCALAR' },
  GitObjectID: { kind: 'SCALAR' },
  __Schema: {
    kind: 'OBJECT',
    fields: {
      types: f('[__Type!]!', () => Object.keys(SCHEMA).map((name) => typeValue(name))),
      queryType: f('__Type!', () => typeValue('Query')),
      mutationType: f('__Type', () => typeValue('Mutation')),
      subscriptionType: f('__Type', () => null),
      directives: f('[__Directive!]!', () => [
        { __typename: '__Directive', name: 'include', description: null, locations: ['FIELD', 'FRAGMENT_SPREAD', 'INLINE_FRAGMENT'], args: [inputValue('if', 'Boolean!')] },
        { __typename: '__Directive', name: 'skip', description: null, locations: ['FIELD', 'FRAGMENT_SPREAD', 'INLINE_FRAGMENT'], args: [inputValue('if', 'Boolean!')] },
      ]),
    },
  },
  __Directive: { kind: 'OBJECT', fields: { name: f('String!'), description: f('String'), locations: f('[String!]!'), args: f('[__InputValue!]!') } },
  __Type: {
    kind: 'OBJECT',
    fields: {
      kind: f('__TypeKind!', (p) => p.kind),
      name: f('String', (p) => p.name ?? null),
      description: f('String', (p) => p.description ?? null),
      fields: f('[__Field!]', (p, a) => {
        if (p.def === undefined || p.def.fields === undefined) return null;
        void a;
        return Object.entries(p.def.fields as Record<string, FieldDef>)
          .filter(([name]) => !name.startsWith('__'))
          .map(([name, def]) => ({ __typename: '__Field', name, description: def.description ?? null, args: Object.entries(def.args ?? {}).map(([argName, type]) => inputValue(argName, type)), type: typeRefValue(def.type), isDeprecated: false, deprecationReason: null }));
      }, { includeDeprecated: 'Boolean' }),
      interfaces: f('[__Type!]', (p) => (p.def?.kind === 'OBJECT' ? (p.def.interfaces ?? []).map((name: string) => typeValue(name)) : null)),
      possibleTypes: f('[__Type!]', (p) => (p.def?.kind === 'INTERFACE' ? Object.entries(SCHEMA).filter(([, def]) => def.interfaces?.includes(p.name)).map(([name]) => typeValue(name)) : null)),
      enumValues: f('[__EnumValue!]', (p) => (p.def?.kind === 'ENUM' ? (p.def.values as string[]).map((name) => ({ __typename: '__EnumValue', name, description: null, isDeprecated: false, deprecationReason: null })) : null), { includeDeprecated: 'Boolean' }),
      inputFields: f('[__InputValue!]', (p) => (p.def?.kind === 'INPUT_OBJECT' ? Object.entries(p.def.inputFields as Record<string, string>).map(([name, type]) => inputValue(name, type)) : null)),
      ofType: f('__Type', (p) => p.ofType ?? null),
      specifiedByURL: f('String', () => null),
    },
  },
  __Field: { kind: 'OBJECT', fields: { name: f('String!'), description: f('String'), args: f('[__InputValue!]!'), type: f('__Type!'), isDeprecated: f('Boolean!'), deprecationReason: f('String') } },
  __InputValue: { kind: 'OBJECT', fields: { name: f('String!'), description: f('String'), type: f('__Type!'), defaultValue: f('String') } },
  __EnumValue: { kind: 'OBJECT', fields: { name: f('String!'), description: f('String'), isDeprecated: f('Boolean!'), deprecationReason: f('String') } },
};

/** A Relay connection type and its edge type for `node` (nodes, edges, pageInfo, totalCount). */
function connectionTypes(node: string, connectionName = `${node}Connection`): Record<string, TypeDef> {
  const edge = `${connectionName.replace(/Connection$/, '')}Edge`;
  return {
    [connectionName]: { kind: 'OBJECT', fields: { nodes: f(`[${node}]`), edges: f(`[${edge}]`), pageInfo: f('PageInfo!'), totalCount: f('Int!') } },
    [edge]: { kind: 'OBJECT', fields: { node: f(node), cursor: f('String!') } },
  };
}

Object.assign(SCHEMA, {
  ...connectionTypes('Issue'),
  ...connectionTypes('PullRequest'),
  ...connectionTypes('IssueComment'),
  ...connectionTypes('Label'),
  ...connectionTypes('PullRequestChangedFile'),
  ...connectionTypes('Ref'),
  ...connectionTypes('User'),
  ...connectionTypes('PullRequestCommit'),
});

function typeValue(name: string): Record<string, unknown> | null {
  const def = SCHEMA[name];
  if (def === undefined) return null;
  return { __typename: '__Type', kind: def.kind, name, description: def.description ?? null, def };
}

function typeRefValue(ref: string): Record<string, unknown> | null {
  if (ref.endsWith('!')) return { __typename: '__Type', kind: 'NON_NULL', name: null, ofType: typeRefValue(ref.slice(0, -1)) };
  if (ref.startsWith('[')) return { __typename: '__Type', kind: 'LIST', name: null, ofType: typeRefValue(ref.slice(1, -1)) };
  return typeValue(ref);
}

function inputValue(name: string, type: string): Record<string, unknown> {
  return { __typename: '__InputValue', name, description: null, type: typeRefValue(type), defaultValue: null };
}

/** The named type at the bottom of a type reference (`[Issue!]!` -> `Issue`). */
function namedType(ref: string): string {
  return ref.replace(/[[\]!]/g, '');
}

function implementsType(runtime: string, condition: string): boolean {
  if (runtime === condition) return true;
  return SCHEMA[runtime]?.interfaces?.includes(condition) === true;
}

// ------------------------------------------------------------------ validation and execution

interface GraphQLError {
  message: string;
  locations?: Array<{ line: number; column: number }>;
  path?: Array<string | number>;
  type?: string;
  extensions?: Record<string, unknown>;
}

class Execution {
  readonly ctx: Ctx;
  readonly fragments: Map<string, FragmentNode>;
  readonly variables: Record<string, unknown>;
  readonly errors: GraphQLError[] = [];

  constructor(ctx: Ctx, fragments: Map<string, FragmentNode>, variables: Record<string, unknown>) {
    this.ctx = ctx;
    this.fragments = fragments;
    this.variables = variables;
  }

  value(node: ValueNode): unknown {
    switch (node.kind) {
      case 'var':
        return this.variables[node.name] ?? null;
      case 'int':
        return Number.parseInt(node.value, 10);
      case 'float':
        return Number.parseFloat(node.value);
      case 'boolean':
        return node.value === 'true';
      case 'string':
      case 'enum':
        return node.value;
      case 'null':
        return null;
      case 'list':
        return node.values.map((item) => this.value(item));
      case 'object': {
        const out: Record<string, unknown> = {};
        for (const field of node.fields) out[field.name] = this.value(field.value);
        return out;
      }
    }
  }

  private included(directives: readonly Directive[]): boolean {
    for (const directive of directives) {
      const condition = directive.args.find((arg) => arg.name === 'if');
      const value = condition === undefined ? false : this.value(condition.value) === true;
      if (directive.name === 'skip' && value) return false;
      if (directive.name === 'include' && !value) return false;
    }
    return true;
  }

  /** Validate a selection set against a type; every problem is recorded. */
  validate(selections: readonly Selection[], typeName: string, seen: Set<string> = new Set()): void {
    const def = SCHEMA[typeName];
    for (const selection of selections) {
      if (selection.kind === 'field') {
        if (selection.name === '__typename') {
          if (selection.selections !== null) this.errors.push({ message: 'Selections can\'t be made on scalars (field \'__typename\' returns String but has selections)', locations: [{ line: selection.line, column: selection.column }] });
          continue;
        }
        const field = def?.fields?.[selection.name];
        if (field === undefined) {
          this.errors.push({
            message: `Field '${selection.name}' doesn't exist on type '${typeName}'`,
            locations: [{ line: selection.line, column: selection.column }],
            path: [selection.alias ?? selection.name],
            extensions: { code: 'undefinedField', typeName, fieldName: selection.name },
          });
          continue;
        }
        const target = namedType(field.type);
        const targetDef = SCHEMA[target];
        const composite = targetDef !== undefined && (targetDef.kind === 'OBJECT' || targetDef.kind === 'INTERFACE');
        if (composite && selection.selections === null) {
          this.errors.push({ message: `Field must have selections (field '${selection.name}' returns ${target} but has no selections. Did you mean '${selection.name} { ... }'?)`, locations: [{ line: selection.line, column: selection.column }], extensions: { code: 'selectionMismatch' } });
        } else if (!composite && selection.selections !== null) {
          this.errors.push({ message: `Selections can't be made on scalars (field '${selection.name}' returns ${target} but has selections [${selection.selections.map((child) => (child.kind === 'field' ? child.name : '...')).join(', ')}])`, locations: [{ line: selection.line, column: selection.column }], extensions: { code: 'selectionMismatch' } });
        } else if (composite && selection.selections !== null) {
          this.validate(selection.selections, target, seen);
        }
        for (const arg of selection.args) {
          if (field.args === undefined || !(arg.name in field.args)) {
            this.errors.push({ message: `Field '${selection.name}' doesn't accept argument '${arg.name}'`, locations: [{ line: selection.line, column: selection.column }], extensions: { code: 'argumentNotAccepted', name: selection.name, typeName: 'Field', argumentName: arg.name } });
          }
        }
        for (const [argName, argType] of Object.entries(field.args ?? {})) {
          if (argType.endsWith('!') && !selection.args.some((arg) => arg.name === argName)) {
            this.errors.push({ message: `Field '${selection.name}' is missing required arguments: ${argName}`, locations: [{ line: selection.line, column: selection.column }], extensions: { code: 'missingRequiredArguments', className: 'Field', name: selection.name, arguments: argName } });
          }
        }
      } else if (selection.kind === 'inline') {
        const condition = selection.typeCondition ?? typeName;
        if (SCHEMA[condition] === undefined) {
          this.errors.push({ message: `No such type ${condition}, so it can't be a fragment condition`, extensions: { code: 'undefinedType', typeName: condition } });
          continue;
        }
        this.validate(selection.selections, condition, seen);
      } else {
        const fragment = this.fragments.get(selection.name);
        if (fragment === undefined) {
          this.errors.push({ message: `Fragment ${selection.name} was used, but not defined`, locations: [{ line: selection.line, column: selection.column }], extensions: { code: 'useAndDefineFragment', fragmentName: selection.name } });
          continue;
        }
        if (seen.has(fragment.name)) continue;
        if (SCHEMA[fragment.typeCondition] === undefined) {
          this.errors.push({ message: `No such type ${fragment.typeCondition}, so it can't be a fragment condition`, extensions: { code: 'undefinedType', typeName: fragment.typeCondition } });
          continue;
        }
        const nested = new Set(seen);
        nested.add(fragment.name);
        this.validate(fragment.selections, fragment.typeCondition, nested);
      }
    }
  }

  /** Group the fields a selection set asks of an object of `runtime` type, by response key, in order. */
  private collect(selections: readonly Selection[], runtime: string, out: Map<string, FieldNode[]> = new Map()): Map<string, FieldNode[]> {
    for (const selection of selections) {
      if (!this.included(selection.directives)) continue;
      if (selection.kind === 'field') {
        const key = selection.alias ?? selection.name;
        const list = out.get(key);
        if (list === undefined) out.set(key, [selection]);
        else list.push(selection);
      } else if (selection.kind === 'inline') {
        if (selection.typeCondition === null || implementsType(runtime, selection.typeCondition)) this.collect(selection.selections, runtime, out);
      } else {
        const fragment = this.fragments.get(selection.name);
        if (fragment !== undefined && implementsType(runtime, fragment.typeCondition)) this.collect(fragment.selections, runtime, out);
      }
    }
    return out;
  }

  selectionSet(selections: readonly Selection[], typeName: string, parent: Record<string, any>, path: Array<string | number>): Record<string, unknown> {
    const runtime = typeof parent.__typename === 'string' ? parent.__typename : typeName;
    const def = SCHEMA[runtime];
    const out: Record<string, unknown> = {};
    for (const [key, nodes] of this.collect(selections, runtime)) {
      const node = nodes[0] as FieldNode;
      if (node.name === '__typename') {
        out[key] = runtime;
        continue;
      }
      const field = def?.fields?.[node.name];
      if (field === undefined) {
        out[key] = null;
        continue;
      }
      const args: Record<string, unknown> = {};
      for (const arg of node.args) args[arg.name] = this.value(arg.value);
      const fieldPath = [...path, key];
      let value: unknown;
      try {
        value = field.resolve === undefined ? parent[node.name] : field.resolve(parent, args, this.ctx);
      } catch (err) {
        this.errors.push(errorOf(err, fieldPath, node));
        out[key] = null;
        continue;
      }
      const merged: Selection[] = [];
      for (const each of nodes) if (each.selections !== null) merged.push(...each.selections);
      out[key] = this.complete(field.type, value, merged, fieldPath);
    }
    return out;
  }

  private complete(ref: string, value: unknown, selections: readonly Selection[], path: Array<string | number>): unknown {
    if (value === null || value === undefined) return null;
    const inner = ref.endsWith('!') ? ref.slice(0, -1) : ref;
    if (inner.startsWith('[')) {
      if (!Array.isArray(value)) return null;
      const item = inner.slice(1, -1);
      return value.map((entry, index) => this.complete(item, entry, selections, [...path, index]));
    }
    const def = SCHEMA[inner];
    if (def !== undefined && (def.kind === 'OBJECT' || def.kind === 'INTERFACE')) {
      return this.selectionSet(selections, inner, value as Record<string, any>, path);
    }
    return value;
  }
}

function errorOf(err: unknown, path: Array<string | number>, node: FieldNode): GraphQLError {
  const message = err instanceof Error ? err.message : String(err);
  const status = err instanceof ApiError ? err.status : 500;
  const type = status === 404 ? 'NOT_FOUND' : status === 422 || status === 409 || status === 405 ? 'UNPROCESSABLE' : status === 403 ? 'FORBIDDEN' : 'INTERNAL';
  const detail = err instanceof ApiError && err.errors !== undefined ? `${message}: ${err.errors.map((entry) => (typeof entry === 'object' && entry !== null && 'message' in entry ? String((entry as { message: unknown }).message) : JSON.stringify(entry))).join('; ')}` : message;
  return { type, path, locations: [{ line: node.line, column: node.column }], message: detail };
}

/** Run one {query, variables, operationName} request and answer its JSON body. */
export function runGraphQL(ctx: Ctx, request: unknown): Record<string, unknown> {
  if (typeof request !== 'object' || request === null || Array.isArray(request)) return { errors: [{ message: 'A query attribute must be specified and must be a string.' }] };
  const { query, variables, operationName } = request as { query?: unknown; variables?: unknown; operationName?: unknown };
  if (typeof query !== 'string' || query.trim() === '') return { errors: [{ message: 'A query attribute must be specified and must be a string.' }] };
  let definitions: Definition[];
  try {
    definitions = new Parser(query).document();
  } catch (err) {
    if (err instanceof GraphQLSyntaxError) return { errors: [{ message: err.message, locations: [{ line: err.line, column: err.column }] }] };
    return { errors: [{ message: err instanceof Error ? err.message : String(err) }] };
  }
  const fragments = new Map<string, FragmentNode>();
  const operations: OperationNode[] = [];
  for (const definition of definitions) {
    if (definition.kind === 'fragment') fragments.set(definition.name, definition);
    else operations.push(definition);
  }
  let operation: OperationNode | undefined;
  if (typeof operationName === 'string' && operationName !== '') {
    operation = operations.find((candidate) => candidate.name === operationName);
    if (operation === undefined) return { errors: [{ message: `Unknown operation named "${operationName}".` }] };
  } else if (operations.length === 1) {
    operation = operations[0];
  } else {
    return { errors: [{ message: operations.length === 0 ? 'No operations in query document' : 'An operation name is required' }] };
  }
  const op = operation as OperationNode;
  if (op.operation === 'subscription') return { errors: [{ message: 'Subscriptions are not supported' }] };
  const given = typeof variables === 'object' && variables !== null && !Array.isArray(variables) ? (variables as Record<string, unknown>) : {};
  const values: Record<string, unknown> = {};
  const execution = new Execution(ctx, fragments, values);
  for (const declared of op.variables) {
    if (declared.name in given && given[declared.name] !== undefined) values[declared.name] = given[declared.name];
    else if (declared.defaultValue !== null) values[declared.name] = execution.value(declared.defaultValue);
    else if (declared.type.endsWith('!')) {
      execution.errors.push({ message: `Variable $${declared.name} of type ${declared.type} was provided invalid value`, extensions: { code: 'variableMismatch', variableName: declared.name, typeName: declared.type } });
    } else values[declared.name] = null;
  }
  const root = op.operation === 'mutation' ? 'Mutation' : 'Query';
  execution.validate(op.selections, root);
  if (execution.errors.length > 0) return { errors: execution.errors };
  const data = execution.selectionSet(op.selections, root, {}, []);
  return execution.errors.length > 0 ? { data, errors: execution.errors } : { data };
}
