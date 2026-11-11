//! Read-only MCP server: stateless Streamable HTTP with JSON responses only.

use std::borrow::Cow;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::crypto::sha256_hex;
use crate::error::{ApiError, ApiResult};
use crate::http::keys::KEY_PREFIX;
use crate::repositories::{self, RepoRow, CONNECTED_IDS};
use crate::search::symbols::{self, SymbolRow};
use crate::search::{self, Filter, SearchHit, SearchRequest};
use crate::state::{lock, now, AppState, SECS_PER_DAY};

const SUPPORTED_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const LATEST_VERSION: &str = SUPPORTED_VERSIONS[0];

const INSTRUCTIONS: &str = "SourceLens searches indexed GitHub repositories. Start with list_repositories, then search_code. Results give repository, path and line range; read more with get_code. Use find_symbol for a definition by name and get_file_outline before reading a large file.";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const UNAUTHORIZED: i64 = -32001;
/// Implementation-defined server error: foreign origin, rate limit.
const SERVER_ERROR: i64 = -32000;

const SEARCH_TEXT_BUDGET: usize = 24_000;
const SEARCH_PREVIEW_LINES: usize = 6;
const SEARCH_PREVIEW_CHARS: usize = 1_500;
/// Longer lines of a search hit or a `get_code` page are cut, in the text and in
/// `structuredContent`, so one minified or data line cannot flood the agent's context.
const MAX_LINE_CHARS: usize = 400;
const GET_CODE_DEFAULT_LINES: usize = 400;
const GET_CODE_MAX_LINES: usize = 800;
const FIND_SYMBOL_DEFAULT: usize = 20;
const FIND_SYMBOL_MAX: usize = 50;
/// `api_keys.last_used_at` is written at most this often per key.
const TOUCH_INTERVAL: Duration = Duration::from_secs(60);

/// `/mcp` (POST; GET/DELETE → 405).
pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/mcp",
        post(handle)
            .get(method_not_allowed)
            .delete(method_not_allowed),
    )
}

async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, HeaderValue::from_static("POST"))],
    )
        .into_response()
}

async fn handle(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    match serve(&state, &headers, &body).await {
        Ok(response) => response,
        Err(reject) => reject.into_response(),
    }
}

async fn serve(state: &AppState, headers: &HeaderMap, body: &[u8]) -> Result<Response, Reject> {
    check_origin(state, headers)?;
    let scope = authorize(state, headers).await?;
    state
        .mcp_rate
        .check(scope.key_id)
        .map_err(Reject::RateLimited)?;
    check_version(headers)?;
    let message: Value = serde_json::from_slice(body)
        .map_err(|_| Reject::BadRequest(PARSE_ERROR, "Parse error: body is not valid JSON"))?;
    let Some(Envelope { id, method, params }) = parse_envelope(&message)? else {
        // A notification, or a response to a server request (this server sends none).
        return Ok(StatusCode::ACCEPTED.into_response());
    };

    let result = match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(state, &scope, params).await,
        _ => Err((METHOD_NOT_FOUND, format!("Method not found: {method}"))),
    };
    Ok(match result {
        Ok(result) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response(),
        Err((code, message)) => rpc_error(StatusCode::OK, id.clone(), code, &message),
    })
}

/// A request answered before it reaches a method.
enum Reject {
    Origin,
    Unauthorized,
    /// Seconds until the key may send again.
    RateLimited(u64),
    BadRequest(i64, &'static str),
    Internal(ApiError),
}

impl IntoResponse for Reject {
    fn into_response(self) -> Response {
        match self {
            Self::Origin => rpc_error(
                StatusCode::FORBIDDEN,
                Value::Null,
                SERVER_ERROR,
                "Forbidden: origin not allowed",
            ),
            Self::Unauthorized => {
                let mut response = rpc_error(
                    StatusCode::UNAUTHORIZED,
                    Value::Null,
                    UNAUTHORIZED,
                    "Unauthorized: invalid or missing API key",
                );
                response.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Bearer realm=\"sourcelens\""),
                );
                response
            }
            Self::RateLimited(seconds) => {
                let mut response = rpc_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    Value::Null,
                    SERVER_ERROR,
                    &format!("Rate limit exceeded. Retry in {seconds} s."),
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
                response
            }
            Self::BadRequest(code, message) => {
                rpc_error(StatusCode::BAD_REQUEST, Value::Null, code, message)
            }
            Self::Internal(err) => rpc_error(err.status, Value::Null, INTERNAL_ERROR, &err.message),
        }
    }
}

fn rpc_error(status: StatusCode, id: Value, code: i64, message: &str) -> Response {
    let body = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    (status, Json(body)).into_response()
}

/// Browsers may only call `/mcp` from the public origin; clients without `Origin` are not browsers.
fn check_origin(state: &AppState, headers: &HeaderMap) -> Result<(), Reject> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let allowed = origin
        .to_str()
        .is_ok_and(|o| o.trim_end_matches('/') == state.cfg.public_origin());
    if allowed {
        Ok(())
    } else {
        Err(Reject::Origin)
    }
}

async fn authorize(state: &AppState, headers: &HeaderMap) -> Result<KeyScope, Reject> {
    let secret = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, token) = v.trim().split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        })
        .ok_or(Reject::Unauthorized)?;
    authenticate(state, secret).await.map_err(|err| {
        if err.status == StatusCode::UNAUTHORIZED {
            Reject::Unauthorized
        } else {
            Reject::Internal(err)
        }
    })
}

fn check_version(headers: &HeaderMap) -> Result<(), Reject> {
    let Some(version) = headers.get("mcp-protocol-version") else {
        return Ok(());
    };
    let supported = version
        .to_str()
        .is_ok_and(|v| SUPPORTED_VERSIONS.contains(&v.trim()));
    if supported {
        Ok(())
    } else {
        Err(Reject::BadRequest(
            INVALID_REQUEST,
            "Unsupported MCP-Protocol-Version header",
        ))
    }
}

/// A JSON-RPC request that expects an answer.
struct Envelope<'a> {
    id: &'a Value,
    method: &'a str,
    params: &'a Value,
}

/// `None` for notifications and responses, which get no answer.
fn parse_envelope(message: &Value) -> Result<Option<Envelope<'_>>, Reject> {
    let invalid = |message| Reject::BadRequest(INVALID_REQUEST, message);
    let message = message
        .as_object()
        .ok_or_else(|| invalid("Invalid request: expected one JSON-RPC message object"))?;
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid("Invalid request: jsonrpc must be \"2.0\""));
    }
    let Some(id) = message.get("id") else {
        return Ok(None);
    };
    let Some(method) = message.get("method") else {
        if message.contains_key("result") || message.contains_key("error") {
            return Ok(None);
        }
        return Err(invalid("Invalid request: method is missing"));
    };
    if !(id.is_string() || id.is_i64() || id.is_u64()) {
        return Err(invalid(
            "Invalid request: id must be a string or an integer",
        ));
    }
    let method = method
        .as_str()
        .ok_or_else(|| invalid("Invalid request: method must be a string"))?;
    Ok(Some(Envelope {
        id,
        method,
        params: message.get("params").unwrap_or(&Value::Null),
    }))
}

/// What an API key may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyScope {
    pub key_id: i64,
    /// Connected repositories in the key's scope, ascending.
    pub repo_ids: Vec<i64>,
}

/// Resolves an API key secret (`sl_…`) to the connected repositories it may read.
pub async fn authenticate(state: &AppState, secret: &str) -> ApiResult<KeyScope> {
    let secret = secret.trim();
    if !secret.starts_with(KEY_PREFIX) {
        return Err(invalid_key());
    }
    let key_hash = sha256_hex(secret.as_bytes());
    let found = state
        .db
        .read(move |conn| {
            let row: Option<(i64, bool, Option<i64>)> = conn
                .query_row(
                    "SELECT id, all_repos, expires_at FROM api_keys WHERE key_hash = ?1",
                    [&key_hash],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((key_id, all_repos, expires_at)) = row else {
                return Ok(None);
            };
            if expires_at.is_some_and(|at| at <= now()) {
                return Ok(None);
            }
            let repo_ids = if all_repos {
                repositories::connected_ids(conn)?
            } else {
                conn.prepare(&format!(
                    "SELECT repo_id FROM api_key_repos
                     WHERE key_id = ?1 AND repo_id IN ({CONNECTED_IDS})
                     ORDER BY repo_id"
                ))?
                .query_map([key_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?
            };
            Ok::<_, rusqlite::Error>(Some(KeyScope { key_id, repo_ids }))
        })
        .await?;
    let scope = found.ok_or_else(invalid_key)?;
    touch(state, scope.key_id);
    Ok(scope)
}

/// Records `last_used_at` at most once per [`TOUCH_INTERVAL`] per key, in the background: it is
/// bookkeeping, and the writer may be busy with an index batch.
fn touch(state: &AppState, key_id: i64) {
    let due = {
        let mut touched = lock(&state.key_touch);
        let now = Instant::now();
        let due = touched
            .get(&key_id)
            .is_none_or(|last| now.duration_since(*last) >= TOUCH_INTERVAL);
        if due {
            touched.insert(key_id, now);
        }
        due
    };
    if !due {
        return;
    }
    let db = state.db.clone();
    tokio::spawn(async move {
        let written: rusqlite::Result<usize> = db
            .write(move |conn| {
                conn.execute(
                    "UPDATE api_keys SET last_used_at = ?1 WHERE id = ?2",
                    params![now(), key_id],
                )
            })
            .await;
        if let Err(err) = written {
            tracing::warn!("Could not record the use of API key {key_id}: {err}");
        }
    });
}

fn invalid_key() -> ApiError {
    ApiError::unauthorized("invalid_api_key", "Invalid, expired or revoked API key.")
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| SUPPORTED_VERSIONS.contains(v))
        .unwrap_or(LATEST_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "sourcelens", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

/// A tool's contract: `properties` is both the advertised schema and the set of accepted arguments.
struct ToolSpec {
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &'static [&'static str],
}

fn tool_specs() -> [ToolSpec; 5] {
    let repositories = json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "Repository full names (owner/repo) to search in. Default: all.",
    });
    let repository =
        json!({ "type": "string", "description": "Repository full name, e.g. owner/repo." });
    let path = json!({ "type": "string", "description": "File path inside the repository." });
    [
        ToolSpec {
            name: "list_repositories",
            description: "List the repositories this API key can search, with branch and index state.",
            properties: json!({}),
            required: &[],
        },
        ToolSpec {
            name: "search_code",
            description: "Search code by meaning, keywords or identifiers across indexed repositories. Returns the best matching code blocks with repository, path and line range.",
            properties: json!({
                "query": { "type": "string", "description": "What to look for: words, identifiers or a description." },
                "repositories": repositories,
                "languages": { "type": "array", "items": { "type": "string" }, "description": "Language names, e.g. rust, typescript." },
                "pathPrefix": { "type": "string", "description": "Only files whose path starts with this prefix." },
                "limit": { "type": "integer", "minimum": 1, "maximum": search::MAX_LIMIT, "default": search::DEFAULT_LIMIT },
            }),
            required: &["query"],
        },
        ToolSpec {
            name: "get_code",
            description: "Read a file or a line range from an indexed repository.",
            properties: json!({
                "repository": repository,
                "path": path,
                "startLine": { "type": "integer", "minimum": 1, "description": "First line, 1-based. Default 1." },
                "endLine": { "type": "integer", "minimum": 1, "description": "Last line, inclusive." },
            }),
            required: &["repository", "path"],
        },
        ToolSpec {
            name: "get_file_outline",
            description: "List the symbols of one file with kinds, signatures and line ranges.",
            properties: json!({ "repository": repository, "path": path }),
            required: &["repository", "path"],
        },
        ToolSpec {
            name: "find_symbol",
            description: "Find where a function, class, type or other symbol is defined. Matches exact names first, then prefixes, then substrings.",
            properties: json!({
                "name": { "type": "string", "description": "Symbol name, optionally qualified (Type.method, mod::fn)." },
                "repositories": repositories,
                "kind": { "type": "string", "description": "Symbol kind, e.g. function, method, class, struct, constant. Markdown headings are only returned for kind heading (any level) or heading_1 … heading_6." },
                "limit": { "type": "integer", "minimum": 1, "maximum": FIND_SYMBOL_MAX, "default": FIND_SYMBOL_DEFAULT },
            }),
            required: &["name"],
        },
    ]
}

fn tool_definitions() -> Value {
    let tools: Vec<Value> = tool_specs()
        .into_iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": {
                    "type": "object",
                    "properties": spec.properties,
                    "required": spec.required,
                    "additionalProperties": false,
                },
                "annotations": { "readOnlyHint": true },
            })
        })
        .collect();
    Value::Array(tools)
}

/// Failure of a `tools/call`: invalid arguments become a JSON-RPC error, everything else an `isError` result.
enum ToolError {
    Params(String),
    Failed(String),
}

impl From<ApiError> for ToolError {
    fn from(err: ApiError) -> Self {
        Self::Failed(err.message)
    }
}

impl From<rusqlite::Error> for ToolError {
    fn from(err: rusqlite::Error) -> Self {
        ApiError::from(err).into()
    }
}

struct ToolOutput {
    text: String,
    structured: Value,
}

impl ToolOutput {
    fn new(text: String, structured: impl Serialize) -> Result<Self, ToolError> {
        let structured = serde_json::to_value(structured).map_err(ApiError::internal)?;
        Ok(Self { text, structured })
    }
}

async fn call_tool(
    state: &AppState,
    scope: &KeyScope,
    params: &Value,
) -> Result<Value, (i64, String)> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err((INVALID_PARAMS, "Missing tool name in params.name".into()));
    };
    let empty = Map::new();
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(map)) => map,
        Some(_) => return Err((INVALID_PARAMS, "params.arguments must be an object".into())),
    };
    let Some(spec) = tool_specs().into_iter().find(|spec| spec.name == name) else {
        return Err((INVALID_PARAMS, format!("Unknown tool: {name}")));
    };
    let args = Args::new(arguments, &spec.properties)?;
    let output = match spec.name {
        "list_repositories" => list_repositories(state, scope).await,
        "search_code" => search_code(state, scope, args).await,
        "get_code" => get_code(state, scope, args).await,
        "get_file_outline" => get_file_outline(state, scope, args).await,
        _ => find_symbol(state, scope, args).await,
    };
    match output {
        Ok(output) => Ok(json!({
            "content": [{ "type": "text", "text": output.text }],
            "structuredContent": output.structured,
            "isError": false,
        })),
        Err(ToolError::Params(message)) => Err((INVALID_PARAMS, message)),
        Err(ToolError::Failed(message)) => Ok(json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        })),
    }
}

struct Args<'a>(&'a Map<String, Value>);

impl<'a> Args<'a> {
    /// Rejects arguments that are not in the tool's schema `properties`.
    fn new(map: &'a Map<String, Value>, properties: &Value) -> Result<Self, (i64, String)> {
        let known = |key: &str| properties.get(key).is_some();
        match map.keys().find(|key| !known(key)) {
            Some(key) => Err((INVALID_PARAMS, format!("Unknown argument \"{key}\""))),
            None => Ok(Self(map)),
        }
    }

    fn value(&self, name: &str) -> Option<&'a Value> {
        self.0.get(name).filter(|v| !v.is_null())
    }

    fn string(&self, name: &str) -> Result<&'a str, ToolError> {
        self.opt_string(name)?
            .ok_or_else(|| ToolError::Params(format!("Missing required argument \"{name}\"")))
    }

    fn opt_string(&self, name: &str) -> Result<Option<&'a str>, ToolError> {
        match self.value(name) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(ToolError::Params(format!(
                "Argument \"{name}\" must be a string"
            ))),
        }
    }

    fn strings(&self, name: &str) -> Result<Vec<&'a str>, ToolError> {
        let invalid =
            || ToolError::Params(format!("Argument \"{name}\" must be an array of strings"));
        match self.value(name) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| item.as_str().ok_or_else(invalid))
                .collect(),
            Some(_) => Err(invalid()),
        }
    }

    fn integer(&self, name: &str) -> Result<Option<i64>, ToolError> {
        let Some(value) = self.value(name) else {
            return Ok(None);
        };
        let number = value.as_i64().or_else(|| {
            value
                .as_f64()
                .filter(|f| f.fract() == 0.0 && f.abs() < 1e15)
                .map(|f| f as i64)
        });
        number
            .map(Some)
            .ok_or_else(|| ToolError::Params(format!("Argument \"{name}\" must be an integer")))
    }

    /// The integer argument `name` clamped to `1..=max`, `default` when absent.
    fn limit(&self, name: &str, default: usize, max: usize) -> Result<usize, ToolError> {
        let limit = match self.integer(name)? {
            Some(n) => usize::try_from(n.max(1)).unwrap_or(usize::MAX),
            None => default,
        };
        Ok(limit.clamp(1, max))
    }
}

/// Connected repositories of the key's scope, ordered by name.
async fn scope_repositories(state: &AppState, scope: &KeyScope) -> Result<Vec<RepoRow>, ToolError> {
    let ids = scope.repo_ids.clone();
    Ok(state
        .db
        .read(move |conn| repositories::connected_in(conn, &ids))
        .await?)
}

fn resolve<'r>(repos: &'r [RepoRow], name: &str) -> Result<&'r RepoRow, ToolError> {
    let name = name.trim();
    repos
        .iter()
        .find(|repo| repo.full_name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let available = if repos.is_empty() {
                "none".to_string()
            } else {
                repos
                    .iter()
                    .map(|repo| repo.full_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            ToolError::Failed(format!(
                "Unknown repository \"{name}\". Available: {available}."
            ))
        })
}

/// Repository ids for an optional `repositories` argument; empty means the whole scope.
fn narrow(repos: &[RepoRow], names: &[&str]) -> Result<Vec<i64>, ToolError> {
    if names.is_empty() {
        return Ok(repos.iter().map(|repo| repo.id).collect());
    }
    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        let id = resolve(repos, name)?.id;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListedRepository<'a> {
    name: &'a str,
    branch: &'a str,
    indexed_commit: Option<&'a str>,
    indexed_at: Option<i64>,
    file_count: i64,
}

async fn list_repositories(state: &AppState, scope: &KeyScope) -> Result<ToolOutput, ToolError> {
    let repos = scope_repositories(state, scope).await?;
    let mut lines = Vec::with_capacity(repos.len());
    let mut listed = Vec::with_capacity(repos.len());
    for repo in &repos {
        let index_state = match (&repo.indexed_commit, repo.indexed_at) {
            (Some(commit), at) => {
                let date = at.map(|at| format!(" ({})", date(at))).unwrap_or_default();
                format!(
                    "indexed {}{date}  {} files",
                    short_commit(commit),
                    repo.file_count
                )
            }
            (None, _) => "not indexed yet".to_string(),
        };
        lines.push(format!(
            "{}  branch {}  {index_state}",
            repo.full_name,
            repo.effective_branch()
        ));
        listed.push(ListedRepository {
            name: &repo.full_name,
            branch: repo.effective_branch(),
            indexed_commit: repo.indexed_commit.as_deref(),
            indexed_at: repo.indexed_at,
            file_count: repo.file_count,
        });
    }
    let text = if lines.is_empty() {
        "No repositories are available to this API key. Connect repositories in SourceLens or widen the key's access.".to_string()
    } else {
        lines.join("\n")
    };
    ToolOutput::new(text, json!({ "repositories": listed }))
}

async fn search_code(
    state: &AppState,
    scope: &KeyScope,
    args: Args<'_>,
) -> Result<ToolOutput, ToolError> {
    let query = args.string("query")?;
    let repository_names = args.strings("repositories")?;
    let languages = args.strings("languages")?;
    let path_prefix = args.opt_string("pathPrefix")?;
    let limit = args.limit("limit", search::DEFAULT_LIMIT, search::MAX_LIMIT)?;

    let repos = scope_repositories(state, scope).await?;
    let repo_ids = narrow(&repos, &repository_names)?;
    let response = search::search(
        state,
        SearchRequest {
            query: query.to_string(),
            filter: Filter::new(repo_ids, languages, path_prefix)?,
            limit: Some(limit),
        },
    )
    .await?;
    let hits: Vec<SearchHit> = response
        .hits
        .into_iter()
        .map(|hit| SearchHit {
            content: cap_lines(&hit.content),
            ..hit
        })
        .collect();

    let count = hits.len();
    let noun = if count == 1 { "result" } else { "results" };
    let mut text = format!(
        "{count} {noun} for \"{}\" ({})",
        query.trim(),
        response.mode
    );
    let mut used = text.chars().count();
    let mut truncating = false;
    for (n, hit) in hits.iter().enumerate() {
        let heading = format!(
            "\n\n{}. {} {}:{}-{}  {}\n",
            n + 1,
            hit.repository,
            hit.path,
            hit.start_line,
            hit.end_line,
            hit_label(hit)
        );
        let full = fenced(hit.language.as_deref(), &hit.content);
        let block_len = heading.chars().count() + full.chars().count();
        if !truncating && used + block_len > SEARCH_TEXT_BUDGET {
            truncating = true;
        }
        text.push_str(&heading);
        if truncating {
            text.push_str(&preview(hit.language.as_deref(), &hit.content));
        } else {
            text.push_str(&full);
        }
        used += block_len;
    }
    ToolOutput::new(text, json!({ "mode": response.mode, "results": hits }))
}

/// `kind symbol_path`; chunks that group declarations (`block`, `block_part`) have no symbol
/// path and show the names of the symbols they contain instead.
fn hit_label(hit: &SearchHit) -> String {
    let name = if matches!(hit.kind.as_str(), "block" | "block_part") && !hit.names.is_empty() {
        hit.names.join(", ")
    } else {
        hit.symbol_path.clone()
    };
    format!("{} {name}", hit.kind).trim_end().to_string()
}

/// Cuts every line longer than [`MAX_LINE_CHARS`] to `… [+N chars]`.
fn cap_lines(content: &str) -> String {
    content
        .split('\n')
        .map(cap_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn cap_line(line: &str) -> Cow<'_, str> {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        None => Cow::Borrowed(line),
        Some((at, _)) => {
            let rest = line[at..].chars().count();
            Cow::Owned(format!("{}… [+{rest} chars]", &line[..at]))
        }
    }
}

/// The first [`SEARCH_PREVIEW_LINES`] lines, fewer when they exceed [`SEARCH_PREVIEW_CHARS`]
/// (always at least one line).
fn preview(language: Option<&str>, content: &str) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut shown = 0;
    let mut chars = 0;
    for line in &lines {
        let len = line.chars().count() + 1;
        if shown == SEARCH_PREVIEW_LINES || (shown > 0 && chars + len > SEARCH_PREVIEW_CHARS) {
            break;
        }
        shown += 1;
        chars += len;
    }
    if shown == lines.len() {
        return fenced(language, content);
    }
    format!(
        "{}\n… {} more lines, read with get_code",
        fenced(language, &lines[..shown].join("\n")),
        lines.len() - shown
    )
}

struct FileRow {
    id: i64,
    language: Option<String>,
    skip_reason: Option<String>,
    content: Option<Vec<u8>>,
}

/// Loads a file of a repository in scope, or the `isError` text that tells the agent what to do.
async fn load_file(
    state: &AppState,
    repo: &RepoRow,
    path: &str,
    with_content: bool,
) -> Result<FileRow, ToolError> {
    let repo_id = repo.id;
    let lookup_path = path.to_string();
    let row = state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT id, language, skip_reason, CASE WHEN ?3 THEN content END
                 FROM files WHERE repo_id = ?1 AND path = ?2",
                params![repo_id, lookup_path, with_content],
                |r| {
                    Ok(FileRow {
                        id: r.get(0)?,
                        language: r.get(1)?,
                        skip_reason: r.get(2)?,
                        content: r.get(3)?,
                    })
                },
            )
            .optional()
        })
        .await?;
    match row {
        None => Err(ToolError::Failed(format!(
            "No file \"{path}\" in {}. Use search_code or get_file_outline to find paths.",
            repo.full_name
        ))),
        Some(FileRow {
            skip_reason: Some(reason),
            ..
        }) => Err(ToolError::Failed(format!(
            "\"{path}\" is not indexed ({reason})."
        ))),
        Some(row) => Ok(row),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CodeRange<'a> {
    repository: &'a str,
    path: &'a str,
    start_line: usize,
    end_line: usize,
    line_count: usize,
    commit: Option<&'a str>,
    content: &'a str,
}

/// Lines `start..=end` to return and the `wanted_end` the caller asked for (more than one page
/// when `end < wanted_end`).
struct Page {
    start: usize,
    end: usize,
    wanted_end: usize,
}

/// Clamps the requested range to the file and to one page: [`GET_CODE_DEFAULT_LINES`] without
/// `endLine` (the rest of the file, served in pages), else at most [`GET_CODE_MAX_LINES`].
fn page(
    path: &str,
    start_arg: Option<i64>,
    end_arg: Option<i64>,
    line_count: usize,
) -> Result<Page, ToolError> {
    let start = to_line(start_arg.unwrap_or(1));
    if start > line_count {
        return Err(ToolError::Failed(format!(
            "\"{path}\" has {line_count} lines; startLine {start} is past the end."
        )));
    }
    let wanted_end = end_arg.map_or(line_count, to_line).min(line_count);
    let page_lines = if end_arg.is_some() {
        GET_CODE_MAX_LINES
    } else {
        GET_CODE_DEFAULT_LINES
    };
    Ok(Page {
        start,
        end: wanted_end.min(start + page_lines - 1),
        wanted_end,
    })
}

async fn get_code(
    state: &AppState,
    scope: &KeyScope,
    args: Args<'_>,
) -> Result<ToolOutput, ToolError> {
    let repository = args.string("repository")?;
    let path = normalize_path(args.string("path")?);
    let start_arg = args.integer("startLine")?;
    let end_arg = args.integer("endLine")?;
    if let (Some(start), Some(end)) = (start_arg, end_arg) {
        if end < start {
            return Err(ToolError::Params(
                "Argument \"endLine\" must not be smaller than \"startLine\"".into(),
            ));
        }
    }

    let repos = scope_repositories(state, scope).await?;
    let repo = resolve(&repos, repository)?;
    let file = load_file(state, repo, path, true).await?;
    let content = String::from_utf8_lossy(file.content.as_deref().unwrap_or_default());
    let lines = split_lines(&content);
    let line_count = lines.len();
    let commit = repo.indexed_commit.as_deref();
    let at_commit = commit
        .map(|c| format!(" @ {}", short_commit(c)))
        .unwrap_or_default();

    if line_count == 0 {
        let empty = CodeRange {
            repository: &repo.full_name,
            path,
            start_line: 0,
            end_line: 0,
            line_count: 0,
            commit,
            content: "",
        };
        return ToolOutput::new(
            format!("{} {path} is empty{at_commit}.", repo.full_name),
            empty,
        );
    }

    let Page {
        start,
        end,
        wanted_end,
    } = page(path, start_arg, end_arg, line_count)?;
    let code = lines[start - 1..end]
        .iter()
        .map(|line| cap_line(line))
        .collect::<Vec<_>>()
        .join("\n");
    let mut text = format!(
        "{} {path}:{start}-{end} of {line_count} lines{at_commit}\n{}",
        repo.full_name,
        fenced(file.language.as_deref(), &code)
    );
    if end < wanted_end {
        text.push_str(&format!(
            "\nLines {start}-{end} of {line_count}. Call get_code with startLine={} for more.",
            end + 1
        ));
    }
    let range = CodeRange {
        repository: &repo.full_name,
        path,
        start_line: start,
        end_line: end,
        line_count,
        commit,
        content: &code,
    };
    ToolOutput::new(text, range)
}

async fn get_file_outline(
    state: &AppState,
    scope: &KeyScope,
    args: Args<'_>,
) -> Result<ToolOutput, ToolError> {
    let repository = args.string("repository")?;
    let path = normalize_path(args.string("path")?);
    let repos = scope_repositories(state, scope).await?;
    let repo = resolve(&repos, repository)?;
    let file = load_file(state, repo, path, false).await?;
    let file_id = file.id;
    let symbols = state
        .db
        .read(move |conn| symbols::outline(conn, file_id))
        .await?;

    let text = if symbols.is_empty() {
        format!("No symbols in {path}. Read it with get_code.")
    } else {
        symbols
            .iter()
            .map(|s| {
                let indent = "  ".repeat(usize::try_from(s.depth).unwrap_or(0));
                format!(
                    "{indent}L{}-{} {} {}{}",
                    s.start_line,
                    s.end_line,
                    s.kind,
                    s.name,
                    signature_suffix(s)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    ToolOutput::new(
        text,
        json!({ "repository": repo.full_name, "path": path, "symbols": symbols }),
    )
}

fn signature_suffix(symbol: &SymbolRow) -> String {
    symbol
        .signature
        .as_deref()
        .map(|sig| format!(" — {sig}"))
        .unwrap_or_default()
}

#[derive(Serialize)]
struct FoundSymbol<'a> {
    #[serde(flatten)]
    symbol: &'a SymbolRow,
    repository: &'a str,
    path: &'a str,
}

async fn find_symbol(
    state: &AppState,
    scope: &KeyScope,
    args: Args<'_>,
) -> Result<ToolOutput, ToolError> {
    let name = args.string("name")?.trim().to_string();
    let repository_names = args.strings("repositories")?;
    let kind = args
        .opt_string("kind")?
        .map(|k| k.trim().to_ascii_lowercase())
        .filter(|k| !k.is_empty());
    let limit = args.limit("limit", FIND_SYMBOL_DEFAULT, FIND_SYMBOL_MAX)?;
    let repos = scope_repositories(state, scope).await?;
    let repo_ids = narrow(&repos, &repository_names)?;
    if name.is_empty() {
        return Err(ToolError::Failed(
            "Enter a symbol name, for example find_symbol with name \"parseConfig\".".into(),
        ));
    }

    let filter = Filter {
        repo_ids,
        ..Filter::default()
    };
    let lookup_name = name.clone();
    let rows = state
        .db
        .read(move |conn| symbols::lookup(conn, &filter, &lookup_name, kind.as_deref(), limit))
        .await?;

    let mut text = Vec::with_capacity(rows.len());
    let mut found = Vec::with_capacity(rows.len());
    for (n, row) in rows.iter().enumerate() {
        let repository = repos
            .iter()
            .find(|repo| repo.id == row.repo_id)
            .map_or("", |repo| repo.full_name.as_str());
        text.push(format!(
            "{}. {} {} — {repository} {}:{}-{}",
            n + 1,
            row.kind,
            row.qualified_name,
            row.path,
            row.start_line,
            row.end_line
        ));
        if let Some(signature) = &row.signature {
            text.push(format!("   {signature}"));
        }
        found.push(FoundSymbol {
            symbol: row,
            repository,
            path: &row.path,
        });
    }
    let text = if text.is_empty() {
        format!("No symbol named \"{name}\" found. Try search_code with a description or related words.")
    } else {
        text.join("\n")
    };
    ToolOutput::new(text, json!({ "symbols": found }))
}

fn normalize_path(path: &str) -> &str {
    let mut path = path.trim();
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    path.trim_start_matches('/')
}

fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    content
        .strip_suffix('\n')
        .unwrap_or(content)
        .split('\n')
        .collect()
}

fn to_line(n: i64) -> usize {
    usize::try_from(n.max(1)).unwrap_or(usize::MAX)
}

fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// Code fence that cannot be closed by backticks inside `code`.
fn fenced(language: Option<&str>, code: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in code.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{}\n{code}\n{fence}", language.unwrap_or_default())
}

/// `YYYY-MM-DD` (UTC) of a unix timestamp.
fn date(unix: i64) -> String {
    let days = unix.div_euclid(SECS_PER_DAY);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_formats_unix_seconds() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_794_355_200), "2026-11-11");
    }

    #[test]
    fn fence_outgrows_backticks_in_code() {
        assert_eq!(fenced(Some("md"), "a"), "```md\na\n```");
        assert_eq!(fenced(None, "````x"), "`````\n````x\n`````");
    }

    #[test]
    fn lines_ignore_trailing_newline() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a\n\nb"), vec!["a", "", "b"]);
        assert!(split_lines("").is_empty());
    }
}
