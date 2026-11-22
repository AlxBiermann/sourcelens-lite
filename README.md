# SourceLens Lite

SourceLens Lite indexes the GitHub repositories you choose and lets coding agents search them over MCP, without a local checkout.

- Your agent (Claude Code, Cursor, VS Code or any other MCP client) can search code, find where a symbol is defined and read files in repositories that are not on your machine: other services, shared libraries, the rest of the organisation.
- The index follows one branch per repository. A push to that branch re-indexes only the files that changed.
- It runs as one container with one SQLite file. A web UI handles setup, repositories, members, API keys and a search playground.

![Repository list](docs/screenshots/repositories.png)

## What an agent gets

After you add the MCP endpoint to your agent, it can call tools like these. A `find_symbol` call:

```json
{ "name": "find_symbol", "arguments": { "name": "getUserById" } }
```

returns where the symbol is defined, with its signature:

```text
1. function getUserById — acme/api src/users.ts:9-16
   function getUserById(id: string): User | undefined
```

A `search_code` call with plain words, `{ "query": "user id lookup" }`, returns the matching code blocks with repository, path and line range (first of three results shown):

````text
3 results for "user id lookup" (lexical)

1. acme/api src/users.ts:1-16  block User, users, getUserById
```typescript
export interface User {
  id: string;
  name: string;
  email: string;
}

const users = new Map<string, User>();

export function getUserById(id: string): User | undefined {
  // Look the user up in the in-memory store.
  const user = users.get(id);
  if (!user) {
    return undefined;
  }
  return user;
}
```
````

Both outputs come from the test fixtures in this repository. The agent can then read more of the file with `get_code`.

![Search playground](docs/screenshots/playground.png)

## How it works

1. **GitHub App.** SourceLens creates a GitHub App for you (or uses one you provide) with read-only access (`Contents: read`, `Metadata: read`). You install it on the repositories it may read. Users sign in with GitHub, or with an OpenID Connect provider if an admin sets up SSO.
2. **Index.** For each connected repository SourceLens indexes one branch (the default branch unless you pick another). Files are split into chunks along functions, classes and methods with tree-sitter; definitions go into a symbol table; everything goes into a full-text index. On a `push` webhook, SourceLens compares the Git tree with the indexed one and fetches and re-indexes only changed files. "Sync now", "Sync all" and an optional schedule do the same on demand (see [Sync](#sync)).
3. **Embeddings (optional).** If you configure an OpenAI-compatible embeddings endpoint, chunks are also embedded and search adds results by meaning.
4. **MCP.** A read-only MCP endpoint at `/mcp` answers agents. Each request needs an API key created in the UI or an OAuth access token that the agent obtains after you allow it in the browser.

One binary (Rust, with the React UI embedded), one SQLite database, no other services.

## Quick start with Docker

```sh
docker run -d --name sourcelens \
  -p 3000:3000 \
  -e SOURCELENS_PUBLIC_URL=http://localhost:3000 \
  -v sourcelens-data:/data \
  ghcr.io/alxbiermann/sourcelens-lite:latest

docker logs sourcelens
```

The log contains a line like:

```text
Setup token: 3f9c…e1 — open http://localhost:3000/setup
```

The token is printed at every start until an admin exists, and it changes with every start. Then:

1. **Open `/setup`** (`http://localhost:3000/setup`).
2. **Create the GitHub App.** Paste the setup token, optionally enter an organization (leave it empty for your personal account) and click "Create GitHub App". GitHub shows the app it is about to create; confirm it. GitHub sends you back to SourceLens, which stores the app credentials.
3. **Install it.** Click "Install on GitHub" and choose the repositories SourceLens may read. You can change this later on GitHub.
4. **Sign in.** Click "Sign in with GitHub". The first person who signs in with the setup token becomes the admin of the instance.
5. **Connect repositories.** On "Repositories", click "Add repositories" and "Connect" next to each repository. Indexing starts at once; the row shows Queued, Indexing and then Ready.
6. **Create an API key.** On "API keys", click "Create key", choose an expiry and which repositories the key may search. The key is shown once. Agents that support OAuth need no key (see [OAuth](#oauth)).
7. **Configure your agent.** The same dialog shows ready-to-paste configuration for Claude Code, Cursor, VS Code and other MCP clients, with the real key filled in (see [Connecting agents](#connecting-agents)).

**On `localhost`** GitHub cannot deliver webhooks, so pushes are not picked up automatically. SourceLens creates the app with its webhook switched off and shows "GitHub cannot reach this address, so pushes are not picked up automatically. Use Sync now." Click "Sync now" on a repository, or "Sync all" on "Repositories", to index the latest commit, or set a schedule under Settings → Sync. Everything else works. For a permanent setup, see [Hosting](#hosting).

Other people sign in after an admin adds their GitHub username under Settings → Members, or with SSO after an admin approves them (see [Members, roles and SSO](#members-roles-and-sso)).

## Connecting agents

The MCP URL is `SOURCELENS_PUBLIC_URL` followed by `/mcp`, for example `https://sourcelens.example.com/mcp`. Agents that support OAuth need only this URL (see [OAuth](#oauth)). Otherwise every request needs the header `Authorization: Bearer <key>`; keys start with `sl_`.

Claude Code (run in a terminal):

```sh
claude mcp add --transport http sourcelens https://sourcelens.example.com/mcp --header "Authorization: Bearer sl_..."
```

Cursor (`~/.cursor/mcp.json`); other MCP clients use the same shape:

```json
{
  "mcpServers": {
    "sourcelens": {
      "url": "https://sourcelens.example.com/mcp",
      "headers": {
        "Authorization": "Bearer sl_..."
      }
    }
  }
}
```

VS Code (`.vscode/mcp.json`):

```json
{
  "servers": {
    "sourcelens": {
      "type": "http",
      "url": "https://sourcelens.example.com/mcp",
      "headers": {
        "Authorization": "Bearer sl_..."
      }
    }
  }
}
```

To check a key from the command line:

```sh
curl -s https://sourcelens.example.com/mcp \
  -H "Authorization: Bearer sl_..." \
  -H "content-type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_repositories","arguments":{}}}'
```

### OAuth

SourceLens is also an OAuth 2.1 authorization server for `/mcp`, so agents that support OAuth connect with the MCP URL alone. Claude Code:

```sh
claude mcp add --transport http sourcelens https://sourcelens.example.com/mcp
```

Then run `/mcp` in Claude Code. A request without credentials gets 401 with a `WWW-Authenticate` header that points to `/.well-known/oauth-protected-resource/mcp`; the agent finds the authorization server there, registers itself (`POST /oauth/register`) and opens `/oauth/authorize` in the browser. After you sign in, SourceLens asks whether the client may search the repositories of this instance as you and names where the browser will be sent back to. "Allow" sends the browser back to the agent, which exchanges the code for tokens at `POST /oauth/token`. If the agent's request is invalid, the page shows the error with a button back to the client; SourceLens never sends the browser there on its own.

- An OAuth token reads all connected repositories, including ones connected later.
- Access tokens expire after 1 hour. The agent renews them with its refresh token, which expires 30 days after its last use; each refresh replaces both tokens. If a refresh token that was already replaced is presented again, SourceLens revokes the grant and the client must be allowed again.
- For members who sign in with SSO, a client can renew its tokens only while the member's last SSO sign-in is at most 30 days old. After that the refresh fails with `invalid_grant` until the member signs in to SourceLens again; the client stays allowed.
- Settings → MCP clients shows the MCP URL and the clients connected with OAuth: members see their own, admins all. "Revoke" stops a client at once.
- API keys keep working. Members who sign in with SSO have no API keys and connect agents only with OAuth.

### Tools

All tools are read-only. Repository names are full names (`owner/repo`), matched case-insensitively within what the key or token may search.

| Tool | What it does |
|---|---|
| `list_repositories` | Lists the repositories the key can search, with branch, indexed commit and file count. |
| `list_files` | Lists the directories and files directly under a path of one repository, with file counts, languages and line counts. Arguments: `repository`, optional `pathPrefix`, `limit` (1–500, default 200). |
| `search_code` | Searches by keywords, identifiers or (with embeddings) meaning. Arguments: `query`, optional `repositories`, `languages`, `pathPrefix`, `limit` (1–20, default 8), `offset` (0–40, default 0). The answer says when more results exist and which `offset` continues. |
| `grep_code` | Finds every line that contains an exact text or matches a regular expression (`regex: true`, Rust regex syntax). Arguments: `pattern`, optional `regex`, `caseSensitive` (default true), `repositories`, `languages`, `pathPrefix`, `limit` (1–200, default 50). A search stops after 5 seconds and then says that it is incomplete. |
| `get_code` | Reads a file or a line range (`startLine`, `endLine`). Without a range it returns the first 400 lines; at most 800 lines per call. |
| `get_file_outline` | Lists the symbols of one file with kinds, signatures and line ranges. |
| `find_symbol` | Finds where a function, class, type or other symbol is defined: exact names first, then prefixes, then substrings. Optional `repositories`, `kind`, `limit` (1–50, default 20). |

Protocol details: stateless Streamable HTTP with JSON responses (`POST /mcp` only; `GET` and `DELETE` answer 405), protocol versions `2025-11-25`, `2025-06-18` and `2025-03-26`. Each key, and each client connected with OAuth, may send 10 requests per second with bursts of up to 20; above that the server answers 429.

At most two `grep_code` searches run at the same time on an instance. A further one waits up to 2 seconds for a free slot and otherwise fails with "Other exhaustive searches are running. Retry in a few seconds."

API keys can access all connected repositories (including ones connected later) or a selected list, and expire after 30 days, 90 days, 1 year or never. A revoked key stops working immediately. Members see, change and revoke only the keys they created; admins manage all keys.

## Search

`search_code` (and the Playground in the UI) combine up to three ways of finding code and merge their rankings:

- **Symbol lookup.** Words that look like identifiers (`getUserById`, `Order.total`, `parse_header`) are looked up in the table of definitions: exact name, then prefix, then substring. A match here counts double.
- **Keyword search.** Full-text search over symbol names, paths and code. Identifiers are also split into their parts, so `user id lookup` finds `getUserById`. Chunks that contain all words get a higher score than chunks that contain only some.
- **Semantic search** (only with embeddings). The query is embedded and compared with the chunk vectors.

The response says which mode ran: `lexical` or `hybrid`. Test files rank lower unless the query mentions tests, and import blocks rank lower. At most three results come from the same file. `offset` continues after the results already returned; each channel contributes a limited number of candidates, so a broad query can run out of results before offset 40.

Results are chunks: a function, a method, a class that fits in one chunk, or a group of small neighbouring declarations. Large classes are split into their members.

### Languages

| Support | Languages |
|---|---|
| Chunks along declarations and symbols (tree-sitter) | C, C++, C#, Go, Java, JavaScript, Kotlin, PHP, Python, Ruby, Rust, Scala, TypeScript (including TSX) |
| Sections and headings as symbols | Markdown |
| Indexed as text, in line windows | Every other text file, for example YAML, JSON, SQL, shell, Terraform, Dockerfiles |

Not indexed: files larger than `SOURCELENS_MAX_FILE_BYTES` (1 MiB by default), binary files, files with the extension of a binary or data format (images, PDFs, archives, fonts, audio and video, compiled objects, model weights, database files, and `.svg`, `.csv`, `.tsv`, `.jsonl`, `.ndjson`, `.snap`, `.ipynb`; these are not downloaded), minified files, generated files (for example `.pb.go`, `_pb2.py`, or a `@generated` / `Code generated … DO NOT EDIT` marker in the first ten lines), lockfiles, vendored directories (`node_modules`, `vendor`, `third_party`, …), symlinks and submodules. Files with such an extension that an earlier version indexed leave the index when the next push is indexed or with "Rebuild index".

## Embeddings

Embeddings are optional. Without them, symbol and keyword search work as described above.

An admin configures them under Settings → Embeddings. SourceLens calls any server that speaks the OpenAI embeddings API (`POST {base URL}/embeddings`), for example OpenAI, Ollama or LM Studio. Fields: base URL, API key (optional), model, dimensions (optional), query prefix, document prefix, batch size (1–256, default 32). "Test connection" checks the settings without saving them. Saving tests them again and records the vector dimensions; on the first save, and after a change that affects the vectors, every connected repository is embedded.

Example with [Ollama](https://ollama.com):

```sh
ollama pull nomic-embed-text
```

| Field | Value |
|---|---|
| Base URL | `http://localhost:11434/v1` when SourceLens runs on the same host outside Docker; `http://host.docker.internal:11434/v1` from a container (on Linux, start the container with `--add-host=host.docker.internal:host-gateway`) |
| Model | `nomic-embed-text` |
| Query prefix | `search_query: ` |
| Document prefix | `search_document: ` |

Changing the base URL, model, dimensions or document prefix re-embeds all chunks. Turning embeddings off keeps the stored vectors; search stops using them. If the provider fails during a search, that search falls back to `lexical` and the response names the reason (`semanticUnavailable`: `timeout`, `provider_error` or `internal`); the Playground shows it. Requests to the embeddings server do not follow redirects. The vectors of the last 256 distinct queries are kept in memory, so a repeated query does not call the provider again.

## Sync

Besides the `push` webhook, index jobs are queued by "Sync now" on a repository, by "Sync all" on "Repositories" (every connected repository; disconnected ones are skipped) and, for every connected repository, at each start. Every signed-in user can sync.

A repository that lost its GitHub access is shown as disconnected and offers "Check access" instead of "Sync now": SourceLens asks GitHub whether the app can read the repository again and, if so, reconnects it and queues a sync. "Rebuild index" downloads every file again and asks for confirmation first.

Under Settings → Sync an admin can also sync all repositories automatically. The schedule is a cron expression with five fields (minute hour day-of-month month day-of-week) in UTC, for example `*/15 * * * *` or `0 2 * * *`; the page shows it in words with the next run. Runs missed while SourceLens was down are not caught up; the jobs queued at start cover them. Like those, a scheduled job that finds nothing changed leaves no entry in the repository's activity, and a scheduled run leaves a sync that is already queued for a repository as it is.

## Members, roles and SSO

There are two roles. Admins manage members and the settings (embeddings, sync schedule, sign-in); members use everything else. Everyone can search all connected repositories, sync them and connect further repositories.

- **GitHub accounts.** The person who sets up the instance signs in with GitHub and is an admin. Admins add further GitHub users by username under Settings → Members; they join as members. GitHub accounts keep their role, so the admin who set up the instance cannot be demoted or removed.
- **SSO.** Under Settings → Sign-in an admin connects one OpenID Connect provider (for example Entra ID, Google, Okta, Keycloak, Authentik or Auth0): button label (at most 24 characters), issuer URL, client ID and client secret. Register the redirect URL shown there, `<public URL>/auth/sso/callback`, with the provider. The issuer must be an `https` URL (`http` only for `localhost`, `127.0.0.1` or `::1`); saving reads `<issuer>/.well-known/openid-configuration` and fails if the provider reports a different issuer or its endpoints do not follow the same rule. The login page then shows "Sign in with <label>". SSO sign-in works only once the instance is set up. The saved client secret is kept only while the issuer and client ID stay the same; after changing either, enter the secret again. Turning SSO off or changing the issuer signs out everyone who signed in with SSO, disconnects their MCP clients and deletes the API keys they created.
- **Approval.** Someone who signs in with SSO for the first time appears under Settings → Members as "Awaiting approval" and gets no session until an admin clicks "Approve"; then they sign in again. "Reject" deletes the request; signing in again creates a new one. Admins see how many people wait for approval on "Repositories" and on the Members tab. The username is the email address if the provider sends `email_verified: true`, else the provider's `preferred_username`, else its subject.
- **Sessions.** A GitHub sign-in lasts 30 days, an SSO sign-in 7 days. Agents that an SSO member connected with OAuth stop renewing their tokens 30 days after that member's last SSO sign-in (see [OAuth](#oauth)).
- **Roles.** Admins can make approved SSO accounts admins ("Make admin") and SSO admins members again ("Make member"). Making an SSO admin a member deletes the API keys that account created. Nobody changes their own role, and admins cannot be removed until they are members again.
- **SSO members** can search, sync, connect any repository the GitHub App was given (they have no GitHub token, so the app's installations decide; suspended installations and ones GitHub answers with an error for are skipped) and connect agents with [OAuth](#oauth). They cannot see or create API keys. SSO admins have every admin right, including API keys.

## Configuration

SourceLens reads its configuration from environment variables (and from a `.env` file in the working directory, if present).

| Variable | Default | Meaning |
|---|---|---|
| `SOURCELENS_PUBLIC_URL` | required | External base URL without trailing slash, for example `https://sourcelens.example.com`. Used for the GitHub App, the GitHub and SSO sign-in callbacks, webhooks, the MCP URL shown in the UI and the OAuth metadata for agents. |
| `SOURCELENS_BIND` | `0.0.0.0:3000` | Listen address. Keep port 3000 in the container image; its health check calls `http://127.0.0.1:3000/healthz`. |
| `SOURCELENS_DATA_DIR` | `./data` (`/data` in the image) | Holds `sourcelens.db`, `secret.key` and `tmp/`. Set to mode 0700 at every start; `tmp/` is emptied at every start. |
| `SOURCELENS_SECRET_KEY` | unset | Base64 of 32 bytes, for example from `openssl rand -base64 32`. Encrypts the GitHub App credentials, GitHub sign-in tokens, the embeddings API key and the SSO client secret in the database, and encrypts and signs the client IDs that agents receive when they register over OAuth. If unset, `secret.key` is created in the data directory on first start and reused. |
| `SOURCELENS_INDEX_CONCURRENCY` | `2` | Index jobs that run in parallel. |
| `SOURCELENS_MAX_FILE_BYTES` | `1048576` | Larger files are not indexed. |
| `SOURCELENS_MAX_ARCHIVE_BYTES` | `2147483648` | Repository archives larger than this fail the index job. Archives (tarballs) are downloaded when more than 200 files changed or GitHub truncates the file tree of a very large repository. |
| `SOURCELENS_DEV_ORIGIN` | unset | Extra browser origin allowed to call the API, for the UI dev server. |
| `SOURCELENS_DEV_MODE` | unset | `1` or `true`: sign-in without GitHub and demo data, for development only (see [Building from source and development](#building-from-source-and-development)). Allowed only with a `localhost` public URL and a loopback listen address, which then defaults to `127.0.0.1:3000`. |
| `RUST_LOG` | `info` | Log filter. At `info`, every request except `/healthz` writes one line with method, path (never the query string), status, latency in milliseconds and a request id (log target `sourcelens::http`). The id is also sent as the `x-request-id` response header. |

### GitHub App from the environment

Instead of the manifest flow on `/setup`, you can create a GitHub App yourself and pass its credentials. Set all six variables or none; a partial set stops the server with `Missing GitHub App variables: …`.

| Variable | Value |
|---|---|
| `SOURCELENS_GITHUB_APP_ID` | The app's numeric ID. |
| `SOURCELENS_GITHUB_APP_SLUG` | The app's URL name (`https://github.com/apps/<slug>`). |
| `SOURCELENS_GITHUB_CLIENT_ID` | The app's client ID. |
| `SOURCELENS_GITHUB_CLIENT_SECRET` | A client secret of the app. |
| `SOURCELENS_GITHUB_WEBHOOK_SECRET` | The webhook secret set in the app. |
| `SOURCELENS_GITHUB_PRIVATE_KEY_PATH` | Path to the app's private key (`.pem`). In a container, mount the file and make it readable for the container user (uid 10001). |

Configure the app like the one SourceLens would create: repository permissions `Contents: read` and `Metadata: read`; events `Push` and `Repository`; callback URL `<public URL>/auth/github/callback`; webhook URL `<public URL>/webhooks/github` with the webhook secret; optionally setup URL `<public URL>/repositories?installed=1`. With these variables set, `/setup` shows step 1 as done; you still install the app and sign in with the setup token.

## Hosting

A production setup needs:

- **A URL that your users' browsers can open.** Set `SOURCELENS_PUBLIC_URL` to it; HTTPS through a reverse proxy that terminates TLS is recommended. It does not have to be reachable from the internet: only the webhook path must be, and only if pushes should be indexed automatically (see [Network exposure](#network-exposure)).
- **The final URL before you create the GitHub App.** The app stores the webhook URL and callback URL from the moment it is created. An app created for a `localhost` URL has its webhook switched off, so set up the production instance with its final URL from the start (a fresh data volume runs the setup again). If the URL changes later, set the new `SOURCELENS_PUBLIC_URL` and update the app on GitHub: `https://github.com/settings/apps/<slug>` for an app on a personal account, `https://github.com/organizations/<org>/settings/apps/<slug>` for an organization app. Change "Homepage URL", "Callback URL", "Setup URL" and, under "Webhook", the webhook URL.
- **A persistent volume for `/data`.** It holds the SQLite database with the indexed source code of every connected repository, and `secret.key`. Treat it like the source code itself: restrict access and back it up. Without `secret.key` (or the same `SOURCELENS_SECRET_KEY`), the stored GitHub App credentials and SSO client secret cannot be decrypted.
- **One instance per team.** Every member can search every connected repository. Members who sign in with GitHub can connect any repository that both they and the GitHub App can read; members who sign in with SSO, any repository the GitHub App can read. Run separate instances for groups that must not see each other's code.
- **HTTPS.** When `SOURCELENS_PUBLIC_URL` starts with `https://`, every response carries `Strict-Transport-Security: max-age=31536000`.

The container listens on port 3000, runs as a non-root user (uid 10001) and has a Docker health check on `GET /healthz`, which answers `ok` while the database responds and 503 otherwise. If you mount a host directory instead of a named volume at `/data`, it must be writable by uid 10001.

### Network exposure

SourceLens can run inside a company network with the UI, the API and the MCP endpoint reachable only from there. The only request that comes from the internet is GitHub's webhook delivery, and that one is optional.

| Endpoint | Called by | Must be reachable from |
|---|---|---|
| `POST /webhooks/github` | GitHub's webhook servers | The internet, only if pushes should be indexed automatically |
| `GET /auth/github/login`, `GET /auth/github/callback` | The user's browser, sent to and back from github.com during sign-in | Users' browsers |
| `GET /auth/sso/login`, `GET /auth/sso/callback` | The user's browser, sent to and back from the SSO provider during sign-in | Users' browsers |
| `GET /auth/github/app-created`, `GET /repositories?installed=1` | The user's browser, sent back from github.com after creating and after installing the GitHub App | Users' browsers |
| `/` and the static assets of the UI, including the consent page `/oauth/authorize` | Users' browsers | Users' browsers |
| `/api/*` | The UI | Users' browsers |
| `POST /mcp` | Agents | Machines that run agents |
| `GET /.well-known/oauth-protected-resource`, `GET /.well-known/oauth-protected-resource/mcp`, `GET /.well-known/oauth-authorization-server`, `POST /oauth/register`, `POST /oauth/token` | Agents that connect with OAuth | Machines that run agents |
| `GET /healthz` | The container health check, load balancers | The host or orchestrator |

- **`/webhooks/github`** checks the `X-Hub-Signature-256` header, an HMAC-SHA256 of the raw body with the app's webhook secret. A request without a valid signature gets 401 (`invalid_signature`); if no webhook secret is configured, every request gets 404 (`webhooks_disabled`). Bodies are limited to 8 MiB, and a repeated delivery ID is answered 200 and ignored. Without webhooks, "Sync now", "Sync all", the optional sync schedule and the index job that runs for every repository at each start keep the index current.
- **Sign-in, app creation and installation** need nothing public. GitHub and the SSO provider only redirect the user's browser to these URLs; the server then exchanges the code through outbound calls. `SOURCELENS_PUBLIC_URL` can therefore be an internal hostname that resolves only inside the network, as long as your users' browsers can open it. The browsers themselves need `github.com` (and `avatars.githubusercontent.com` for avatars in the UI) and, with SSO, the provider.
- **SSO** uses the authorization code flow with PKCE (S256), a nonce and the scopes `openid email profile`. SourceLens checks the ID token's issuer, audience, expiry and nonce but does not verify its signature: it receives the token directly from the provider's token endpoint over TLS with client authentication, which OpenID Connect Core §3.1.3.7 permits.
- **`/api/*`** requires the session cookie set after sign-in (except `GET /api/state`, which the UI reads before sign-in). Requests other than `GET`, `HEAD` and `OPTIONS` must also carry an `Origin` header equal to the origin of `SOURCELENS_PUBLIC_URL` (or `SOURCELENS_DEV_ORIGIN`); others get 403 (`origin_not_allowed`). This covers the consent, which the UI sends as `POST /api/oauth/authorize`.
- **`/mcp`** requires an API key or an OAuth access token; requests from browsers are accepted only from the origin of `SOURCELENS_PUBLIC_URL`. It needs to be reachable from outside only for agents that do not run inside the network, for example hosted agents or CI runners. The key or token is then the only protection. An API key has 256 random bits, is stored as a SHA-256 hash, is limited to the repositories chosen for it, has an optional expiry and is revocable at once. An OAuth access token has 256 random bits, is stored as a SHA-256 hash, reads all connected repositories, expires after 1 hour and is revocable at once. The rate limit counts only requests with valid credentials, so add a rate limit on the reverse proxy and forward nothing but `POST /mcp` from outside (and, for agents that connect with OAuth, the `/.well-known/oauth-*` documents, `POST /oauth/register` and `POST /oauth/token`; the consent page still needs the user's browser to reach the UI).
- **`/oauth/register` and `/oauth/token`** need no session. SourceLens accepts only public clients: no client secret, PKCE with S256 is mandatory, and authorization codes are single-use and expire after 60 seconds. Registration (dynamic client registration) writes nothing to the database: the client ID carries the client's name and redirect URIs, encrypted and signed with the secret key. Redirect URIs must use `https`, `http` only for `localhost`, `127.0.0.1` or `::1`, or an app's own scheme; schemes that run script, open local files, other programs or browser internals are refused, for example `javascript`, `data`, `file`, `ms-*`, `search-ms`, `shell`, `view-source`, `chrome-extension`, `ws`, `mailto` and `intent`. Tokens are stored only as SHA-256 hashes.

Outbound, the server connects to:

| Host | Used for |
|---|---|
| `api.github.com` | GitHub REST API: app and installation tokens, the app's installations, branches, file trees, file contents, repository archives, the signed-in user, member lookup, the conversion of the app manifest |
| `github.com` | OAuth: exchanging the sign-in code for a token and refreshing it (`/login/oauth/access_token`) |
| `codeload.github.com` | Repository archives: GitHub answers the archive request on `api.github.com` with a redirect to this host, which the server follows. Archives are used when more than 200 files changed or GitHub truncates the file tree of a very large repository |
| The embeddings base URL | Embeddings, only if configured; redirects are not followed |
| The SSO provider | The discovery document (`<issuer>/.well-known/openid-configuration`) when an admin saves the SSO settings, and the token endpoint at each SSO sign-in, both without following redirects; only if configured |

The server's HTTP client uses the proxy given in `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` and skips it for hosts listed in `NO_PROXY` (for example an internal embeddings server or SSO provider).

#### Private network with webhooks

To have pushes indexed automatically, let GitHub reach `POST /webhooks/github` and nothing else. The webhook handler does not look at the `Host` header, is not subject to the `Origin` check and does not use `SOURCELENS_PUBLIC_URL`, so the delivery can arrive through any hostname as long as the path, method, headers and body reach the instance unchanged.

(a) **Same hostname, split DNS.** Inside the network, the hostname of `SOURCELENS_PUBLIC_URL` resolves to the instance. On the internet, it resolves to a reverse proxy that forwards only the webhook. A Caddyfile for that proxy (`10.0.0.12:3000` stands for the instance):

```text
sourcelens.example.com {
	@webhook {
		method POST
		path /webhooks/github
	}
	handle @webhook {
		reverse_proxy 10.0.0.12:3000
	}
	handle {
		respond 404
	}
}
```

(b) **Separate public hostname or tunnel**, for example `https://hooks.example.com/webhooks/github` forwarded to the instance. A GitHub App created on `/setup` stores `<SOURCELENS_PUBLIC_URL>/webhooks/github`, so after creating it, change the webhook URL in the app's settings on GitHub (see the settings URLs under the requirements above). Or create the app yourself with the public webhook URL and pass it through the `SOURCELENS_GITHUB_*` variables ([GitHub App from the environment](#github-app-from-the-environment)).

In case (b), Settings → GitHub in SourceLens still shows `<SOURCELENS_PUBLIC_URL>/webhooks/github` as the webhook URL, not the URL configured on GitHub. Its webhook state only reflects the configuration: "Active" whenever a webhook secret exists and `SOURCELENS_PUBLIC_URL` is not `localhost`, `127.0.0.1` or `::1`. "Last delivery" shows whether deliveries actually arrive.

GitHub publishes the source address ranges of webhook deliveries under `hooks` in `https://api.github.com/meta`; use them for an allowlist on the proxy.

#### Private network without webhooks

If GitHub cannot reach the instance at all, SourceLens still works: use "Sync now", "Sync all" or a schedule under Settings → Sync, and every start indexes what changed. Note that an internal hostname is not `localhost`, so the app created on `/setup` asks GitHub for an active webhook at `<SOURCELENS_PUBLIC_URL>/webhooks/github`. GitHub's deliveries to it fail without effect on SourceLens. Settings → GitHub can still show the webhook as "Active", because that state reflects only the configuration; "Last delivery" stays at "No deliveries yet". To stop the failed deliveries, switch the webhook off in the GitHub App settings ("Active" under "Webhook").

### Docker Compose behind Caddy

`docker-compose.yml`:

```yaml
services:
  sourcelens:
    image: ghcr.io/alxbiermann/sourcelens-lite:latest
    environment:
      SOURCELENS_PUBLIC_URL: https://sourcelens.example.com
    volumes:
      - sourcelens-data:/data
    restart: unless-stopped

  caddy:
    image: caddy:2
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data
    restart: unless-stopped

volumes:
  sourcelens-data:
  caddy-data:
```

`Caddyfile` (Caddy obtains the TLS certificate itself once DNS for the name points at the host):

```text
sourcelens.example.com {
	reverse_proxy sourcelens:3000
}
```

```sh
docker compose up -d
docker compose logs sourcelens
```

Then follow the first-run steps from the quick start at `https://sourcelens.example.com/setup`.

The `docker-compose.yml` in this repository is the variant without a proxy: it publishes port 3000, reads `.env`, and pulls the published image (`docker compose up -d`) or builds it from source (`docker compose up -d --build`).

### Coolify

1. Add a new resource of type Docker image: `ghcr.io/alxbiermann/sourcelens-lite:latest`.
2. Set the exposed port to `3000` and the domain, for example `https://sourcelens.example.com`.
3. Add the environment variable `SOURCELENS_PUBLIC_URL=https://sourcelens.example.com` (the same URL as the domain).
4. Add persistent storage (a volume) mounted at `/data`.
5. Enable the health check with path `/healthz` on port `3000`.
6. Deploy, open the logs for the setup token and continue at `https://sourcelens.example.com/setup`.

### Upgrades

Take a [backup](#backup) first, then pull the new image and recreate the container, for example `docker compose pull && docker compose up -d`. The database is migrated automatically at start, in place and only forward; this version brings it to schema version 3 (SSO accounts, scheduled sync, OAuth grants for MCP). When a new version changes how files are chunked, the next index job of every repository rebuilds its index. At every start, SourceLens also queues an index job per connected repository, which picks up pushes made while it was down.

### Backup

Stop the container (or take an atomic snapshot of the volume), then copy the volume:

```sh
docker stop sourcelens
docker run --rm -v sourcelens-data:/data -v "$PWD":/backup debian:trixie-slim \
  tar czf /backup/sourcelens-data.tar.gz -C /data .
docker start sourcelens
```

If you set `SOURCELENS_SECRET_KEY`, keep it with the backup.

## Container image

`ghcr.io/alxbiermann/sourcelens-lite`, built by GitHub Actions (`.github/workflows/docker.yml`) for `linux/amd64` and `linux/arm64`.

| Tag | Built from |
|---|---|
| `latest` | Every push to `main` |
| `sha-<short sha>` | Every push to `main`, one tag per commit |
| `X.Y.Z`, `X.Y` | Git tags `vX.Y.Z` |

## Building from source and development

Prerequisites: the Rust toolchain pinned in `rust-toolchain.toml` (1.96.1 with clippy and rustfmt, for example via `rustup`) and Node.js 26.

Build the UI first; the binary embeds `ui/dist` at compile time (without it, the server answers web pages with "UI not built"):

```sh
npm --prefix ui ci
npm --prefix ui run build
cp .env.example .env    # then edit SOURCELENS_PUBLIC_URL if needed
cargo run --release
```

Tests and lints:

```sh
cargo test
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

UI development with hot reload: the Vite dev server runs on `http://127.0.0.1:5173` and proxies `/api`, `/auth`, `/mcp`, `/webhooks`, `/.well-known`, `/oauth/register` and `/oauth/token` to the server on `http://127.0.0.1:3000`. The server accepts state-changing API calls only from its own origin or `SOURCELENS_DEV_ORIGIN`:

```sh
# terminal 1
SOURCELENS_PUBLIC_URL=http://127.0.0.1:3000 SOURCELENS_DEV_ORIGIN=http://127.0.0.1:5173 cargo run

# terminal 2
npm --prefix ui run dev
```

Use `127.0.0.1` (not `localhost`) in `SOURCELENS_PUBLIC_URL` so the session cookie set after sign-in is also sent to the dev server; cookies do not depend on the port. The GitHub App's callback URL must use the same host.

Dev mode shows every screen without a GitHub App and without GitHub or SSO sign-in:

```sh
SOURCELENS_DEV_MODE=1 SOURCELENS_PUBLIC_URL=http://localhost:3000 SOURCELENS_DATA_DIR=./data/dev cargo run
```

Then open `http://localhost:3000/login`. The setup is skipped, and the sign-in page offers "Sign in as admin" and "Sign in as SSO member" (the dev accounts `dev-admin@example.com` and `dev-member@example.com`). On an empty database, dev mode adds demo data: members in every state (two of them awaiting approval), the repositories `acme/api` and `acme/web` with a small index to search in the Playground, `acme/legacy` without GitHub access, finished jobs, two MCP clients and an API key. A database that already has users only gets the two dev accounts.

Limits: without a GitHub App, adding, syncing and changing the branch of repositories fail with "The GitHub App is not configured yet."; after "Sync now" a demo repository shows "Failed" (its index stays searchable). No startup jobs are queued in dev mode. Embeddings work only when configured under Settings → Embeddings. To combine dev mode with the Vite dev server, add `SOURCELENS_DEV_ORIGIN` as above and use `127.0.0.1` in `SOURCELENS_PUBLIC_URL`.

Anyone who can reach a dev-mode server can sign in as admin. SourceLens therefore starts in dev mode only when `SOURCELENS_PUBLIC_URL` points to this computer (`localhost`, `127.0.0.1` or `[::1]`) and `SOURCELENS_BIND` is a loopback address (the default becomes `127.0.0.1:3000`; `0.0.0.0` is refused, so change the `SOURCELENS_BIND` line of a `.env` copied from `.env.example`). Never use it for a real instance or on a real instance's data directory.

## Limits

- GitHub only (github.com). No GitLab, Bitbucket or GitHub Enterprise Server.
- Read-only. SourceLens never writes to GitHub, and the MCP tools only read the index.
- One branch per repository.
- Indexing on push needs GitHub to reach `POST /webhooks/github`. Without that, use "Sync now", "Sync all" or a sync schedule (cron, at most once a minute).
- Failed index jobs are not retried automatically. The next push, the next scheduled sync, a restart or "Sync now" runs the job again. The repository's activity shows the error; for an internal error it reads "Internal error: " followed by the cause.
- Members share access: every member can search all connected repositories, and an OAuth token for MCP reads all of them. Only admins manage members and settings.
- SSO with one OpenID Connect provider that offers discovery and a client secret. Plain OAuth 2 providers without OpenID Connect are not supported.
- Embeddings only through the OpenAI-compatible `/embeddings` API.
- Early version (0.1.0). Expect changes.

## License

Apache License 2.0. See [LICENSE](LICENSE).
