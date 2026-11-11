//! Markdown scanner: heading sections (ATX and Setext), code fences, lists, tables, and prose
//! before the first heading.

use super::chunker::{line_range, raw, LineRange, RawChunk, SourceFile};

pub(super) fn markdown_chunks(source: &SourceFile) -> Vec<RawChunk> {
    let line_count = source.line_count();
    let mut chunks = Vec::new();
    let mut heading_stack: Vec<(usize, String, u32)> = Vec::new();
    let mut first_heading = line_count + 1;
    // Fences, lists and tables before the first heading; the lines between them are prose.
    let mut preamble_blocks: Vec<LineRange> = Vec::new();
    let mut line = front_matter_end(source).map_or(1, |end| end + 1);
    while line <= line_count {
        let text = source.line(line);
        let heading = markdown_heading(text)
            .map(|heading| (heading, 1))
            .or_else(|| setext_heading(source, line).map(|heading| (heading, 2)));
        if let Some(((level, name), height)) = heading {
            first_heading = first_heading.min(line);
            while heading_stack
                .last()
                .is_some_and(|(last_level, _, _)| *last_level >= level)
            {
                if let Some((last_level, last_name, start)) = heading_stack.pop() {
                    chunks.push(raw(
                        markdown_heading_kind(last_level),
                        Some(last_name),
                        line_range(start, line - 1),
                    ));
                }
            }
            heading_stack.push((level, name, line));
            line += height;
            continue;
        }
        let block = if is_code_fence(text) {
            let start = line;
            line += 1;
            while line <= line_count && !is_code_fence(source.line(line)) {
                line += 1;
            }
            let end = line.min(line_count);
            line += 1;
            Some(raw("code_fence", None, line_range(start, end)))
        } else if markdown_list_marker(text) {
            let start = line;
            while line <= line_count {
                let current = source.line(line);
                if !markdown_list_marker(current) && !current.trim().is_empty() {
                    break;
                }
                line += 1;
            }
            Some(raw("list", None, line_range(start, line - 1)))
        } else if markdown_table_row(text) {
            let start = line;
            while line <= line_count && markdown_table_row(source.line(line)) {
                line += 1;
            }
            Some(raw("table", None, line_range(start, line - 1)))
        } else {
            line += 1;
            None
        };
        if let Some(block) = block {
            if heading_stack.is_empty() {
                preamble_blocks.push(block.range);
            }
            chunks.push(block);
        }
    }

    while let Some((level, name, start)) = heading_stack.pop() {
        chunks.push(raw(
            markdown_heading_kind(level),
            Some(name),
            line_range(start, line_count),
        ));
    }
    chunks.extend(
        prose_runs(source, first_heading - 1, &preamble_blocks)
            .into_iter()
            .map(|range| raw("block", None, range)),
    );
    chunks
}

/// Maximal runs of non-blank lines in `1..=end` outside `blocks` (sorted, disjoint).
fn prose_runs(source: &SourceFile, end: u32, blocks: &[LineRange]) -> Vec<LineRange> {
    let mut runs = Vec::new();
    let mut blocks = blocks.iter().peekable();
    let mut run_start = None;
    for line in 1..=end {
        while blocks.next_if(|block| block.end < line).is_some() {}
        let covered = blocks.peek().is_some_and(|block| block.start <= line);
        if covered || source.line(line).trim().is_empty() {
            if let Some(start) = run_start.take() {
                runs.push(line_range(start, line - 1));
            }
        } else if run_start.is_none() {
            run_start = Some(line);
        }
    }
    if let Some(start) = run_start {
        runs.push(line_range(start, end));
    }
    runs
}

/// Last line of a YAML front matter block opened by `---` on the first line.
fn front_matter_end(source: &SourceFile) -> Option<u32> {
    if source.line(1).trim_end() != "---" {
        return None;
    }
    (2..=source.line_count()).find(|&line| matches!(source.line(line).trim_end(), "---" | "..."))
}

fn markdown_heading(text: &str) -> Option<(usize, String)> {
    let trimmed = text.trim_start();
    let level = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if level == 0 || level > 6 || !trimmed[level..].starts_with(' ') {
        return None;
    }
    Some((level, trimmed[level..].trim().to_owned()))
}

/// A paragraph line underlined by `===` (level 1) or `---` (level 2) on the next line.
fn setext_heading(source: &SourceFile, line: u32) -> Option<(usize, String)> {
    if line >= source.line_count() {
        return None;
    }
    let level = setext_underline(source.line(line + 1))?;
    let text = source.line(line);
    let is_paragraph = !text.trim().is_empty()
        && indentation(text) <= 3
        && setext_underline(text).is_none()
        && !is_code_fence(text)
        && !markdown_list_marker(text)
        && !markdown_table_row(text)
        && !text.trim_start().starts_with('>');
    is_paragraph.then(|| (level, text.trim().to_owned()))
}

fn setext_underline(text: &str) -> Option<usize> {
    if indentation(text) > 3 {
        return None;
    }
    let trimmed = text.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|character| character == '=') {
        Some(1)
    } else if trimmed.len() >= 2 && trimmed.chars().all(|character| character == '-') {
        Some(2)
    } else {
        None
    }
}

fn indentation(text: &str) -> usize {
    text.len() - text.trim_start_matches(' ').len()
}

fn markdown_heading_kind(level: usize) -> &'static str {
    match level {
        1 => "heading_1",
        2 => "heading_2",
        3 => "heading_3",
        4 => "heading_4",
        5 => "heading_5",
        _ => "heading_6",
    }
}

fn is_code_fence(text: &str) -> bool {
    text.trim_start().starts_with("```")
}

fn markdown_list_marker(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return true;
    }
    trimmed.split_once('.').is_some_and(|(number, rest)| {
        !number.is_empty()
            && number.chars().all(|character| character.is_ascii_digit())
            && rest.starts_with(' ')
    })
}

fn markdown_table_row(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
}
