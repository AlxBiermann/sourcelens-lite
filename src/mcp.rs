//! Read-only MCP server: stateless Streamable HTTP with JSON responses only.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Map, Value};

use crate::error::ApiError;
use crate::http::keys::{self, KeyScope};
use crate::index::languages;
use crate::search::{self, symbols, Filter, SearchHit, SearchRequest};
use crate::state::AppState;

const SUPPORTED_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const LATEST_VERSION: &str = SUPPORTED_VERSIONS[0];

const INSTRUCTIONS: &str = "SourceLens searches indexed GitHub repositories. Start with list_repositories, then search_code. Results give repository, path and line range; read more with get_code. Use find_symbol for a definition by name and get_file_outline before reading a large file.";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const UNAUTHORIZED: i64 = -32001;
const FORBIDDEN: i64 = -32000;

const SEARCH_TEXT_BUDGET: usize = 24_000;
const SEARCH_PREVIEW_LINES: usize = 6;
const GET_CODE_DEFAULT_LINES: usize = 400;
const GET_CODE_MAX_LINES: usize = 800;

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
    if let Some(origin) = headers.get(header::ORIGIN) {
        let allowed = origin
            .to_str()
            .is_ok_and(|o| o.trim_end_matches('/') == state.cfg.public_origin());
        if !allowed {
            return rpc_error(
                StatusCode::FORBIDDEN,
                Value::Null,
                FORBIDDEN,
                "Forbidden: origin not allowed",
            );
        }
    }

    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, token) = v.trim().split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        });
    let scope = match bearer {
        Some(token) => keys::authenticate(&state, token).await,
        None => Err(ApiError::unauthorized("invalid_api_key", "")),
    };
    let scope = match scope {
        Ok(scope) => scope,
        Err(err) if err.status == StatusCode::UNAUTHORIZED => return unauthorized(),
        Err(err) => return rpc_error(err.status, Value::Null, INTERNAL_ERROR, &err.message),
    };

    if let Some(version) = headers.get("mcp-protocol-version") {
        let supported = version
            .to_str()
            .is_ok_and(|v| SUPPORTED_VERSIONS.contains(&v.trim()));
        if !supported {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                Value::Null,
                INVALID_REQUEST,
                "Unsupported MCP-Protocol-Version header",
            );
        }
    }

    let message: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                Value::Null,
                PARSE_ERROR,
                "Parse error: body is not valid JSON",
            )
        }
    };
    let Some(message) = message.as_object() else {
        return invalid_request("Invalid request: expected one JSON-RPC message object");
    };
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return invalid_request("Invalid request: jsonrpc must be \"2.0\"");
    }
    let Some(id) = message.get("id") else {
        // Notification.
        return StatusCode::ACCEPTED.into_response();
    };
    let Some(method) = message.get("method") else {
        if message.contains_key("result") || message.contains_key("error") {
            // A response to a server request; this server sends none, so there is nothing to do.
            return StatusCode::ACCEPTED.into_response();
        }
        return invalid_request("Invalid request: method is missing");
    };
    if !(id.is_string() || id.is_i64() || id.is_u64()) {
        return invalid_request("Invalid request: id must be a string or an integer");
    }
    let Some(method) = method.as_str() else {
        return invalid_request("Invalid request: method must be a string");
    };
    let params = message.get("params").unwrap_or(&Value::Null);

    let result = match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(&state, &scope, params).await,
        _ => Err((METHOD_NOT_FOUND, format!("Method not found: {method}"))),
    };
    match result {
        Ok(result) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response(),
        Err((code, message)) => rpc_error(StatusCode::OK, id.clone(), code, &message),
    }
}

fn rpc_error(status: StatusCode, id: Value, code: i64, message: &str) -> Response {
    let body = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    (status, Json(body)).into_response()
}

fn invalid_request(message: &str) -> Response {
    rpc_error(
        StatusCode::BAD_REQUEST,
        Value::Null,
        INVALID_REQUEST,
        message,
    )
}

fn unauthorized() -> Response {
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

fn tool_definitions() -> Value {
    fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": true },
        })
    }
    let repositories = json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "Repository full names (owner/repo) to search in. Default: all.",
    });
    let repository =
        json!({ "type": "string", "description": "Repository full name, e.g. owner/repo." });
    let path = json!({ "type": "string", "description": "File path inside the repository." });
    json!([
        tool(
            "list_repositories",
            "List the repositories this API key can search, with branch and index state.",
            json!({}),
            &[],
        ),
        tool(
            "search_code",
            "Search code by meaning, keywords or identifiers across indexed repositories. Returns the best matching code blocks with repository, path and line range.",
            json!({
                "query": { "type": "string", "description": "What to look for: words, identifiers or a description." },
                "repositories": repositories,
                "languages": { "type": "array", "items": { "type": "string" }, "description": "Language names, e.g. rust, typescript." },
                "pathPrefix": { "type": "string", "description": "Only files whose path starts with this prefix." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 20, "default": 8 },
            }),
            &["query"],
        ),
        tool(
            "get_code",
            "Read a file or a line range from an indexed repository.",
            json!({
                "repository": repository,
                "path": path,
                "startLine": { "type": "integer", "minimum": 1, "description": "First line, 1-based. Default 1." },
                "endLine": { "type": "integer", "minimum": 1, "description": "Last line, inclusive." },
            }),
            &["repository", "path"],
        ),
        tool(
            "get_file_outline",
            "List the symbols of one file with kinds, signatures and line ranges.",
            json!({ "repository": repository, "path": path }),
            &["repository", "path"],
        ),
        tool(
            "find_symbol",
            "Find where a function, class, type or other symbol is defined. Matches exact names first, then prefixes, then substrings.",
            json!({
                "name": { "type": "string", "description": "Symbol name, optionally qualified (Type.method, mod::fn)." },
                "repositories": repositories,
                "kind": { "type": "string", "description": "Symbol kind, e.g. function, method, class, struct." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 20 },
            }),
            &["name"],
        ),
    ])
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
    let output = match name {
        "list_repositories" => list_repositories(state, scope, Args::new(arguments, &[])?).await,
        "search_code" => {
            let allowed = ["query", "repositories", "languages", "pathPrefix", "limit"];
            search_code(state, scope, Args::new(arguments, &allowed)?).await
        }
        "get_code" => {
            let allowed = ["repository", "path", "startLine", "endLine"];
            get_code(state, scope, Args::new(arguments, &allowed)?).await
        }
        "get_file_outline" => {
            let allowed = ["repository", "path"];
            get_file_outline(state, scope, Args::new(arguments, &allowed)?).await
        }
        "find_symbol" => {
            let allowed = ["name", "repositories", "kind", "limit"];
            find_symbol(state, scope, Args::new(arguments, &allowed)?).await
        }
        _ => return Err((INVALID_PARAMS, format!("Unknown tool: {name}"))),
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
    fn new(map: &'a Map<String, Value>, allowed: &[&str]) -> Result<Self, (i64, String)> {
        match map.keys().find(|key| !allowed.contains(&key.as_str())) {
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

    fn limit(&self, name: &str, default: i64, max: i64) -> Result<usize, ToolError> {
        let limit = self.integer(name)?.unwrap_or(default).clamp(1, max);
        Ok(usize::try_from(limit).unwrap_or(1))
    }
}

struct ScopeRepo {
    id: i64,
    full_name: String,
    branch: String,
    indexed_commit: Option<String>,
    indexed_at: Option<i64>,
    file_count: i64,
}

/// Connected repositories of the key's scope, ordered by name.
async fn scope_repositories(
    state: &AppState,
    scope: &KeyScope,
) -> Result<Vec<ScopeRepo>, ToolError> {
    let all = state
        .db
        .read(|conn| {
            conn.prepare(
                "SELECT id, full_name, COALESCE(branch, default_branch), indexed_commit, indexed_at, file_count
                 FROM repositories WHERE state = 'connected' ORDER BY full_name",
            )?
            .query_map([], |r| {
                Ok(ScopeRepo {
                    id: r.get(0)?,
                    full_name: r.get(1)?,
                    branch: r.get(2)?,
                    indexed_commit: r.get(3)?,
                    indexed_at: r.get(4)?,
                    file_count: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    Ok(all
        .into_iter()
        .filter(|repo| scope.repo_ids.contains(&repo.id))
        .collect())
}

fn resolve<'r>(repos: &'r [ScopeRepo], name: &str) -> Result<&'r ScopeRepo, ToolError> {
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
fn narrow(repos: &[ScopeRepo], names: &[&str]) -> Result<Vec<i64>, ToolError> {
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

async fn list_repositories(
    state: &AppState,
    scope: &KeyScope,
    _args: Args<'_>,
) -> Result<ToolOutput, ToolError> {
    let repos = scope_repositories(state, scope).await?;
    let mut lines = Vec::with_capacity(repos.len());
    let mut structured = Vec::with_capacity(repos.len());
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
            repo.full_name, repo.branch
        ));
        structured.push(json!({
            "name": repo.full_name,
            "branch": repo.branch,
            "indexedCommit": repo.indexed_commit,
            "indexedAt": repo.indexed_at,
            "fileCount": repo.file_count,
        }));
    }
    let text = if lines.is_empty() {
        "No repositories are available to this API key. Connect repositories in SourceLens or widen the key's access.".to_string()
    } else {
        lines.join("\n")
    };
    Ok(ToolOutput {
        text,
        structured: json!({ "repositories": structured }),
    })
}

async fn search_code(
    state: &AppState,
    scope: &KeyScope,
    args: Args<'_>,
) -> Result<ToolOutput, ToolError> {
    let query = args.string("query")?;
    let repository_names = args.strings("repositories")?;
    let languages = args
        .strings("languages")?
        .into_iter()
        .map(|name| {
            languages::language_by_name(name)
                .map_or_else(|| name.trim().to_ascii_lowercase(), |l| l.name.to_string())
        })
        .collect();
    let path_prefix = args
        .opt_string("pathPrefix")?
        .map(|p| p.trim().trim_start_matches('/').to_string())
        .filter(|p| !p.is_empty());
    let limit = args.limit("limit", 8, 20)?;

    let repos = scope_repositories(state, scope).await?;
    let repo_ids = narrow(&repos, &repository_names)?;
    let response = search::search(
        state,
        SearchRequest {
            query: query.to_string(),
            filter: Filter {
                repo_ids,
                languages,
                path_prefix,
            },
            limit,
        },
    )
    .await?;

    let count = response.hits.len();
    let noun = if count == 1 { "result" } else { "results" };
    let mut text = format!(
        "{count} {noun} for \"{}\" ({})",
        query.trim(),
        response.mode
    );
    let mut used = text.chars().count();
    let mut truncating = false;
    for (n, hit) in response.hits.iter().enumerate() {
        let label = format!("{} {}", hit.kind, hit.symbol_path);
        let heading = format!(
            "\n\n{}. {} {}:{}-{}  {}\n",
            n + 1,
            hit.repository,
            hit.path,
            hit.start_line,
            hit.end_line,
            label.trim_end()
        );
        let full = fenced(hit.language.as_deref(), &hit.content);
        let block_len = heading.chars().count() + full.chars().count();
        if !truncating && used + block_len > SEARCH_TEXT_BUDGET {
            truncating = true;
        }
        text.push_str(&heading);
        if truncating {
            text.push_str(&preview(hit));
        } else {
            text.push_str(&full);
        }
        used += block_len;
    }
    let results: Vec<&SearchHit> = response.hits.iter().collect();
    Ok(ToolOutput {
        text,
        structured: json!({ "mode": response.mode, "results": results }),
    })
}

fn preview(hit: &SearchHit) -> String {
    let lines: Vec<&str> = hit.content.split('\n').collect();
    if lines.len() <= SEARCH_PREVIEW_LINES {
        return fenced(hit.language.as_deref(), &hit.content);
    }
    let head = lines[..SEARCH_PREVIEW_LINES].join("\n");
    format!(
        "{}\n… {} more lines, read with get_code",
        fenced(hit.language.as_deref(), &head),
        lines.len() - SEARCH_PREVIEW_LINES
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
    repo: &ScopeRepo,
    path: &str,
    with_content: bool,
) -> Result<FileRow, ToolError> {
    let repo_id = repo.id;
    let lookup_path = normalize_path(path).to_string();
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
    let path = normalize_path(path);
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

async fn get_code(
    state: &AppState,
    scope: &KeyScope,
    args: Args<'_>,
) -> Result<ToolOutput, ToolError> {
    let repository = args.string("repository")?;
    let path = args.string("path")?;
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
    let path = normalize_path(path);
    let content = String::from_utf8_lossy(file.content.as_deref().unwrap_or_default()).into_owned();
    let lines = split_lines(&content);
    let line_count = lines.len();
    let commit = repo.indexed_commit.as_deref();
    let at_commit = commit
        .map(|c| format!(" @ {}", short_commit(c)))
        .unwrap_or_default();

    if line_count == 0 {
        return Ok(ToolOutput {
            text: format!("{} {path} is empty{at_commit}.", repo.full_name),
            structured: json!({
                "repository": repo.full_name, "path": path, "startLine": 0, "endLine": 0,
                "lineCount": 0, "commit": commit, "content": "",
            }),
        });
    }

    let start = to_line(start_arg.unwrap_or(1));
    if start > line_count {
        return Err(ToolError::Failed(format!(
            "\"{path}\" has {line_count} lines; startLine {start} is past the end."
        )));
    }
    // Without endLine the caller wants the rest of the file, served in pages.
    let wanted_end = end_arg.map_or(line_count, to_line).min(line_count);
    let page = if end_arg.is_some() {
        GET_CODE_MAX_LINES
    } else {
        GET_CODE_DEFAULT_LINES
    };
    let end = wanted_end.min(start + page - 1);
    let code = lines[start - 1..end].join("\n");

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
    Ok(ToolOutput {
        text,
        structured: json!({
            "repository": repo.full_name, "path": path, "startLine": start, "endLine": end,
            "lineCount": line_count, "commit": commit, "content": code,
        }),
    })
}

struct OutlineSymbol {
    kind: String,
    name: String,
    qualified_name: String,
    signature: Option<String>,
    start_line: u32,
    end_line: u32,
    depth: u32,
}

async fn get_file_outline(
    state: &AppState,
    scope: &KeyScope,
    args: Args<'_>,
) -> Result<ToolOutput, ToolError> {
    let repository = args.string("repository")?;
    let path = args.string("path")?;
    let repos = scope_repositories(state, scope).await?;
    let repo = resolve(&repos, repository)?;
    let file = load_file(state, repo, path, false).await?;
    let path = normalize_path(path);
    let file_id = file.id;
    let symbols = state
        .db
        .read(move |conn| {
            conn.prepare(
                "SELECT kind, name, qualified_name, signature, start_line, end_line, depth
                 FROM symbols WHERE file_id = ?1 ORDER BY start_line, depth, id",
            )?
            .query_map([file_id], |r| {
                Ok(OutlineSymbol {
                    kind: r.get(0)?,
                    name: r.get(1)?,
                    qualified_name: r.get(2)?,
                    signature: r.get(3)?,
                    start_line: r.get(4)?,
                    end_line: r.get(5)?,
                    depth: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;

    let text = if symbols.is_empty() {
        format!("No symbols in {path}. Read it with get_code.")
    } else {
        symbols
            .iter()
            .map(|s| {
                let indent = "  ".repeat(usize::try_from(s.depth).unwrap_or(0));
                let signature = s
                    .signature
                    .as_deref()
                    .map(|sig| format!(" — {sig}"))
                    .unwrap_or_default();
                format!(
                    "{indent}L{}-{} {} {}{signature}",
                    s.start_line, s.end_line, s.kind, s.name
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let structured: Vec<Value> = symbols
        .iter()
        .map(|s| {
            json!({
                "kind": s.kind, "name": s.name, "qualifiedName": s.qualified_name, "signature": s.signature,
                "startLine": s.start_line, "endLine": s.end_line, "depth": s.depth,
            })
        })
        .collect();
    Ok(ToolOutput {
        text,
        structured: json!({ "repository": repo.full_name, "path": path, "symbols": structured }),
    })
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
    let limit = args.limit("limit", 20, 50)?;
    let repos = scope_repositories(state, scope).await?;
    let repo_ids = narrow(&repos, &repository_names)?;
    if name.is_empty() {
        return Err(ToolError::Failed(
            "Enter a symbol name, for example find_symbol with name \"parseConfig\".".into(),
        ));
    }

    let lookup_name = name.clone();
    let found = if repo_ids.is_empty() {
        Vec::new()
    } else {
        state
            .db
            .read(move |conn| {
                let rows = symbols::lookup(conn, &repo_ids, &lookup_name, kind.as_deref(), limit)?;
                let mut path_of = conn.prepare("SELECT path FROM files WHERE id = ?1")?;
                let mut paths: HashMap<i64, String> = HashMap::new();
                let mut found = Vec::with_capacity(rows.len());
                for row in rows {
                    let path = match paths.get(&row.file_id) {
                        Some(path) => path.clone(),
                        None => {
                            let path: String = path_of.query_row([row.file_id], |r| r.get(0))?;
                            paths.insert(row.file_id, path.clone());
                            path
                        }
                    };
                    found.push((row, path));
                }
                Ok(found)
            })
            .await?
    };

    let names: HashMap<i64, &str> = repos
        .iter()
        .map(|repo| (repo.id, repo.full_name.as_str()))
        .collect();
    let mut text = Vec::with_capacity(found.len());
    let mut structured = Vec::with_capacity(found.len());
    for (n, (row, path)) in found.iter().enumerate() {
        let repository = names.get(&row.repo_id).copied().unwrap_or_default();
        text.push(format!(
            "{}. {} {} — {repository} {path}:{}-{}",
            n + 1,
            row.kind,
            row.qualified_name,
            row.start_line,
            row.end_line
        ));
        if let Some(signature) = &row.signature {
            text.push(format!("   {signature}"));
        }
        structured.push(json!({
            "kind": row.kind, "name": row.name, "qualifiedName": row.qualified_name, "signature": row.signature,
            "startLine": row.start_line, "endLine": row.end_line, "depth": row.depth,
            "repository": repository, "path": path,
        }));
    }
    let text = if text.is_empty() {
        format!("No symbol named \"{name}\" found. Try search_code with a description or related words.")
    } else {
        text.join("\n")
    };
    Ok(ToolOutput {
        text,
        structured: json!({ "symbols": structured }),
    })
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
    let days = unix.div_euclid(86_400);
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
