//! Deterministic chunking for code and text.
//!
//! Tree-sitter languages go through the adapter; their top-level imports and statements merge
//! into budgeted runs, containers that fit the budget absorb their members, small siblings merge
//! forward, and oversized chunks split at AST boundaries under a non-whitespace character budget.
//! Markdown has its own scanner; everything else is cut into line windows.

use std::cmp::Reverse;

use super::adapter::{self, AdapterChunk};
use super::markdown;

pub const CHUNKER_VERSION: i64 = 2;
/// Chunk budget in non-whitespace characters, measured over whole lines.
pub const MAX_CHUNK_CHARS: u32 = 2000;
/// Sibling chunks that are both below this many non-whitespace characters merge.
pub const MIN_CHUNK_CHARS: u32 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub kind: &'static str,
    pub symbol_path: Vec<String>,
    /// Names of the symbols that start inside this chunk.
    pub names: Vec<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// Byte offsets into the file content; `end_byte` excludes the last line's newline.
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub kind: &'static str,
    pub name: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub depth: u32,
}

/// 1-based, inclusive line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LineRange {
    pub(super) start: u32,
    pub(super) end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RawChunk {
    pub(super) kind: &'static str,
    pub(super) name: Option<String>,
    /// Segments placed before `name` in the symbol path (Go receiver, C++ scope).
    pub(super) owner: Vec<String>,
    /// Package or namespace prefixed to qualified names.
    pub(super) namespace: Option<String>,
    pub(super) range: LineRange,
    /// Start lines of direct body members, used as AST-aligned split points.
    pub(super) boundaries: Vec<u32>,
    pub(super) signature: Option<String>,
    /// Contributes a symbol only; never becomes a chunk.
    pub(super) symbol_only: bool,
}

/// A raw chunk with its place in the declaration tree.
#[derive(Debug, Clone)]
struct Logical {
    chunk: RawChunk,
    /// Index of the direct parent in the same list.
    parent: Option<usize>,
    path: Vec<String>,
    mergeable: bool,
}

#[derive(Debug, Clone)]
struct ChunkPart {
    logical_index: usize,
    kind: &'static str,
    range: LineRange,
}

#[derive(Debug, Clone)]
pub(super) struct SourceFile<'a> {
    lines: Vec<&'a str>,
    sanitized: Vec<String>,
    /// `nonws_prefix[i]` = non-whitespace chars in lines `1..=i`; `nonws_prefix[0] = 0`.
    nonws_prefix: Vec<u32>,
    /// Byte offset of the start of each line.
    line_starts: Vec<usize>,
}

/// Chunks and symbols of one file. `content` must be LF-normalised.
pub fn chunk_file(path: &str, language: Option<&str>, content: &str) -> (Vec<Chunk>, Vec<Symbol>) {
    if content.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }
    let source = SourceFile::new(content);
    let logical = logical_chunks(raw_chunks(path, language, content, &source));
    let symbols = collect_symbols(&logical);
    let absorbed = absorbed_chunks(&source, &logical);
    let logical = merge_small_siblings(&source, logical, &absorbed);

    let mut symbol_starts: Vec<(u32, &str)> = symbols
        .iter()
        .map(|symbol| (symbol.start_line, symbol.name.as_str()))
        .collect();
    symbol_starts.sort_by_key(|(line, _)| *line);
    let chunks = split_chunks(&source, &logical)
        .into_iter()
        .map(|part| {
            let from = symbol_starts.partition_point(|(line, _)| *line < part.range.start);
            let to = symbol_starts.partition_point(|(line, _)| *line <= part.range.end);
            let (start_byte, end_byte) = source.byte_range(part.range);
            Chunk {
                kind: part.kind,
                symbol_path: logical[part.logical_index].path.clone(),
                names: symbol_starts[from..to.max(from)]
                    .iter()
                    .map(|(_, name)| (*name).to_owned())
                    .collect(),
                start_line: part.range.start,
                end_line: part.range.end,
                start_byte,
                end_byte,
            }
        })
        .collect();
    (chunks, symbols)
}

/// Declaration, statement and symbol-only chunks from the adapter or the Markdown scanner, or
/// line windows when neither yields a chunk.
fn raw_chunks(
    path: &str,
    language: Option<&str>,
    content: &str,
    source: &SourceFile,
) -> Vec<RawChunk> {
    let chunks = match language {
        Some("markdown") => markdown::markdown_chunks(source),
        Some(language) => adapter::parse(language, path, content)
            .map(|chunks| adapter_raw_chunks(source, chunks))
            .unwrap_or_default(),
        None => Vec::new(),
    };
    if chunks.iter().all(|chunk| chunk.symbol_only) {
        fallback_chunks(source)
    } else {
        chunks
    }
}

fn adapter_raw_chunks(source: &SourceFile, chunks: Vec<AdapterChunk>) -> Vec<RawChunk> {
    let line_count = source.line_count();
    let (symbol_only, chunks): (Vec<RawChunk>, Vec<RawChunk>) = chunks
        .into_iter()
        .map(|chunk| {
            let start = chunk.start_line.clamp(1, line_count);
            RawChunk {
                kind: chunk.kind,
                name: chunk.name,
                owner: chunk.owner,
                namespace: chunk.namespace,
                range: line_range(start, chunk.end_line.min(line_count).max(start)),
                boundaries: chunk.child_boundaries,
                signature: chunk.signature,
                symbol_only: chunk.symbol_only,
            }
        })
        .partition(|chunk| chunk.symbol_only);
    let mut chunks = merge_sibling_chunks(source, chunks);
    chunks.extend(symbol_only);
    chunks
}

/// Sorts the raw chunks into document order and links each to its parent and symbol path.
fn logical_chunks(mut chunks: Vec<RawChunk>) -> Vec<Logical> {
    chunks.sort_by_key(|chunk| {
        (
            chunk.range.start,
            Reverse(chunk.range.end),
            !is_parent_kind(chunk.kind),
            chunk.kind,
        )
    });
    let parents = parent_indices(&chunks);
    let paths = symbol_paths(&chunks, &parents);
    chunks
        .into_iter()
        .zip(parents)
        .zip(paths)
        .map(|((chunk, parent), path)| Logical {
            chunk,
            parent,
            path,
            mergeable: false,
        })
        .collect()
}

/// Direct parent of each chunk: the smallest enclosing earlier chunk of a parent kind. `chunks`
/// is sorted by start, then by end descending, so enclosing chunks form a stack.
fn parent_indices(chunks: &[RawChunk]) -> Vec<Option<usize>> {
    let mut parents = Vec::with_capacity(chunks.len());
    let mut open: Vec<usize> = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        while open
            .last()
            .is_some_and(|&top| chunks[top].range.end < chunk.range.start)
        {
            open.pop();
        }
        let parent = open.iter().rev().copied().find(|&candidate| {
            let candidate = &chunks[candidate];
            candidate.range.end >= chunk.range.end
                && (candidate.range != chunk.range || !is_parent_kind(chunk.kind))
        });
        parents.push(parent);
        if is_parent_kind(chunk.kind) && !chunk.symbol_only {
            open.push(index);
        }
    }
    parents
}

/// Parent path plus the chunk's owner segments and name; imports, packages and unnamed chunks
/// (statements, Markdown fences, lists, tables, line windows) add nothing.
fn symbol_paths(chunks: &[RawChunk], parents: &[Option<usize>]) -> Vec<Vec<String>> {
    let mut paths: Vec<Vec<String>> = Vec::with_capacity(chunks.len());
    for (chunk, parent) in chunks.iter().zip(parents) {
        let mut path = parent
            .map(|parent| paths[parent].clone())
            .unwrap_or_default();
        if let Some(name) = &chunk.name {
            if !matches!(chunk.kind, "imports" | "package") {
                path.extend(chunk.owner.iter().cloned());
                path.push(name.clone());
            }
        }
        paths.push(path);
    }
    paths
}

fn is_symbol_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function"
            | "method"
            | "constructor"
            | "class"
            | "struct"
            | "trait"
            | "impl"
            | "union"
            | "macro"
            | "interface"
            | "enum"
            | "record"
            | "type"
            | "object"
            | "companion_object"
            | "property"
            | "constant"
            | "module"
            | "extension"
            | "heading_1"
            | "heading_2"
            | "heading_3"
            | "heading_4"
            | "heading_5"
            | "heading_6"
    )
}

/// Symbols come from the raw declaration chunks, independent of how chunks are merged or split.
fn collect_symbols(logical: &[Logical]) -> Vec<Symbol> {
    logical
        .iter()
        .filter(|item| is_symbol_kind(item.chunk.kind) && item.chunk.name.is_some())
        .filter_map(|item| {
            let name = item.path.last()?.clone();
            let joined = item.path.join(".");
            Some(Symbol {
                kind: item.chunk.kind,
                name,
                qualified_name: match &item.chunk.namespace {
                    Some(namespace) => format!("{namespace}.{joined}"),
                    None => joined,
                },
                signature: item.chunk.signature.clone(),
                start_line: item.chunk.range.start,
                end_line: item.chunk.range.end,
                depth: (item.path.len() - 1).saturating_sub(item.chunk.owner.len()) as u32,
            })
        })
        .collect()
}

/// Containers that fit the budget absorb all their descendants, so member content is not
/// emitted twice. Descendants follow their container contiguously in document order.
fn absorbed_chunks(source: &SourceFile, logical: &[Logical]) -> Vec<bool> {
    let mut absorbed = vec![false; logical.len()];
    for (index, item) in logical.iter().enumerate() {
        if absorbed[index]
            || !is_parent_kind(item.chunk.kind)
            || source.measure(item.chunk.range) > MAX_CHUNK_CHARS
        {
            continue;
        }
        for descendant in index + 1..logical.len() {
            if logical[descendant].chunk.range.start > item.chunk.range.end {
                break;
            }
            let mut ancestor = logical[descendant].parent;
            while let Some(current) = ancestor {
                if current == index {
                    absorbed[descendant] = true;
                    break;
                }
                ancestor = logical[current].parent;
            }
        }
    }
    absorbed
}

/// Drops absorbed and symbol-only chunks and merges runs of small childless siblings into one
/// `block`; a sibling at or above `MIN_CHUNK_CHARS` keeps its own chunk, kind and name.
fn merge_small_siblings(
    source: &SourceFile,
    logical: Vec<Logical>,
    absorbed: &[bool],
) -> Vec<Logical> {
    let mut has_children = vec![false; logical.len()];
    for (index, item) in logical.iter().enumerate() {
        if let Some(parent) = item.parent {
            if !absorbed[index] && !item.chunk.symbol_only {
                has_children[parent] = true;
            }
        }
    }

    let mut new_index: Vec<Option<usize>> = vec![None; logical.len()];
    let mut merged: Vec<Logical> = Vec::with_capacity(logical.len());
    for (index, mut item) in logical.into_iter().enumerate() {
        if absorbed[index] || item.chunk.symbol_only {
            continue;
        }
        let parent = item.parent.and_then(|parent| new_index[parent]);
        let mergeable = !has_children[index] && !matches!(item.chunk.kind, "imports" | "package");
        let parent_path = parent.map(|parent| merged[parent].path.clone());
        if let Some(last) = merged.last_mut() {
            let end = last.chunk.range.end.max(item.chunk.range.end);
            if mergeable
                && last.mergeable
                && last.parent == parent
                && source.measure(last.chunk.range) < MIN_CHUNK_CHARS
                && source.measure(item.chunk.range) < MIN_CHUNK_CHARS
                && source.measure(line_range(last.chunk.range.start, end)) <= MAX_CHUNK_CHARS
            {
                last.chunk.range.end = end;
                last.chunk.kind = "block";
                last.chunk.name = None;
                last.chunk.signature = None;
                last.chunk.boundaries.push(item.chunk.range.start);
                last.path = parent_path.unwrap_or_default();
                new_index[index] = Some(merged.len() - 1);
                continue;
            }
        }
        item.parent = parent;
        item.mergeable = mergeable;
        new_index[index] = Some(merged.len());
        merged.push(item);
    }
    merged
}

impl<'a> SourceFile<'a> {
    pub(super) fn new(content: &'a str) -> Self {
        let mut lines: Vec<&str> = content.split('\n').collect();
        if content.ends_with('\n') {
            lines.pop();
        }
        let sanitized = sanitize_lines(&lines);
        let mut nonws_prefix = Vec::with_capacity(lines.len() + 1);
        let mut line_starts = Vec::with_capacity(lines.len());
        nonws_prefix.push(0);
        let mut total = 0_u32;
        let mut offset = 0_usize;
        for line in &lines {
            total += line
                .chars()
                .filter(|character| !character.is_whitespace())
                .count() as u32;
            nonws_prefix.push(total);
            line_starts.push(offset);
            offset += line.len() + 1;
        }
        Self {
            lines,
            sanitized,
            nonws_prefix,
            line_starts,
        }
    }

    pub(super) fn line_count(&self) -> u32 {
        self.lines.len() as u32
    }

    /// Non-whitespace characters in the inclusive line range.
    fn measure(&self, range: LineRange) -> u32 {
        let end = (range.end as usize).min(self.lines.len());
        let start = (range.start.max(1) - 1) as usize;
        self.nonws_prefix[end].saturating_sub(self.nonws_prefix[start.min(end)])
    }

    fn is_blank(&self, range: LineRange) -> bool {
        (range.start..=range.end).all(|line| self.line(line).trim().is_empty())
    }

    pub(super) fn line(&self, line: u32) -> &str {
        self.lines
            .get(line.saturating_sub(1) as usize)
            .copied()
            .unwrap_or("")
    }

    fn sanitized_line(&self, line: u32) -> &str {
        self.sanitized
            .get(line.saturating_sub(1) as usize)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// Byte offsets of the range: start of its first line to the end of its last line.
    fn byte_range(&self, range: LineRange) -> (usize, usize) {
        let offset = |line: u32| {
            self.line_starts
                .get(line.saturating_sub(1) as usize)
                .copied()
                .unwrap_or(0)
        };
        let start = offset(range.start);
        let end = offset(range.end) + self.line(range.end).len();
        (start, end.max(start))
    }
}

/// cAST merge step for adapter output: maximal runs of consecutive sibling
/// `imports`/`statement` chunks collapse into one chunk while the merged range fits
/// the budget, so every top-level line belongs to exactly one primary chunk.
/// Imports and code statements form separate runs (`imports` vs `block`); comment-only
/// statements join either. Declarations and `package` chunks are never merged.
fn merge_sibling_chunks(source: &SourceFile, mut raw_chunks: Vec<RawChunk>) -> Vec<RawChunk> {
    raw_chunks.sort_by_key(|chunk| (chunk.range.start, Reverse(chunk.range.end)));
    let mut merged = Vec::with_capacity(raw_chunks.len());
    let mut run: Vec<RawChunk> = Vec::new();
    let mut run_owner = None;
    let mut run_end = 0;
    // `Some(true)` = imports run, `Some(false)` = code run, `None` = comments only so far.
    let mut run_imports: Option<bool> = None;
    // Non-mergeable chunks enclosing the walk position: (index, end line).
    let mut open: Vec<(usize, u32)> = Vec::new();
    for (index, chunk) in raw_chunks.into_iter().enumerate() {
        while open.last().is_some_and(|&(_, end)| end < chunk.range.start) {
            open.pop();
        }
        if !matches!(chunk.kind, "imports" | "statement") {
            flush_sibling_run(&mut run, run_imports, &mut merged);
            open.push((index, chunk.range.end));
            merged.push(chunk);
            continue;
        }
        let owner = open.last().map(|&(owner, _)| owner);
        let class = if chunk.kind == "imports" {
            Some(true)
        } else if is_comment_only(source, chunk.range) {
            None
        } else {
            Some(false)
        };
        let extends = run.first().is_some_and(|first| {
            owner == run_owner
                && (class.is_none() || run_imports.is_none() || class == run_imports)
                && source.measure(line_range(first.range.start, run_end.max(chunk.range.end)))
                    <= MAX_CHUNK_CHARS
        });
        if !extends {
            flush_sibling_run(&mut run, run_imports, &mut merged);
            run_owner = owner;
            run_end = 0;
            run_imports = None;
        }
        run_end = run_end.max(chunk.range.end);
        run_imports = run_imports.or(class);
        run.push(chunk);
    }
    flush_sibling_run(&mut run, run_imports, &mut merged);
    merged
}

fn flush_sibling_run(
    run: &mut Vec<RawChunk>,
    run_imports: Option<bool>,
    merged: &mut Vec<RawChunk>,
) {
    let Some(first) = run.first() else {
        return;
    };
    let end = run
        .iter()
        .map(|chunk| chunk.range.end)
        .max()
        .unwrap_or(first.range.end);
    let boundaries = if run.len() == 1 {
        first.boundaries.clone()
    } else {
        run[1..].iter().map(|chunk| chunk.range.start).collect()
    };
    let kind = if run_imports == Some(true) {
        "imports"
    } else {
        "block"
    };
    merged.push(RawChunk {
        boundaries,
        ..raw(kind, None, line_range(first.range.start, end))
    });
    run.clear();
}

/// True when every line is blank once `//`/`/* */` comments are stripped, or is a
/// `#` line comment (`# text`, bare `#`, `#!`); `#define`-style directives are code.
fn is_comment_only(source: &SourceFile, range: LineRange) -> bool {
    (range.start..=range.end).all(|line| {
        let raw = source.line(line).trim_start();
        source.sanitized_line(line).trim().is_empty()
            || raw == "#"
            || raw.starts_with("# ")
            || raw.starts_with("#!")
    })
}

/// Splits every logical chunk into parts under the character budget. Oversized containers
/// emit their header, gaps between members and tail, so member content is never duplicated.
fn split_chunks(source: &SourceFile, logical: &[Logical]) -> Vec<ChunkPart> {
    let mut members: Vec<Vec<LineRange>> = vec![Vec::new(); logical.len()];
    for item in logical {
        if let Some(parent) = item.parent {
            members[parent].push(item.chunk.range);
        }
    }
    let mut parts = Vec::with_capacity(logical.len());
    for (logical_index, item) in logical.iter().enumerate() {
        let chunk = &item.chunk;
        let ranges = if source.measure(chunk.range) <= MAX_CHUNK_CHARS {
            vec![chunk.range]
        } else if is_parent_kind(chunk.kind) && !members[logical_index].is_empty() {
            container_segments(source, chunk.range, &members[logical_index])
        } else {
            leaf_segments(source, chunk.range, &chunk.boundaries)
        };
        for (part_index, range) in ranges.into_iter().enumerate() {
            parts.push(ChunkPart {
                logical_index,
                kind: if part_index == 0 {
                    chunk.kind
                } else {
                    "block_part"
                },
                range,
            });
        }
    }
    parts
}

/// Header, member gaps and tail of an oversized container; `members` are in source
/// order. Blank segments and segments holding only closing delimiters are dropped,
/// the header is always kept.
fn container_segments(
    source: &SourceFile,
    range: LineRange,
    members: &[LineRange],
) -> Vec<LineRange> {
    let header_end = members[0].start.saturating_sub(1).max(range.start);
    let mut segments = vec![line_range(range.start, header_end)];
    let mut covered = header_end;
    for member in members {
        if member.start > covered + 1 {
            segments.push(line_range(covered + 1, member.start - 1));
        }
        covered = covered.max(member.end);
    }
    if covered < range.end {
        segments.push(line_range(covered + 1, range.end));
    }
    let mut ranges = Vec::with_capacity(segments.len());
    for (index, segment) in segments.into_iter().enumerate() {
        if index > 0 && (source.is_blank(segment) || is_closing_only(source, segment)) {
            continue;
        }
        if source.measure(segment) <= MAX_CHUNK_CHARS {
            ranges.push(segment);
        } else {
            ranges.extend(window_split(source, segment.start, segment.end));
        }
    }
    ranges
}

fn is_closing_only(source: &SourceFile, range: LineRange) -> bool {
    (range.start..=range.end).all(|line| {
        source.line(line).chars().all(|character| {
            character.is_whitespace() || matches!(character, '}' | ')' | ']' | ';' | ',')
        })
    })
}

/// Segments `[start, b1-1], [b1, b2-1], …, [bn, end]` from member boundaries,
/// greedily packed while the pack fits the budget; a single over-budget segment
/// is windowed.
fn leaf_segments(source: &SourceFile, range: LineRange, boundaries: &[u32]) -> Vec<LineRange> {
    let mut starts = boundaries
        .iter()
        .copied()
        .filter(|line| *line > range.start && *line <= range.end)
        .collect::<Vec<_>>();
    starts.sort_unstable();
    starts.dedup();
    let mut ranges = Vec::new();
    let mut pack: Option<LineRange> = None;
    let mut segment_start = range.start;
    for segment_end in starts
        .iter()
        .map(|start| start - 1)
        .chain(std::iter::once(range.end))
    {
        let segment = line_range(segment_start, segment_end);
        segment_start = segment_end + 1;
        if let Some(current) = pack.take() {
            let extended = line_range(current.start, segment.end);
            if source.measure(extended) <= MAX_CHUNK_CHARS {
                pack = Some(extended);
                continue;
            }
            ranges.push(current);
        }
        if source.measure(segment) <= MAX_CHUNK_CHARS {
            pack = Some(segment);
        } else {
            ranges.extend(window_split(source, segment.start, segment.end));
        }
    }
    ranges.extend(pack);
    ranges
}

/// Line windows over `start..=end`, each extended while it fits the budget and then
/// backed off to the last blank or lone `}` line. Every window holds at least one
/// line, so a single over-budget line still terminates.
fn window_split(source: &SourceFile, start: u32, end: u32) -> Vec<LineRange> {
    let mut windows = Vec::new();
    let mut cursor = start;
    while cursor <= end {
        let mut window_end = cursor;
        while window_end < end
            && source.measure(line_range(cursor, window_end + 1)) <= MAX_CHUNK_CHARS
        {
            window_end += 1;
        }
        if window_end < end && window_end > cursor {
            window_end = best_split_end(source, cursor + 1, window_end);
        }
        windows.push(line_range(cursor, window_end));
        cursor = window_end + 1;
    }
    windows
}

fn best_split_end(source: &SourceFile, start: u32, target_end: u32) -> u32 {
    for line in (start..=target_end).rev() {
        let text = source.line(line).trim();
        if text.is_empty() || text == "}" {
            return line;
        }
    }

    target_end
}

fn fallback_chunks(source: &SourceFile) -> Vec<RawChunk> {
    window_split(source, 1, source.line_count())
        .into_iter()
        .map(|range| raw("fallback", None, range))
        .collect()
}

fn is_parent_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class"
            | "struct"
            | "trait"
            | "impl"
            | "union"
            | "interface"
            | "enum"
            | "record"
            | "object"
            | "companion_object"
            | "module"
            | "heading_1"
            | "heading_2"
            | "heading_3"
            | "heading_4"
            | "heading_5"
            | "heading_6"
    )
}

pub(super) fn raw(kind: &'static str, name: Option<String>, range: LineRange) -> RawChunk {
    RawChunk {
        kind,
        name,
        owner: Vec::new(),
        namespace: None,
        range,
        boundaries: Vec::new(),
        signature: None,
        symbol_only: false,
    }
}

pub(super) fn line_range(start: u32, end: u32) -> LineRange {
    LineRange { start, end }
}

fn sanitize_lines(lines: &[&str]) -> Vec<String> {
    let mut sanitized = Vec::with_capacity(lines.len());
    let mut in_block_comment = false;
    for line in lines {
        let mut output = String::with_capacity(line.len());
        let mut chars = line.chars().peekable();
        let mut quote = None;
        while let Some(character) = chars.next() {
            if in_block_comment {
                if character == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    in_block_comment = false;
                }
                output.push(' ');
                continue;
            }

            if let Some(active_quote) = quote {
                if character == '\\' {
                    output.push(' ');
                    if chars.next().is_some() {
                        output.push(' ');
                    }
                    continue;
                }
                if character == active_quote {
                    quote = None;
                }
                output.push(' ');
                continue;
            }

            if character == '/' && chars.peek() == Some(&'/') {
                output.push(' ');
                break;
            }
            if character == '/' && chars.peek() == Some(&'*') {
                chars.next();
                in_block_comment = true;
                output.push(' ');
                output.push(' ');
                continue;
            }
            if matches!(character, '"' | '\'' | '`') {
                quote = Some(character);
                output.push(' ');
                continue;
            }
            output.push(character);
        }
        sanitized.push(output);
    }

    sanitized
}
