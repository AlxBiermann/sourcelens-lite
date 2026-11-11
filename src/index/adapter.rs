//! Tree-sitter declaration walk: turns a parse tree into declaration and root-statement chunks.
//!
//! Pure: never executes repository code, retains parse trees or source, or performs I/O.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{Language, Node, ParseOptions, Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParserBudget {
    pub max_source_bytes: usize,
    pub max_ast_depth: usize,
    pub max_nodes: usize,
    pub max_parse_time: Duration,
    pub max_estimated_memory_bytes: usize,
}

impl Default for ParserBudget {
    fn default() -> Self {
        Self {
            max_source_bytes: 2 * 1024 * 1024,
            max_ast_depth: 384,
            max_nodes: 250_000,
            max_parse_time: Duration::from_secs(2),
            max_estimated_memory_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterChunk {
    pub kind: String,
    pub name: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// Start lines of the direct body members of this declaration (1-based), used as split points.
    pub child_boundaries: Vec<u32>,
    /// Declaration text up to its body, whitespace collapsed, at most 300 chars.
    pub signature: Option<String>,
}

const MAX_SIGNATURE_CHARS: usize = 300;

/// Parses `source` with the grammar of `language` and returns its declaration chunks.
/// `None` when there is no grammar, a budget is exceeded, the tree is mostly errors, or
/// no chunk was produced; callers then fall back to line windows.
pub fn parse(language: &str, path: &str, source: &str) -> Option<Vec<AdapterChunk>> {
    let (language, grammar) = grammar(language, path)?;
    parse_tree_sitter(language, path, source, ParserBudget::default(), &grammar)
}

/// Canonical grammar name and tree-sitter language for a language label.
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
        "typescript" => Some((
            "typescript",
            if path.ends_with(".tsx") {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            },
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

thread_local! {
    /// One parser per grammar and thread; reset before every parse so no document state survives.
    static PARSERS: RefCell<HashMap<&'static str, Parser>> = RefCell::new(HashMap::new());
}

fn parser_key(language: &'static str, path: &str) -> &'static str {
    match language {
        "typescript" if path.ends_with(".tsx") => "typescript:tsx",
        "typescript" => "typescript:ts",
        other => other,
    }
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
    language: &'static str,
    path: &str,
    source: &str,
    budget: ParserBudget,
    grammar: &Language,
) -> Option<Vec<AdapterChunk>> {
    let started = Instant::now();
    let source_bytes = source.len();
    if source_bytes > budget.max_source_bytes {
        return None;
    }
    let deadline = started + budget.max_parse_time;
    let tree = parse_with_cached_parser(parser_key(language, path), grammar, source, deadline)?;

    let root = tree.root_node();
    let mut walk = Walk {
        language,
        source: source.as_bytes(),
        chunks: Vec::new(),
        absorbed: HashSet::new(),
        handled: HashSet::new(),
        root_candidates: Vec::new(),
    };
    let mut node_count = 0usize;
    let mut error_bytes = 0usize;
    // (node, depth, inside_function, inside_root_statement)
    let mut stack = vec![(root, 0usize, false, false)];
    while let Some((node, depth, inside_function, inside_statement)) = stack.pop() {
        node_count += 1;
        if node_count > budget.max_nodes
            || depth > budget.max_ast_depth
            || estimate_memory(source_bytes, node_count) > budget.max_estimated_memory_bytes
            || started.elapsed() > budget.max_parse_time
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
    language: &'static str,
    source: &'s [u8],
    chunks: Vec<AdapterChunk>,
    /// Comments/attributes absorbed into a following declaration.
    absorbed: HashSet<usize>,
    /// Declarations already emitted through their transparent wrapper.
    handled: HashSet<usize>,
    /// Unclassified nodes whose effective parent is the root; emitted as `statement` unless absorbed.
    root_candidates: Vec<Node<'t>>,
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
            return self.root_statement(node);
        }
        if is_transparent(self.language, node) {
            return false;
        }
        if is_comment(kind) || ATTRIBUTE_KINDS.contains(&kind) {
            return self.root_statement(node);
        }
        if let Some(chunk_kind) = classify_node(self.language, node, self.source) {
            self.push_declaration(chunk_kind, node, node);
            return false;
        }
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
        self.chunks.push(AdapterChunk {
            kind: kind.to_owned(),
            name: chunk_name(self.language, declaration, self.source),
            start_line,
            end_line,
            child_boundaries,
            signature,
        });
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
                kind: "statement".to_owned(),
                name: None,
                start_line,
                end_line,
                child_boundaries: member_boundaries(
                    statement_container(node),
                    start_line,
                    end_line,
                ),
                signature: None,
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

/// Declaration wrapped by a transparent node (`export`, `declare`, Python decorators).
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
            "mod_item" => Some("module"),
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
        "c" => match kind {
            "function_definition" => Some("function"),
            "struct_specifier" => Some("struct"),
            "enum_specifier" => Some("enum"),
            "union_specifier" => Some("union"),
            "type_definition" => Some("type"),
            _ => None,
        },
        "cpp" => match kind {
            "function_definition" => Some("function"),
            "class_specifier" => Some("class"),
            "struct_specifier" => Some("struct"),
            "enum_specifier" => Some("enum"),
            "union_specifier" => Some("union"),
            "namespace_definition" => Some("module"),
            "type_definition" | "alias_declaration" => Some("type"),
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
        _ => node_name(node, source),
    }
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
    });
}

fn estimate_memory(source_bytes: usize, nodes: usize) -> usize {
    source_bytes
        .saturating_mul(2)
        .saturating_add(nodes.saturating_mul(128))
}
