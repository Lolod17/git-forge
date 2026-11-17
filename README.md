[![github-core-clone-01](https://make.fun/badge/github-core-clone-01.svg)](https://make.fun/bounty/github-core-clone-01)
[![rank](https://make.fun/badge/github-core-clone-01/Lolod17/git-forge/rank.svg)](https://make.fun/bounty/github-core-clone-01)
[![reward](https://make.fun/badge/github-core-clone-01/Lolod17/git-forge/reward.svg)](https://make.fun/bounty/github-core-clone-01)

# gitforge

A small GitHub-compatible git forge for the make.fun `github-core-clone-01` contest: one Node 22 process,
standard library only, no git binary, no network calls of its own. It serves git smart HTTP, a subset of
the REST v3 and GraphQL v4 APIs, and a Model Context Protocol endpoint, all over one shared store.

## Run it

```sh
MAKEFUN_ENV_URL=http://store:8080 MAKEFUN_SHOP_TOKEN=<token> node main.ts
```

`main.ts` is the contest lifecycle: it binds an ephemeral port, POSTs `{"port": N}` to
`$MAKEFUN_ENV_URL/register`, polls `$MAKEFUN_ENV_URL/done` and exits 0 when it reads `{"done": true}`.

| Variable | Meaning |
| --- | --- |
| `MAKEFUN_SHOP_TOKEN` | The bearer token every request must carry. Unset: any non-empty token is accepted (local use only). |
| `FORGE_LOGIN` | The login that token names. It owns every repository. Default `adaiversen` (see below). |
| `FORGE_DATA_DIR` | Data directory. Default: a fresh directory under `/tmp/gitforge-data/`. Pointing at an existing one reloads it. |

## What it implements

**Git smart HTTP** at `/<owner>/<repo>.git` (`git.ts`, `githttp.ts`), written from the format docs:

- SHA-1 object hashing; tree, commit and tag encoding; pkt-line framing; packfile reading (OFS_DELTA and
  REF_DELTA, thin-pack bases from the store, trailer checksum verified) and packfile writing.
- `info/refs`: the protocol v2 capability advertisement (`version 2`, `ls-refs=unborn`, `fetch=shallow`,
  `object-format=sha1`) when the client sends `Git-Protocol: version=2`; otherwise the v0 ref advertisement
  with the `# service=` preamble, peeled tags, `symref=HEAD:...` and the empty repository's
  `capabilities^{}` line. A missing repository is 404 on both services.
- upload-pack v2: `ls-refs` (peel, symrefs, unborn, ref-prefix) and `fetch` (want/have/done, `shallow`
  and `deepen N` with `shallow-info`, side-band packfile section). The v0 fallback handles side-band-64k,
  multi_ack_detailed negotiation rounds and shallow.
- receive-pack v0: create, update and delete (`report-status`, `side-band-64k`, `delete-refs`). Updates
  must fast-forward, and each refused ref gets its own `ng <ref> non-fast-forward` line. Every ref in a
  command list is decided on its own (no atomic pushes). New tips are connectivity-checked before a ref
  moves.

**REST v3** under `/api/v3` (`rest.ts`): API root and `/meta`, `/user`, repositories (create, get, patch,
delete, paginated list with `Link` headers), branches, `git/refs` (create, get, update, delete), git
commits/trees/blobs, commits (list with `sha`/`path`, get one), contents (get file or directory, create,
update with a 409 on a stale sha, delete), labels, issues (create with label validation and a 422 for an
unknown label, list with state/sort/direction/labels filters, get, patch), issue comments, and pull
requests: create, list, get, files, commits, and merge. Merge supports `merge`, `squash` and `rebase` over a
real three-way tree merge, and a conflicting pull request is 405. Every reply is JSON with a non-empty
`X-GitHub-Request-Id`. Errors carry a string `message`, a missing token is 401, a malformed body is 400,
and an unknown route is a JSON 404.

**GraphQL v4** at `/api/graphql` (`graphql.ts`): its own parser (variables, aliases, fragments, inline
fragments, `@skip`/`@include`) and a typed schema that drives both validation and introspection
(`__type`, `__schema`). Queries: `viewer`, `repository` (issues and pull requests with states/orderBy/cursor
pagination, `issue`, `pullRequest`, labels, refs, `defaultBranchRef`), `node`/`nodes` with opaque global
ids. Mutations: `createIssue`, `closeIssue`, `reopenIssue`, `updateIssue`, `addComment`,
`createPullRequest`, `mergePullRequest`, `closePullRequest`, `createRepository`. A field the schema does not
define is a 200 with an `errors` array, and a JSON array body is a batch.

**MCP** at `/mcp` (`mcp.ts`): JSON-RPC 2.0 over Streamable HTTP, answered as `application/json`. It supports
revision 2026-07-28 (`server/discover`) and the `initialize` handshake (2025-11-25 and earlier, with
`Mcp-Session-Id`). There are 15 tools: create/get/list repos, create/get/list/update issues, issue comments,
create branch, create/get/list/merge pull requests, and get/create-or-update file contents. They call the
same domain operations as REST, so ids match across surfaces.

**Store** (`forge.ts`): everything is held in memory and written through to disk. Metadata changes are
appended to `journal.ndjson`, and each repository's git objects are appended to `repos/<id>/objects`. A
forge restarted on the same data directory replays both.

## What it does not do

- **One user.** The token names a single login, which owns every repository. There are no organisations,
  collaborators or permission levels. Any other owner's path is a 404. The default login, `adaiversen`, is
  the viewer of the contest's fixed public scenario (`SCENARIO_SEED` in the kit's `wire.ts`). Nothing in
  the trial tells a forge which login a token names, so the suite's owner has to be configured. Set
  `FORGE_LOGIN` to use another.
- **No deltas in served packs.** Packs the forge sends hold whole objects (valid, but larger than
  git's). Deltas are accepted on push.
- **Protocol coverage.** SHA-1 only. No protocol v1, no partial clone (`filter`), no `packfile-uris`, no
  `want-ref`, no `deepen-since`/`deepen-not`, no `include-tag`, no atomic pushes, no push options. There is
  no SSH, only HTTP.
- **API coverage.** Only the REST endpoints listed above. In particular there are no search, reviews,
  review comments, releases, statuses/checks, webhooks, forks or users other than the viewer. The GraphQL
  schema covers only the types above (no `search` field, no subscriptions). MCP answers JSON only (no SSE
  stream, no GET stream) and lists no resources or prompts.
- **Not hardened for real use.** Single process, in-memory working set, no rate limiting and no TLS. It is
  a contest entry, not a production server.

## Layout

- `main.ts`: the entry/env lifecycle (the starter's, plus process-level guards so one bad request is never
  fatal).
- `app.ts`: the HTTP server, authentication and dispatch.
- `git.ts`, `githttp.ts`: the git object layer and smart HTTP.
- `forge.ts`: the domain model, store and operations shared by every surface.
- `rest.ts`, `graphql.ts`, `mcp.ts`: the three API surfaces.

MIT licensed (see `LICENSE`).
