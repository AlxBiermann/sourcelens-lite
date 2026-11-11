//! Deterministic chunking for code and text.
//!
//! Tree-sitter languages go through the adapter; their top-level imports and statements merge
//! into budgeted runs, containers that fit the budget absorb their members, small siblings merge
//! forward, and oversized chunks split at AST boundaries under a non-whitespace character budget.
//! Markdown has its own scanner; everything else is cut into line windows.

use std::cmp::Reverse;

use super::{adapter, markdown};

pub const CHUNKER_VERSION: i64 = 1;
/// Chunk budget in non-whitespace characters, measured over whole lines.
pub const MAX_CHUNK_CHARS: u32 = 2000;
/// Sibling chunks below this many non-whitespace characters merge with the next sibling.
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
    pub(super) range: LineRange,
    /// Start lines of direct body members, used as AST-aligned split points.
    pub(super) boundaries: Vec<u32>,
    pub(super) signature: Option<String>,
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

    let raw_chunks = match language {
        Some("markdown") => markdown::markdown_chunks(&source),
        Some(language) => adapter::parse(language, path, content)
            .map(|chunks| {
                let line_count = source.line_count();
                let raw_chunks = chunks
                    .into_iter()
                    .map(|chunk| {
                        let start = chunk.start_line.clamp(1, line_count);
                        RawChunk {
                            kind: adapter_chunk_kind(&chunk.kind),
                            name: chunk.name,
                            range: line_range(start, chunk.end_line.min(line_count).max(start)),
                            boundaries: chunk.child_boundaries,
                            signature: chunk.signature,
                        }
                    })
                    .collect();
                merge_sibling_chunks(&source, raw_chunks)
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let mut raw_chunks = if raw_chunks.is_empty() {
        fallback_chunks(&source, "fallback")
    } else {
        raw_chunks
    };

    raw_chunks.sort_by_key(|chunk| {
        (
            chunk.range.start,
            Reverse(chunk.range.end),
            !is_parent_kind(chunk.kind),
            chunk.kind,
        )
    });
    let parents = parent_indices(&raw_chunks);
    let mut symbol_paths: Vec<Vec<String>> = Vec::with_capacity(raw_chunks.len());
    for (index, chunk) in raw_chunks.iter().enumerate() {
        let mut path = parents[index]
            .map(|parent| symbol_paths[parent].clone())
            .unwrap_or_default();
        if !matches!(chunk.kind, "imports" | "package" | "fallback") {
            if let Some(name) = &chunk.name {
                path.push(name.clone());
            }
        }
        symbol_paths.push(path);
    }

    let symbols = collect_symbols(&source, &raw_chunks, &symbol_paths);
    let absorbed = absorbed_chunks(&source, &raw_chunks, &parents);
    let (logical, logical_parents, logical_paths) =
        merge_small_siblings(&source, raw_chunks, &parents, symbol_paths, &absorbed);

    let chunks = split_chunks(&source, &logical, &logical_parents)
        .into_iter()
        .map(|part| {
            let names = symbols
                .iter()
                .filter(|symbol| {
                    symbol.start_line >= part.range.start && symbol.start_line <= part.range.end
                })
                .map(|symbol| symbol.name.clone())
                .collect();
            let (start_byte, end_byte) = source.byte_range(part.range);
            Chunk {
                kind: part.kind,
                symbol_path: logical_paths[part.logical_index].clone(),
                names,
                start_line: part.range.start,
                end_line: part.range.end,
                start_byte,
                end_byte,
            }
        })
        .collect();
    (chunks, symbols)
}

/// Direct parent of each chunk: the smallest enclosing earlier chunk of a parent kind.
fn parent_indices(chunks: &[RawChunk]) -> Vec<Option<usize>> {
    chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            chunks
                .iter()
                .enumerate()
                .filter(|(candidate_index, candidate)| {
                    *candidate_index < index
                        && is_parent_kind(candidate.kind)
                        && (candidate.range != chunk.range || !is_parent_kind(chunk.kind))
                        && candidate.range.start <= chunk.range.start
                        && candidate.range.end >= chunk.range.end
                })
                .min_by_key(|(_, candidate)| candidate.range.end - candidate.range.start)
                .map(|(index, _)| index)
        })
        .collect()
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
fn collect_symbols(
    source: &SourceFile,
    chunks: &[RawChunk],
    symbol_paths: &[Vec<String>],
) -> Vec<Symbol> {
    let package = chunks
        .iter()
        .filter(|chunk| chunk.kind == "package")
        .find_map(|chunk| package_name(source, chunk.range));
    chunks
        .iter()
        .zip(symbol_paths)
        .filter(|(chunk, path)| is_symbol_kind(chunk.kind) && !path.is_empty())
        .filter_map(|(chunk, path)| {
            let name = path.last()?.clone();
            let joined = path.join(".");
            Some(Symbol {
                kind: chunk.kind,
                name,
                qualified_name: match &package {
                    Some(package) => format!("{package}.{joined}"),
                    None => joined,
                },
                signature: chunk.signature.clone(),
                start_line: chunk.range.start,
                end_line: chunk.range.end,
                depth: path.len() as u32 - 1,
            })
        })
        .collect()
}

fn package_name(source: &SourceFile, range: LineRange) -> Option<String> {
    (range.start..=range.end).find_map(|line| {
        source
            .line(line)
            .trim()
            .strip_prefix("package ")
            .map(|value| value.trim().trim_end_matches(';').to_owned())
            .filter(|value| !value.is_empty())
    })
}

/// Containers that fit the budget absorb all their descendants, so member content is not
/// emitted twice.
fn absorbed_chunks(
    source: &SourceFile,
    chunks: &[RawChunk],
    parents: &[Option<usize>],
) -> Vec<bool> {
    let mut absorbed = vec![false; chunks.len()];
    for (index, chunk) in chunks.iter().enumerate() {
        if absorbed[index]
            || !is_parent_kind(chunk.kind)
            || source.measure(chunk.range) > MAX_CHUNK_CHARS
        {
            continue;
        }
        for descendant in index + 1..chunks.len() {
            let mut ancestor = parents[descendant];
            while let Some(current) = ancestor {
                if current == index {
                    absorbed[descendant] = true;
                    break;
                }
                ancestor = parents[current];
            }
        }
    }
    absorbed
}

/// Drops absorbed chunks and merges each small childless sibling forward into one `block`.
/// Returns the logical chunks with their parents and symbol paths.
fn merge_small_siblings(
    source: &SourceFile,
    chunks: Vec<RawChunk>,
    parents: &[Option<usize>],
    symbol_paths: Vec<Vec<String>>,
    absorbed: &[bool],
) -> (Vec<RawChunk>, Vec<Option<usize>>, Vec<Vec<String>>) {
    let mut has_children = vec![false; chunks.len()];
    for (index, parent) in parents.iter().enumerate() {
        if let Some(parent) = parent {
            if !absorbed[index] {
                has_children[*parent] = true;
            }
        }
    }
    let mergeable =
        |index: usize, kind: &str| !has_children[index] && !matches!(kind, "imports" | "package");

    let mut new_index = vec![None; chunks.len()];
    let mut merged: Vec<RawChunk> = Vec::with_capacity(chunks.len());
    let mut merged_parents: Vec<Option<usize>> = Vec::with_capacity(chunks.len());
    let mut merged_paths: Vec<Vec<String>> = Vec::with_capacity(chunks.len());
    let mut merged_mergeable: Vec<bool> = Vec::with_capacity(chunks.len());
    for ((index, chunk), path) in chunks.into_iter().enumerate().zip(symbol_paths) {
        if absorbed[index] {
            continue;
        }
        let parent = parents[index].and_then(|parent| new_index[parent]);
        let chunk_mergeable = mergeable(index, chunk.kind);
        if let Some(last) = merged.len().checked_sub(1) {
            let previous = &merged[last];
            let end = previous.range.end.max(chunk.range.end);
            if chunk_mergeable
                && merged_mergeable[last]
                && merged_parents[last] == parent
                && source.measure(previous.range) < MIN_CHUNK_CHARS
                && source.measure(line_range(previous.range.start, end)) <= MAX_CHUNK_CHARS
            {
                let previous = &mut merged[last];
                previous.range.end = end;
                previous.kind = "block";
                previous.name = None;
                previous.signature = None;
                previous.boundaries.push(chunk.range.start);
                merged_paths[last] = parent
                    .map(|parent| merged_paths[parent].clone())
                    .unwrap_or_default();
                new_index[index] = Some(last);
                continue;
            }
        }
        new_index[index] = Some(merged.len());
        merged.push(chunk);
        merged_parents.push(parent);
        merged_paths.push(path);
        merged_mergeable.push(chunk_mergeable);
    }
    (merged, merged_parents, merged_paths)
}

fn adapter_chunk_kind(kind: &str) -> &'static str {
    match kind {
        "imports" => "imports",
        "package" => "package",
        "statement" => "statement",
        "function" => "function",
        "method" => "method",
        "constructor" => "constructor",
        "class" => "class",
        "interface" => "interface",
        "struct" => "struct",
        "enum" => "enum",
        "union" => "union",
        "trait" => "trait",
        "impl" => "impl",
        "record" => "record",
        "object" => "object",
        "companion_object" => "companion_object",
        "macro" => "macro",
        "module" => "module",
        "type" => "type",
        "property" => "property",
        "extension" => "extension",
        _ => "block",
    }
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
    merged.push(RawChunk {
        kind: if run_imports == Some(true) {
            "imports"
        } else {
            "block"
        },
        name: None,
        range: line_range(first.range.start, end),
        boundaries,
        signature: None,
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

/// Splits every logical chunk into parts under the character budget. `parents`
/// holds each chunk's direct parent index; oversized containers emit their header,
/// gaps between members and tail, so member content is never duplicated.
fn split_chunks(
    source: &SourceFile,
    chunks: &[RawChunk],
    parents: &[Option<usize>],
) -> Vec<ChunkPart> {
    let budget = MAX_CHUNK_CHARS;
    let mut parts = Vec::with_capacity(chunks.len());
    for (logical_index, chunk) in chunks.iter().enumerate() {
        let ranges = if source.measure(chunk.range) <= budget {
            vec![chunk.range]
        } else {
            let members = parents
                .iter()
                .enumerate()
                .filter(|(_, parent)| **parent == Some(logical_index))
                .map(|(member, _)| chunks[member].range)
                .collect::<Vec<_>>();
            if is_parent_kind(chunk.kind) && !members.is_empty() {
                container_segments(source, chunk.range, &members, budget)
            } else {
                leaf_segments(source, chunk.range, &chunk.boundaries, budget)
            }
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
    budget: u32,
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
        if source.measure(segment) <= budget {
            ranges.push(segment);
        } else {
            ranges.extend(window_split(source, segment.start, segment.end, budget));
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
fn leaf_segments(
    source: &SourceFile,
    range: LineRange,
    boundaries: &[u32],
    budget: u32,
) -> Vec<LineRange> {
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
            if source.measure(extended) <= budget {
                pack = Some(extended);
                continue;
            }
            ranges.push(current);
        }
        if source.measure(segment) <= budget {
            pack = Some(segment);
        } else {
            ranges.extend(window_split(source, segment.start, segment.end, budget));
        }
    }
    ranges.extend(pack);
    ranges
}

/// Line windows over `start..=end`, each extended while it fits the budget and then
/// backed off to the last blank or lone `}` line. Every window holds at least one
/// line, so a single over-budget line still terminates.
fn window_split(source: &SourceFile, start: u32, end: u32, budget: u32) -> Vec<LineRange> {
    let mut windows = Vec::new();
    let mut cursor = start;
    while cursor <= end {
        let mut window_end = cursor;
        while window_end < end && source.measure(line_range(cursor, window_end + 1)) <= budget {
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

fn fallback_chunks(source: &SourceFile, kind: &'static str) -> Vec<RawChunk> {
    window_split(source, 1, source.line_count(), MAX_CHUNK_CHARS)
        .into_iter()
        .map(|range| raw(kind, format!("{kind}-{}", range.start), range))
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

pub(super) fn dedupe_chunks(mut chunks: Vec<RawChunk>) -> Vec<RawChunk> {
    chunks.sort_by_key(|chunk| (chunk.range.start, chunk.range.end, chunk.kind));
    chunks.dedup_by(|left, right| {
        left.kind == right.kind && left.range == right.range && left.name == right.name
    });
    chunks
}

pub(super) fn raw(kind: &'static str, name: String, range: LineRange) -> RawChunk {
    RawChunk {
        kind,
        name: Some(name),
        range,
        boundaries: Vec::new(),
        signature: None,
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
