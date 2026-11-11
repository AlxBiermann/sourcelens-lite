//! Tree-sitter declaration walk: turns a parse tree into declaration and root-statement chunks.
//!
//! Pure: never executes repository code, retains parse trees or source, or performs I/O.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{Language, Node, ParseOptions, Parser, Tree};

const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_AST_DEPTH: usize = 384;
const MAX_NODES: usize = 250_000;
const MAX_PARSE_TIME: Duration = Duration::from_secs(2);
const MAX_ESTIMATED_MEMORY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AdapterChunk {
    pub(super) kind: &'static str,
    pub(super) name: Option<String>,
    /// Segments placed before the name in the symbol path: the Go receiver type, the C++ scope of
    /// an out-of-line definition (`Widget::draw`).
    pub(super) owner: Vec<String>,
    /// Package or namespace (dot-separated) that prefixes qualified names.
    pub(super) namespace: Option<String>,
    pub(super) start_line: u32,
    pub(super) end_line: u32,
    /// Start lines of the direct body members of this declaration (1-based), used as split points.
    pub(super) child_boundaries: Vec<u32>,
    /// Declaration text up to its body, whitespace collapsed, at most 300 chars.
    pub(super) signature: Option<String>,
    /// Declares a symbol (constant, field) without forming a chunk of its own.
    pub(super) symbol_only: bool,
}

const MAX_SIGNATURE_CHARS: usize = 300;

/// Parses `source` with the grammar of `language` and returns its declaration chunks.
/// `None` when there is no grammar, a budget is exceeded, the tree is mostly errors, or
/// no chunk was produced; callers then fall back to line windows.
pub(super) fn parse(language: &str, path: &str, source: &str) -> Option<Vec<AdapterChunk>> {
    let chunks = parse_as(language, path, source);
    // `.h` is labelled C, but many such headers are C++; the C grammar rejects those.
    if chunks.is_none() && language == "c" && path.to_ascii_lowercase().ends_with(".h") {
        return parse_as("cpp", path, source);
    }
    chunks
}

fn parse_as(language: &str, path: &str, source: &str) -> Option<Vec<AdapterChunk>> {
    let (key, grammar) = grammar(language, path)?;
    parse_tree_sitter(language, key, source, &grammar)
}

/// Parser-cache key and tree-sitter grammar for a language label.
fn grammar(name: &str, path: &str) -> Option<(&'static str, Language)> {
    match name {
        "go" => Some(("go", tree_sitter_go::LANGUAGE.into())),
        "c" => Some(("c", tree_sitter_c::LANGUAGE.into())),
        "cpp" => Some(("cpp", tree_sitter_cpp::LANGUAGE.into())),
        "csharp" => Some(("csharp", tree_sitter_c_sharp::LANGUAGE.into())),
        "ruby" => Some(("ruby", tree_sitter_ruby::LANGUAGE.into())),
        "php" => Some(("php", tree_sitter_php::LANGUAGE_PHP.into())),
        "scala" => Some(("scala", tree_sitter_scala::LANGUAGE.into())),
        "rust" => Some(("rust", tree_sitter_rust::LANGUAGE.into())),
        "python" => Some(("python", tree_sitter_python::LANGUAGE.into())),
        "javascript" => Some(("javascript", tree_sitter_javascript::LANGUAGE.into())),
        "java" => Some(("java", tree_sitter_java::LANGUAGE.into())),
        "kotlin" => Some(("kotlin", tree_sitter_kotlin_ng::LANGUAGE.into())),
        "typescript" if path.ends_with(".tsx") => {
            Some(("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()))
        }
        "typescript" => Some((
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        )),
        _ => None,
    }
}

/// Recovered error/missing bytes above this share of the source (per mille) discard the tree.
const MAX_ERROR_BYTE_PERMILLE: usize = 100;

/// Nodes below these kinds are never chunk candidates: nested functions, closures and local
/// declarations stay inside their enclosing chunk.
const FUNCTION_LIKE_KINDS: &[&str] = &[
    "function_item",
    "function_definition",
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
    "method_declaration",
    "constructor_declaration",
    "compact_constructor_declaration",
    "secondary_constructor",
    "anonymous_initializer",
    "local_function_statement",
    "arrow_function",
    "function_expression",
    "function",
    "func_literal",
    "lambda",
    "lambda_literal",
    "lambda_expression",
    "closure_expression",
    "anonymous_function",
    "singleton_method",
    "method",
    "function_signature_item",
];

/// Attribute-like siblings that are absorbed into the following declaration.
const ATTRIBUTE_KINDS: &[&str] = &[
    "attribute_item",
    "inner_attribute_item",
    "decorator",
    "attribute_list",
    "annotation",
    "marker_annotation",
    "file_annotation",
];

/// Body containers whose direct children are the split points of a declaration.
const BODY_KINDS: &[&str] = &[
    "block",
    "statement_block",
    "compound_statement",
    "declaration_list",
    "field_declaration_list",
    "class_body",
    "enum_body",
    "interface_body",
    "enum_class_body",
    "template_body",
    "function_body",
    "body_statement",
    "object_body",
];

const FUNCTION_VALUE_KINDS: &[&str] = &["arrow_function", "function_expression", "function"];

/// C/C++ type specifiers that declare a type when they have a body.
const SPECIFIER_KINDS: &[&str] = &[
    "struct_specifier",
    "enum_specifier",
    "union_specifier",
    "class_specifier",
];

thread_local! {
    /// One parser per grammar and thread; reset before every parse so no document state survives.
    static PARSERS: RefCell<HashMap<&'static str, Parser>> = RefCell::new(HashMap::new());
}

/// `None` when the grammar cannot be loaded or parsing hit the deadline.
fn parse_with_cached_parser(
    key: &'static str,
    grammar: &Language,
    source: &str,
    deadline: Instant,
) -> Option<Tree> {
    PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        let parser = match parsers.entry(key) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let mut parser = Parser::new();
                if parser.set_language(grammar).is_err() {
                    return None;
                }
                entry.insert(parser)
            }
        };
        parser.reset();
        // tree-sitter 0.27 stops parsing when the progress callback breaks.
        let mut progress = |_state: &tree_sitter::ParseState| {
            if Instant::now() >= deadline {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = ParseOptions::new().progress_callback(&mut progress);
        let bytes = source.as_bytes();
        let mut reader = |offset: usize, _point| bytes.get(offset..).unwrap_or_default();
        parser.parse_with_options(&mut reader, None, Some(options))
    })
}

fn parse_tree_sitter(
    language: &str,
    key: &'static str,
    source: &str,
    grammar: &Language,
) -> Option<Vec<AdapterChunk>> {
    let started = Instant::now();
    let source_bytes = source.len();
    if source_bytes > MAX_SOURCE_BYTES {
        return None;
    }
    let deadline = started + MAX_PARSE_TIME;
    let tree = parse_with_cached_parser(key, grammar, source, deadline)?;

    let root = tree.root_node();
    let mut walk = Walk {
        language,
        source: source.as_bytes(),
        chunks: Vec::new(),
        absorbed: HashSet::new(),
        handled: HashSet::new(),
        root_candidates: Vec::new(),
        file_namespace: None,
    };
    let mut node_count = 0usize;
    let mut error_bytes = 0usize;
    // (node, depth, inside_function, inside_root_statement)
    let mut stack = vec![(root, 0usize, false, false)];
    while let Some((node, depth, inside_function, inside_statement)) = stack.pop() {
        node_count += 1;
        if node_count > MAX_NODES
            || depth > MAX_AST_DEPTH
            || estimate_memory(source_bytes, node_count) > MAX_ESTIMATED_MEMORY_BYTES
            || started.elapsed() > MAX_PARSE_TIME
        {
            return None;
        }

        if node.is_error() {
            error_bytes += node.byte_range().len();
            if !inside_function && !inside_statement && walk.effective_parent_is_root(node) {
                walk.root_candidates.push(node);
            }
            continue;
        }
        let mut children_inside_statement = inside_statement;
        if node.parent().is_some() && !inside_function && !inside_statement {
            children_inside_statement = walk.visit(node);
        }
        let children_inside_function =
            inside_function || FUNCTION_LIKE_KINDS.contains(&node.kind());
        let check_missing = node.has_error();
        let mut cursor = node.walk();
        let mut children = Vec::new();
        for child in node.children(&mut cursor) {
            if check_missing && child.is_missing() {
                error_bytes += 1;
            } else if child.is_named() {
                children.push(child);
            }
        }
        stack.extend(children.into_iter().rev().map(|child| {
            (
                child,
                depth + 1,
                children_inside_function,
                children_inside_statement,
            )
        }));
    }

    if root.kind() == "ERROR"
        || error_bytes.saturating_mul(1000) > source_bytes.saturating_mul(MAX_ERROR_BYTE_PERMILLE)
    {
        return None;
    }

    let mut chunks = walk.finish();
    dedupe(&mut chunks);
    (!chunks.is_empty()).then_some(chunks)
}

/// Chunk-candidate state of one tree walk.
struct Walk<'t, 's> {
    language: &'s str,
    source: &'s [u8],
    chunks: Vec<AdapterChunk>,
    /// Comments/attributes absorbed into a following declaration.
    absorbed: HashSet<usize>,
    /// Declarations already emitted through their transparent wrapper.
    handled: HashSet<usize>,
    /// Unclassified nodes whose effective parent is the root; emitted as `statement` unless absorbed.
    root_candidates: Vec<Node<'t>>,
    /// Name of the last file-level `package`/namespace declaration seen in document order.
    file_namespace: Option<String>,
}

impl<'t> Walk<'t, '_> {
    /// Classifies one node outside any function; returns true when its children must not yield
    /// chunks (the node became a root-level `statement`).
    fn visit(&mut self, node: Node<'t>) -> bool {
        if self.handled.contains(&node.id()) {
            return false;
        }
        let kind = node.kind();
        if let Some(inner) = transparent_inner(self.language, node) {
            if let Some(chunk_kind) = classify_node(self.language, inner, self.source) {
                self.handled.insert(inner.id());
                // Decorators and other wrapper parts lie inside the wrapper's chunk range.
                for part in named_children_of(node) {
                    if part.id() != inner.id() {
                        self.absorbed.insert(part.id());
                    }
                }
                self.push_declaration(chunk_kind, node, inner);
                return false;
            }
            self.push_values(node, inner);
            return self.root_statement(node);
        }
        if is_transparent(self.language, node) {
            if let Some(body) = block_namespace_body(self.language, node) {
                self.push_namespace_header(node, body);
            }
            return false;
        }
        if is_comment(kind) || ATTRIBUTE_KINDS.contains(&kind) {
            return self.root_statement(node);
        }
        if let Some(chunk_kind) = classify_node(self.language, node, self.source) {
            if chunk_kind == "package" {
                self.file_namespace = namespace_name(node, self.source);
            }
            if let Some(specifier) = typedef_specifier(self.language, node) {
                self.handled.insert(specifier.id());
            }
            self.push_declaration(chunk_kind, node, node);
            return false;
        }
        self.push_values(node, node);
        self.root_statement(node)
    }

    fn root_statement(&mut self, node: Node<'t>) -> bool {
        if self.effective_parent_is_root(node) {
            self.root_candidates.push(node);
            true
        } else {
            false
        }
    }

    fn effective_parent_is_root(&self, node: Node<'t>) -> bool {
        effective_parent(self.language, node).is_some_and(|parent| parent.parent().is_none())
    }

    /// Enclosing block namespaces (C#, PHP), else the file-level package or namespace.
    fn namespace_of(&self, node: Node<'t>) -> Option<String> {
        let mut segments = Vec::new();
        let mut ancestor = node.parent();
        while let Some(current) = ancestor {
            if block_namespace_body(self.language, current).is_some() {
                segments.extend(namespace_name(current, self.source));
            }
            ancestor = current.parent();
        }
        if segments.is_empty() {
            return self.file_namespace.clone();
        }
        segments.reverse();
        Some(segments.join("."))
    }

    /// `range_node` provides the line range (wrapper incl. `export`/decorators), `declaration`
    /// the name, body and signature.
    fn push_declaration(
        &mut self,
        kind: &'static str,
        range_node: Node<'t>,
        declaration: Node<'t>,
    ) {
        let mut start_row = range_node.start_position().row;
        let end_row = last_row(range_node);
        let mut previous = range_node.prev_named_sibling();
        while let Some(candidate) = previous {
            let candidate_kind = candidate.kind();
            if !(is_comment(candidate_kind) || ATTRIBUTE_KINDS.contains(&candidate_kind))
                || last_row(candidate) + 1 < start_row
                || !starts_own_line(candidate, self.source)
            {
                break;
            }
            start_row = candidate.start_position().row;
            self.absorbed.insert(candidate.id());
            previous = candidate.prev_named_sibling();
        }
        let start_line = start_row as u32 + 1;
        let end_line = end_row as u32 + 1;
        let (child_boundaries, signature) = if matches!(kind, "imports" | "package") {
            (Vec::new(), None)
        } else {
            let body = body_node(self.language, declaration);
            (
                body.map(|body| member_boundaries(body, start_line, end_line))
                    .unwrap_or_default(),
                declaration_signature(declaration, body, self.source),
            )
        };
        let (name, owner) = declaration_name(self.language, declaration, self.source);
        self.chunks.push(AdapterChunk {
            kind,
            name,
            owner,
            namespace: self.namespace_of(declaration),
            start_line,
            end_line,
            child_boundaries,
            signature,
            symbol_only: false,
        });
    }

    /// One-line `package` chunk for the header of a block namespace whose members are walked as
    /// root-level declarations.
    fn push_namespace_header(&mut self, node: Node<'t>, body: Node<'t>) {
        // The name and other header parts must not become root statements of their own.
        for part in named_children_of(node) {
            if part.id() != body.id() {
                self.absorbed.insert(part.id());
            }
        }
        let start_row = node.start_position().row;
        let end_row = body.start_position().row.saturating_sub(1).max(start_row);
        let shares_member_line = named_children_of(body)
            .first()
            .is_some_and(|member| member.start_position().row <= end_row);
        if shares_member_line {
            return;
        }
        self.chunks.push(AdapterChunk {
            kind: "package",
            name: namespace_name(node, self.source),
            owner: Vec::new(),
            namespace: self.namespace_of(node),
            start_line: start_row as u32 + 1,
            end_line: end_row as u32 + 1,
            child_boundaries: Vec::new(),
            signature: None,
            symbol_only: false,
        });
    }

    /// Symbol-only entries for the constants, variables and fields `declaration` declares.
    fn push_values(&mut self, range_node: Node<'t>, declaration: Node<'t>) {
        let values = value_declarations(self.language, declaration, self.source);
        if values.is_empty() {
            return;
        }
        let namespace = self.namespace_of(declaration);
        for value in values {
            let statement = if value.statement.id() == declaration.id() {
                range_node
            } else {
                value.statement
            };
            self.chunks.push(AdapterChunk {
                kind: value.kind,
                name: Some(value.name),
                owner: Vec::new(),
                namespace: namespace.clone(),
                start_line: statement.start_position().row as u32 + 1,
                end_line: last_row(statement) as u32 + 1,
                child_boundaries: Vec::new(),
                signature: declaration_signature(value.statement, None, self.source),
                symbol_only: true,
            });
        }
    }

    fn finish(self) -> Vec<AdapterChunk> {
        let Walk {
            source,
            mut chunks,
            absorbed,
            root_candidates,
            ..
        } = self;
        for node in root_candidates {
            if absorbed.contains(&node.id()) {
                continue;
            }
            // A trailing comment shares its line with the previous root node's chunk.
            if is_comment(node.kind()) && !starts_own_line(node, source) {
                continue;
            }
            let start_line = node.start_position().row as u32 + 1;
            let end_line = last_row(node) as u32 + 1;
            chunks.push(AdapterChunk {
                kind: "statement",
                name: None,
                owner: Vec::new(),
                namespace: None,
                start_line,
                end_line,
                child_boundaries: member_boundaries(
                    statement_container(node),
                    start_line,
                    end_line,
                ),
                signature: None,
                symbol_only: false,
            });
        }
        chunks
    }
}

/// Source from the declaration start to the start of its body (the outermost body wrapper, so
/// the opening delimiter is excluded), or the declaration's first line when it has no body.
fn declaration_signature(
    declaration: Node<'_>,
    body: Option<Node<'_>>,
    source: &[u8],
) -> Option<String> {
    let start = declaration.start_byte().min(source.len());
    let end = match body {
        Some(mut body) => {
            while let Some(parent) = body.parent() {
                if parent.id() == declaration.id()
                    || !(BODY_KINDS.contains(&parent.kind()) || parent.kind() == "statement_list")
                {
                    break;
                }
                body = parent;
            }
            body.start_byte()
        }
        None => source[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(source.len(), |offset| start + offset)
            .min(declaration.end_byte()),
    }
    .clamp(start, source.len());
    let text = String::from_utf8_lossy(&source[start..end]);
    let mut signature = String::new();
    for word in text.split_whitespace() {
        if !signature.is_empty() {
            signature.push(' ');
        }
        signature.push_str(word);
    }
    let signature: String = signature.chars().take(MAX_SIGNATURE_CHARS).collect();
    let signature = signature.trim_end();
    (!signature.is_empty()).then(|| signature.to_owned())
}

/// Declaration wrapped by a transparent node (`export`, `declare`, Python decorators, C++
/// `template <…>`).
fn transparent_inner<'t>(language: &str, node: Node<'t>) -> Option<Node<'t>> {
    match (language, node.kind()) {
        ("typescript" | "javascript", "export_statement") => {
            node.child_by_field_name("declaration")
        }
        ("typescript", "ambient_declaration") => node.named_child(0).filter(|child| {
            matches!(
                child.kind(),
                "abstract_class_declaration"
                    | "class_declaration"
                    | "enum_declaration"
                    | "function_declaration"
                    | "function_signature"
                    | "generator_function_declaration"
                    | "interface_declaration"
                    | "internal_module"
                    | "lexical_declaration"
                    | "module"
                    | "type_alias_declaration"
                    | "variable_declaration"
            )
        }),
        ("python", "decorated_definition") => node.child_by_field_name("definition"),
        ("cpp", "template_declaration") => {
            named_children_of(node).into_iter().rev().find(|child| {
                child.kind() == "function_definition"
                    || child.kind() == "alias_declaration"
                    || SPECIFIER_KINDS.contains(&child.kind())
            })
        }
        _ => None,
    }
}

/// Body of a C# or PHP namespace written as a block (`namespace X { … }`).
fn block_namespace_body<'t>(language: &str, node: Node<'t>) -> Option<Node<'t>> {
    match (language, node.kind()) {
        ("csharp", "namespace_declaration") | ("php", "namespace_definition") => {
            node.child_by_field_name("body")
        }
        _ => None,
    }
}

/// Wrappers whose children count as children of the wrapper's own parent.
fn is_transparent(language: &str, node: Node<'_>) -> bool {
    if transparent_inner(language, node).is_some() {
        return true;
    }
    match language {
        "go" => node.kind() == "type_declaration" && go_specs(node).len() > 1,
        "c" | "cpp" => {
            matches!(
                node.kind(),
                "preproc_ifdef"
                    | "preproc_if"
                    | "preproc_else"
                    | "preproc_elif"
                    | "linkage_specification"
                    | "declaration_list"
            ) && node
                .parent()
                .is_some_and(|parent| parent.parent().is_none() || is_transparent(language, parent))
        }
        "csharp" | "php" => {
            block_namespace_body(language, node).is_some()
                || node.parent().is_some_and(|parent| {
                    block_namespace_body(language, parent)
                        .is_some_and(|body| body.id() == node.id())
                })
        }
        _ => false,
    }
}
/// Nearest ancestor that is not a transparent wrapper.
fn effective_parent<'t>(language: &str, node: Node<'t>) -> Option<Node<'t>> {
    let mut parent = node.parent();
    while let Some(candidate) = parent {
        if candidate.parent().is_none() || !is_transparent(language, candidate) {
            return Some(candidate);
        }
        parent = candidate.parent();
    }
    None
}

fn effective_parent_kind<'t>(language: &str, node: Node<'t>) -> Option<&'t str> {
    effective_parent(language, node).map(|parent| parent.kind())
}

fn go_specs(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| matches!(child.kind(), "type_spec" | "type_alias"))
        .collect()
}

fn go_spec_kind(spec: Node<'_>) -> &'static str {
    if spec.kind() == "type_alias" {
        return "type";
    }
    match spec.child_by_field_name("type").map(|ty| ty.kind()) {
        Some("struct_type") => "struct",
        Some("interface_type") => "interface",
        _ => "type",
    }
}

/// Last row covered by the node's text; a node ending with its newline ends on the previous row.
fn last_row(node: Node<'_>) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    }
}

fn starts_own_line(node: Node<'_>, source: &[u8]) -> bool {
    let start = node.start_byte().min(source.len());
    let line_start = source[..start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    source[line_start..start]
        .iter()
        .all(|byte| byte.is_ascii_whitespace())
}

fn named_children_of(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

/// Body container of a declaration (`body` field, else the last body-like child), unwrapped
/// through single-child bodies such as Kotlin `function_body > block` or Go `block > statement_list`.
fn body_node<'t>(language: &str, declaration: Node<'t>) -> Option<Node<'t>> {
    if language == "go"
        && matches!(
            declaration.kind(),
            "type_declaration" | "type_spec" | "type_alias"
        )
    {
        let spec = if declaration.kind() == "type_declaration" {
            go_specs(declaration).into_iter().next()?
        } else {
            declaration
        };
        let ty = spec.child_by_field_name("type")?;
        return match ty.kind() {
            "struct_type" => named_children_of(ty)
                .into_iter()
                .find(|child| child.kind() == "field_declaration_list"),
            "interface_type" => Some(ty),
            _ => None,
        };
    }
    if let Some(specifier) = typedef_specifier(language, declaration) {
        return specifier.child_by_field_name("body");
    }
    if let Some(value) = function_value(language, declaration) {
        // Expression-bodied arrow functions have no statements to split at.
        return value
            .child_by_field_name("body")
            .filter(|body| body.kind() == "statement_block");
    }
    let mut body = declaration.child_by_field_name("body").or_else(|| {
        named_children_of(declaration)
            .into_iter()
            .rev()
            .find(|child| BODY_KINDS.contains(&child.kind()))
    })?;
    loop {
        let children = named_children_of(body);
        match children.as_slice() {
            [only] if BODY_KINDS.contains(&only.kind()) || only.kind() == "statement_list" => {
                body = *only;
            }
            _ => return Some(body),
        }
    }
}

/// Descends through single multi-line children (`export` > `const` > declarator > object) so a
/// root statement splits at its innermost list of members.
fn statement_container(node: Node<'_>) -> Node<'_> {
    let mut container = node;
    loop {
        let children = named_children_of(container);
        let multi_line = children
            .iter()
            .filter(|child| last_row(**child) > child.start_position().row)
            .collect::<Vec<_>>();
        let [inner] = multi_line.as_slice() else {
            return container;
        };
        let header_row = container.start_position().row;
        let inner_start = inner.start_position().row;
        let only_header_siblings = children.iter().all(|child| {
            child.id() == inner.id()
                || (last_row(*child) == child.start_position().row
                    && (child.start_position().row == header_row
                        || child.start_position().row == inner_start))
        });
        if !only_header_siblings {
            return container;
        }
        container = **inner;
    }
}

/// Start lines of the direct members of `container`; leading comments/attributes that are adjacent
/// to the next member start that member's segment, members sharing a line with the previous member
/// are not split points.
fn member_boundaries(container: Node<'_>, start_line: u32, end_line: u32) -> Vec<u32> {
    let children = named_children_of(container);
    let mut boundaries = Vec::new();
    let mut pending: Option<usize> = None;
    let mut previous_end: Option<usize> = None;
    for (index, child) in children.iter().enumerate() {
        let row = child.start_position().row;
        let end = last_row(*child);
        if previous_end.is_some_and(|previous| row <= previous) {
            previous_end = previous_end.max(Some(end));
            continue;
        }
        previous_end = Some(end);
        let kind = child.kind();
        if is_comment(kind) || ATTRIBUTE_KINDS.contains(&kind) {
            if let Some(next) = children.get(index + 1) {
                if next.start_position().row <= end + 1 {
                    pending.get_or_insert(row);
                    continue;
                }
            }
        }
        boundaries.push(pending.take().unwrap_or(row) as u32 + 1);
    }
    if let Some(row) = pending {
        boundaries.push(row as u32 + 1);
    }
    boundaries.retain(|line| *line > start_line && *line < end_line);
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries
}

fn classify_node(language: &str, node: Node<'_>, source: &[u8]) -> Option<&'static str> {
    let kind = node.kind();
    if matches!(
        kind,
        "import_declaration"
            | "import_statement"
            | "import_from_statement"
            | "future_import_statement"
            | "using_directive"
            | "namespace_use_declaration"
            | "use_declaration"
            | "preproc_include"
    ) {
        return Some("imports");
    }
    if matches!(
        kind,
        "package_clause"
            | "package_declaration"
            | "package_header"
            | "namespace_declaration"
            | "file_scoped_namespace_declaration"
    ) {
        return Some("package");
    }
    match language {
        "rust" => match kind {
            "function_item" => {
                let in_impl_or_trait = effective_parent(language, node).is_some_and(|parent| {
                    parent.kind() == "declaration_list"
                        && parent
                            .parent()
                            .is_some_and(|owner| matches!(owner.kind(), "impl_item" | "trait_item"))
                });
                Some(if in_impl_or_trait {
                    "method"
                } else {
                    "function"
                })
            }
            "function_signature_item" => Some("method"),
            "struct_item" => Some("struct"),
            "enum_item" => Some("enum"),
            "union_item" => Some("union"),
            "trait_item" => Some("trait"),
            "type_item" => Some("type"),
            // `mod name;` only points at another file; that file holds the module.
            "mod_item" => node.child_by_field_name("body").map(|_| "module"),
            "macro_definition" => Some("macro"),
            "impl_item" => Some("impl"),
            _ => None,
        },
        "python" => match kind {
            "class_definition" => Some("class"),
            "function_definition" => {
                let in_class = effective_parent(language, node).is_some_and(|parent| {
                    parent.kind() == "block"
                        && parent
                            .parent()
                            .is_some_and(|owner| owner.kind() == "class_definition")
                });
                Some(if in_class { "method" } else { "function" })
            }
            _ => None,
        },
        "typescript" | "javascript" => match kind {
            "function_declaration" | "generator_function_declaration" => Some("function"),
            "class_declaration" | "abstract_class_declaration" => Some("class"),
            "interface_declaration" => Some("interface"),
            "type_alias_declaration" => Some("type"),
            "enum_declaration" => Some("enum"),
            "internal_module" | "module" => Some("module"),
            "method_definition" => {
                if effective_parent_kind(language, node) != Some("class_body") {
                    return None;
                }
                let name = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok());
                Some(if name == Some("constructor") {
                    "constructor"
                } else {
                    "method"
                })
            }
            "public_field_definition" | "field_definition" => {
                let function_valued = node
                    .child_by_field_name("value")
                    .is_some_and(|value| FUNCTION_VALUE_KINDS.contains(&value.kind()));
                (function_valued && effective_parent_kind(language, node) == Some("class_body"))
                    .then_some("method")
            }
            "lexical_declaration" | "variable_declaration" => {
                single_function_declarator(node).map(|_| "function")
            }
            _ => None,
        },
        "java" => match kind {
            "class_declaration" => Some("class"),
            "interface_declaration" | "annotation_type_declaration" => Some("interface"),
            "enum_declaration" => Some("enum"),
            "record_declaration" => Some("record"),
            "method_declaration" => Some("method"),
            "constructor_declaration" | "compact_constructor_declaration" => Some("constructor"),
            _ => None,
        },
        "kotlin" => match kind {
            "class_declaration" => Some(kotlin_class_kind(node, source)),
            "object_declaration" => Some("object"),
            "companion_object" => Some("companion_object"),
            "function_declaration" => Some(
                if matches!(
                    effective_parent_kind(language, node),
                    Some("class_body" | "enum_class_body")
                ) {
                    "method"
                } else {
                    "function"
                },
            ),
            "property_declaration" => Some("property"),
            "type_alias" => Some("type"),
            "secondary_constructor" => Some("constructor"),
            "import" => Some("imports"),
            _ => None,
        },
        "go" => match kind {
            "function_declaration" => Some("function"),
            "method_declaration" => Some("method"),
            "type_declaration" => match go_specs(node).as_slice() {
                [spec] => Some(go_spec_kind(*spec)),
                _ => None,
            },
            "type_spec" | "type_alias" => node
                .parent()
                .is_some_and(|parent| is_transparent(language, parent))
                .then(|| go_spec_kind(node)),
            _ => None,
        },
        "c" | "cpp" => match kind {
            "function_definition" => Some("function"),
            "struct_specifier" | "enum_specifier" | "union_specifier" | "class_specifier" => {
                specifier_kind(node)
            }
            // `typedef struct { … } Name;` is one declaration of the struct named `Name`.
            "type_definition" => Some(
                typedef_specifier(language, node)
                    .and_then(specifier_kind)
                    .unwrap_or("type"),
            ),
            "namespace_definition" if language == "cpp" => Some("module"),
            "alias_declaration" if language == "cpp" => Some("type"),
            _ => None,
        },
        "csharp" => match kind {
            "class_declaration" => Some("class"),
            "interface_declaration" => Some("interface"),
            "struct_declaration" => Some("struct"),
            "enum_declaration" => Some("enum"),
            "record_declaration" => Some("record"),
            "method_declaration" => Some("method"),
            "constructor_declaration" => Some("constructor"),
            "property_declaration" => Some("property"),
            "delegate_declaration" => Some("type"),
            _ => None,
        },
        "ruby" => match kind {
            "class" => Some("class"),
            "module" => Some("module"),
            "method" => Some("method"),
            "singleton_method" => Some("method"),
            _ => None,
        },
        "php" => match kind {
            "namespace_definition" => Some("package"),
            "class_declaration" => Some("class"),
            "interface_declaration" => Some("interface"),
            "trait_declaration" => Some("trait"),
            "enum_declaration" => Some("enum"),
            "function_definition" => Some("function"),
            "method_declaration" => Some("method"),
            _ => None,
        },
        "scala" => match kind {
            "class_definition" => Some("class"),
            "trait_definition" => Some("trait"),
            "object_definition" => Some("object"),
            "enum_definition" => Some("enum"),
            "function_definition" | "function_declaration" => Some("function"),
            "val_definition" | "var_definition" => Some("property"),
            _ => None,
        },
        _ => None,
    }
}

/// Type kind of a C/C++ struct/enum/union/class specifier that has a body (a definition, not a
/// reference such as `struct Foo *f`).
fn specifier_kind(node: Node<'_>) -> Option<&'static str> {
    node.child_by_field_name("body")?;
    match node.kind() {
        "struct_specifier" => Some("struct"),
        "enum_specifier" => Some("enum"),
        "union_specifier" => Some("union"),
        "class_specifier" => Some("class"),
        _ => None,
    }
}

/// The specifier with a body inside a C/C++ `typedef`.
fn typedef_specifier<'t>(language: &str, node: Node<'t>) -> Option<Node<'t>> {
    if !matches!(language, "c" | "cpp") || node.kind() != "type_definition" {
        return None;
    }
    node.child_by_field_name("type")
        .filter(|ty| specifier_kind(*ty).is_some())
}

/// The function value of a TS/JS declaration classified as a function or method.
fn function_value<'t>(language: &str, node: Node<'t>) -> Option<Node<'t>> {
    if !matches!(language, "typescript" | "javascript") {
        return None;
    }
    match node.kind() {
        "lexical_declaration" | "variable_declaration" => single_function_declarator(node)
            .and_then(|declarator| declarator.child_by_field_name("value")),
        "public_field_definition" | "field_definition" => node
            .child_by_field_name("value")
            .filter(|value| FUNCTION_VALUE_KINDS.contains(&value.kind())),
        _ => None,
    }
}

/// A constant, variable or field declared by a node that forms no declaration chunk.
struct ValueDeclaration<'t> {
    kind: &'static str,
    name: String,
    /// Node whose lines and first line become the symbol's range and signature.
    statement: Node<'t>,
}

/// Root-level constants and variables, and fields in class bodies (Rust, Go, TS/JS, Python, Java,
/// C#). Function-valued declarations are chunks of their own and never reach this.
fn value_declarations<'t>(
    language: &str,
    node: Node<'t>,
    source: &[u8],
) -> Vec<ValueDeclaration<'t>> {
    let text = |name: Node<'_>| {
        name.utf8_text(source)
            .ok()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    };
    let value = |kind, name: Option<Node<'_>>, statement| {
        name.and_then(text).map(|name| ValueDeclaration {
            kind,
            name,
            statement,
        })
    };
    let parent_kind = effective_parent_kind(language, node);
    let at_root = effective_parent(language, node).is_some_and(|parent| parent.parent().is_none());
    let mut values = Vec::new();
    match (language, node.kind()) {
        ("rust", "const_item" | "static_item") => {
            values.extend(value("constant", node.child_by_field_name("name"), node));
        }
        ("go", "const_declaration" | "var_declaration") => {
            let mut specs = Vec::new();
            for child in named_children_of(node) {
                match child.kind() {
                    "const_spec" | "var_spec" => specs.push(child),
                    "var_spec_list" => specs.extend(
                        named_children_of(child)
                            .into_iter()
                            .filter(|spec| spec.kind() == "var_spec"),
                    ),
                    _ => {}
                }
            }
            for spec in specs {
                // A grouped `const ( … )` block: each spec's own line is its location.
                let statement = if spec.start_position().row == node.start_position().row {
                    node
                } else {
                    spec
                };
                let mut cursor = spec.walk();
                for name in spec.children_by_field_name("name", &mut cursor) {
                    values.extend(value("constant", Some(name), statement));
                }
            }
        }
        ("typescript" | "javascript", "lexical_declaration" | "variable_declaration")
            if at_root =>
        {
            for declarator in named_children_of(node) {
                if declarator.kind() != "variable_declarator" {
                    continue;
                }
                let name = declarator
                    .child_by_field_name("name")
                    .filter(|name| name.kind() == "identifier");
                values.extend(value("constant", name, node));
            }
        }
        ("typescript", "public_field_definition") if parent_kind == Some("class_body") => {
            values.extend(value("property", node.child_by_field_name("name"), node));
        }
        ("javascript", "field_definition") if parent_kind == Some("class_body") => {
            values.extend(value(
                "property",
                node.child_by_field_name("property"),
                node,
            ));
        }
        ("python", "expression_statement") => {
            let in_class = effective_parent(language, node).is_some_and(|parent| {
                parent.kind() == "block"
                    && parent
                        .parent()
                        .is_some_and(|owner| owner.kind() == "class_definition")
            });
            let kind = if at_root {
                "constant"
            } else if in_class {
                "property"
            } else {
                return values;
            };
            let name = node
                .named_child(0)
                .filter(|child| child.kind() == "assignment")
                .and_then(|assignment| assignment.child_by_field_name("left"))
                .filter(|left| left.kind() == "identifier");
            values.extend(value(kind, name, node));
        }
        ("java", "field_declaration" | "constant_declaration") => {
            let modifiers = named_children_of(node)
                .into_iter()
                .find(|child| child.kind() == "modifiers")
                .and_then(|modifiers| modifiers.utf8_text(source).ok())
                .unwrap_or_default();
            let has = |word| {
                modifiers
                    .split_whitespace()
                    .any(|modifier| modifier == word)
            };
            let kind = if node.kind() == "constant_declaration" || (has("static") && has("final")) {
                "constant"
            } else {
                "property"
            };
            let mut cursor = node.walk();
            for declarator in node.children_by_field_name("declarator", &mut cursor) {
                values.extend(value(kind, declarator.child_by_field_name("name"), node));
            }
        }
        ("csharp", "field_declaration") => {
            let children = named_children_of(node);
            let has = |word| {
                children.iter().any(|child| {
                    child.kind() == "modifier" && child.utf8_text(source).ok() == Some(word)
                })
            };
            let kind = if has("const") || (has("static") && has("readonly")) {
                "constant"
            } else {
                "property"
            };
            for declaration in children
                .iter()
                .filter(|child| child.kind() == "variable_declaration")
            {
                for declarator in named_children_of(*declaration) {
                    if declarator.kind() == "variable_declarator" {
                        values.extend(value(kind, declarator.child_by_field_name("name"), node));
                    }
                }
            }
        }
        _ => {}
    }
    values
}

/// The only `variable_declarator` of a TS/JS declaration when its value is a function.
fn single_function_declarator(node: Node<'_>) -> Option<Node<'_>> {
    let declarators = named_children_of(node)
        .into_iter()
        .filter(|child| child.kind() == "variable_declarator")
        .collect::<Vec<_>>();
    match declarators.as_slice() {
        [declarator]
            if declarator
                .child_by_field_name("value")
                .is_some_and(|value| FUNCTION_VALUE_KINDS.contains(&value.kind())) =>
        {
            Some(*declarator)
        }
        _ => None,
    }
}

fn kotlin_class_kind(node: Node<'_>, source: &[u8]) -> &'static str {
    let mut cursor = node.walk();
    let children = node.children(&mut cursor).collect::<Vec<_>>();
    if children.iter().any(|child| child.kind() == "interface") {
        return "interface";
    }
    let is_enum = children
        .iter()
        .filter(|child| child.kind() == "modifiers")
        .flat_map(|modifiers| named_children_of(*modifiers))
        .any(|modifier| {
            modifier.kind() == "class_modifier" && modifier.utf8_text(source).ok() == Some("enum")
        });
    if is_enum {
        "enum"
    } else {
        "class"
    }
}

/// Name of a declaration and the owner segments that precede it in the symbol path.
fn declaration_name(
    language: &str,
    node: Node<'_>,
    source: &[u8],
) -> (Option<String>, Vec<String>) {
    match (language, node.kind()) {
        ("c" | "cpp", "function_definition") => match function_definition_name(node, source) {
            Some((name, owner)) => (Some(name), owner),
            None => (node_name(node, source), Vec::new()),
        },
        ("go", "method_declaration") => {
            let receiver = node
                .child_by_field_name("receiver")
                .and_then(|receiver| {
                    named_children_of(receiver)
                        .into_iter()
                        .find(|parameter| parameter.kind() == "parameter_declaration")
                })
                .and_then(|parameter| parameter.child_by_field_name("type"))
                .and_then(|ty| identifier_in(ty, source));
            (
                chunk_name(language, node, source),
                receiver.into_iter().collect(),
            )
        }
        _ => (chunk_name(language, node, source), Vec::new()),
    }
}

fn chunk_name(language: &str, node: Node<'_>, source: &[u8]) -> Option<String> {
    match (language, node.kind()) {
        ("kotlin", "property_declaration") => named_children_of(node)
            .into_iter()
            .find(|child| {
                matches!(
                    child.kind(),
                    "variable_declaration" | "multi_variable_declaration"
                )
            })
            .and_then(|declaration| identifier_in(declaration, source)),
        ("kotlin", "companion_object") => Some(
            node.child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                .unwrap_or("companion")
                .to_owned(),
        ),
        ("kotlin", "secondary_constructor") => Some("constructor".to_owned()),
        ("go", "type_declaration") => go_specs(node)
            .into_iter()
            .next()
            .and_then(|spec| spec.child_by_field_name("name"))
            .and_then(|name| identifier_in(name, source)),
        ("typescript" | "javascript", "lexical_declaration" | "variable_declaration") => {
            single_function_declarator(node)
                .and_then(|declarator| declarator.child_by_field_name("name"))
                .and_then(|name| identifier_in(name, source))
        }
        ("javascript", "field_definition") => node
            .child_by_field_name("property")
            .and_then(|name| identifier_in(name, source)),
        // Only the `name` field: an anonymous specifier must not be named after its first field.
        (
            "c" | "cpp",
            "struct_specifier" | "enum_specifier" | "union_specifier" | "class_specifier",
        ) => node
            .child_by_field_name("name")
            .and_then(|name| qualified_parts(name, source))
            .map(|(name, _)| name),
        _ => node_name(node, source),
    }
}

/// Name and scope of a C/C++ function definition: `void ns::Widget::draw()` → ("draw",
/// ["ns", "Widget"]).
fn function_definition_name(node: Node<'_>, source: &[u8]) -> Option<(String, Vec<String>)> {
    let mut declarator = node.child_by_field_name("declarator")?;
    // Pointer, reference and parenthesized declarators wrap the function declarator.
    while declarator.kind() != "function_declarator" {
        declarator = declarator.child_by_field_name("declarator").or_else(|| {
            named_children_of(declarator)
                .into_iter()
                .find(|child| child.kind().ends_with("declarator"))
        })?;
    }
    qualified_parts(declarator.child_by_field_name("declarator")?, source)
}

/// Last name of a possibly qualified C++ identifier and its scope segments.
fn qualified_parts(node: Node<'_>, source: &[u8]) -> Option<(String, Vec<String>)> {
    let mut scope = Vec::new();
    let mut current = node;
    while current.kind() == "qualified_identifier" {
        if let Some(segment) = current.child_by_field_name("scope") {
            let segment = match segment.kind() {
                "template_type" => segment.child_by_field_name("name").unwrap_or(segment),
                _ => segment,
            };
            if let Ok(text) = segment.utf8_text(source) {
                scope.push(text.trim().to_owned());
            }
        }
        current = current.child_by_field_name("name")?;
    }
    if matches!(
        current.kind(),
        "template_function" | "template_type" | "template_method"
    ) {
        current = current.child_by_field_name("name").unwrap_or(current);
    }
    let name = current.utf8_text(source).ok()?.trim();
    (!name.is_empty()).then(|| (name.to_owned(), scope))
}

/// Dotted name of a package or namespace declaration (`App\Http` → `App.Http`).
fn namespace_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name").or_else(|| {
        named_children_of(node)
            .into_iter()
            .rev()
            .find(|child| !is_comment(child.kind()) && !ATTRIBUTE_KINDS.contains(&child.kind()))
    })?;
    let text = name.utf8_text(source).ok()?;
    let mut dotted = String::new();
    for segment in text
        .split(|character: char| matches!(character, '.' | '\\' | ':') || character.is_whitespace())
        .filter(|segment| !segment.is_empty())
    {
        if !dotted.is_empty() {
            dotted.push('.');
        }
        dotted.push_str(segment);
    }
    (!dotted.is_empty()).then_some(dotted)
}

fn node_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    for field in ["name", "declarator", "type"] {
        if let Some(child) = node.child_by_field_name(field) {
            if let Some(name) = identifier_in(child, source) {
                return Some(name);
            }
        }
    }
    identifier_in(node, source)
}

fn identifier_in(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut stack = vec![(node, 0usize)];
    while let Some((candidate, depth)) = stack.pop() {
        let kind = candidate.kind();
        if (kind == "identifier"
            || kind == "name"
            || kind.ends_with("_identifier")
            || matches!(kind, "constant" | "type_identifier"))
            && candidate.child_count() == 0
        {
            return candidate
                .utf8_text(source)
                .ok()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned);
        }
        if depth < 8 {
            let mut cursor = candidate.walk();
            let children = candidate.named_children(&mut cursor).collect::<Vec<_>>();
            stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
        }
    }
    None
}

fn is_comment(kind: &str) -> bool {
    kind == "comment" || kind.ends_with("_comment")
}

fn dedupe(chunks: &mut Vec<AdapterChunk>) {
    chunks.sort_by(|left, right| {
        (left.start_line, left.end_line, &left.kind, &left.name).cmp(&(
            right.start_line,
            right.end_line,
            &right.kind,
            &right.name,
        ))
    });
    chunks.dedup_by(|left, right| {
        left.start_line == right.start_line
            && left.end_line == right.end_line
            && left.kind == right.kind
            && left.name == right.name
    });
}

fn estimate_memory(source_bytes: usize, nodes: usize) -> usize {
    source_bytes
        .saturating_mul(2)
        .saturating_add(nodes.saturating_mul(128))
}
