//! Markdown scanner: heading sections, code fences, lists and tables.

use super::chunker::{dedupe_chunks, line_range, raw, RawChunk, SourceFile};

pub(super) fn markdown_chunks(source: &SourceFile) -> Vec<RawChunk> {
    let mut chunks = Vec::new();
    let mut heading_stack: Vec<(usize, String, u32)> = Vec::new();
    let mut line = 1;
    while line <= source.line_count() {
        let text = source.line(line);
        if let Some((level, name)) = markdown_heading(text) {
            while heading_stack
                .last()
                .map(|(last_level, _, _)| *last_level >= level)
                .unwrap_or(false)
            {
                if let Some((last_level, last_name, start)) = heading_stack.pop() {
                    chunks.push(raw(
                        markdown_heading_kind(last_level),
                        last_name,
                        line_range(start, line - 1),
                    ));
                }
            }
            heading_stack.push((level, name, line));
            line += 1;
            continue;
        }
        if let Some(language) = markdown_code_fence_language(text) {
            let start = line;
            line += 1;
            while line <= source.line_count() && !source.line(line).trim_start().starts_with("```")
            {
                line += 1;
            }
            let end = if line <= source.line_count() {
                line
            } else {
                source.line_count()
            };
            chunks.push(raw("code_fence", language, line_range(start, end)));
            line += 1;
            continue;
        }
        if markdown_list_marker(text).is_some() {
            let start = line;
            while line <= source.line_count() {
                let current = source.line(line);
                if markdown_list_marker(current).is_none() && !current.trim().is_empty() {
                    break;
                }
                line += 1;
            }
            chunks.push(raw("list", "list".to_owned(), line_range(start, line - 1)));
            continue;
        }
        if markdown_table_row(text) {
            let start = line;
            while line <= source.line_count() && markdown_table_row(source.line(line)) {
                line += 1;
            }
            chunks.push(raw(
                "table",
                "table".to_owned(),
                line_range(start, line - 1),
            ));
            continue;
        }
        line += 1;
    }

    let end = source.line_count();
    while let Some((level, name, start)) = heading_stack.pop() {
        chunks.push(raw(
            markdown_heading_kind(level),
            name,
            line_range(start, end),
        ));
    }

    dedupe_chunks(chunks)
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

fn markdown_code_fence_language(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let info = trimmed.strip_prefix("```")?.trim();
    if info.is_empty() {
        return Some("plain".to_owned());
    }
    Some(
        info.split_whitespace()
            .next()
            .unwrap_or("plain")
            .trim_matches(|character: char| !is_identifier_character(character))
            .to_ascii_lowercase(),
    )
}

fn markdown_list_marker(text: &str) -> Option<()> {
    let trimmed = text.trim_start();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return Some(());
    }
    let (number, rest) = trimmed.split_once('.')?;
    (!number.is_empty()
        && number.chars().all(|character| character.is_ascii_digit())
        && rest.starts_with(' '))
    .then_some(())
}

fn markdown_table_row(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
}

fn is_identifier_character(character: char) -> bool {
    character == '_' || character == '$' || character.is_ascii_alphanumeric()
}
