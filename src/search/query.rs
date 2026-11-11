//! Turns a free-text query into lexical groups, identifier candidates and highlight terms.

use crate::index::store::camel_parts;

const MAX_QUERY_CHARS: usize = 512;
const MAX_SYMBOL_CANDIDATES: usize = 4;

const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "of", "in", "on", "to", "for", "is", "are", "was", "be", "how", "what",
    "where", "which", "why", "when", "does", "do", "did", "can", "i", "we", "it", "this", "that",
    "and", "or", "with", "from", "at", "as", "by",
];

/// One query token: matched as a whole (prefix) or as all of its identifier parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub whole: String,
    pub parts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlan {
    pub groups: Vec<Group>,
    pub symbol_candidates: Vec<String>,
    pub terms: Vec<String>,
}

fn is_code_like(word: &str) -> bool {
    word.contains('_')
        || word.contains("::")
        || word.contains('.')
        || word.contains('/')
        || word.chars().any(char::is_numeric)
        || word.chars().skip(1).any(char::is_uppercase)
}

/// `None` when the query has no usable token.
pub fn plan(query: &str) -> Option<QueryPlan> {
    let query: String = query.trim().chars().take(MAX_QUERY_CHARS).collect();
    let query = query.trim();

    let mut candidates: Vec<(Group, bool)> = Vec::new();
    for word in query.split_whitespace() {
        let code_like = is_code_like(word);
        for piece in word.split(|c: char| !c.is_alphanumeric()) {
            if piece.chars().count() < 2 {
                continue;
            }
            let parts = camel_parts(piece);
            let parts = if parts.len() > 1 { parts } else { Vec::new() };
            candidates.push((
                Group {
                    whole: piece.to_lowercase(),
                    parts,
                },
                code_like,
            ));
        }
    }

    let is_stop = |(group, code_like): &(Group, bool)| {
        !code_like && STOP_WORDS.contains(&group.whole.as_str())
    };
    let drop_stop_words = candidates.iter().any(|candidate| !is_stop(candidate));
    let mut groups: Vec<Group> = Vec::new();
    for candidate in candidates {
        if drop_stop_words && is_stop(&candidate) {
            continue;
        }
        if groups.iter().any(|group| group.whole == candidate.0.whole) {
            continue;
        }
        groups.push(candidate.0);
    }
    if groups.is_empty() {
        return None;
    }

    let mut terms: Vec<String> = Vec::new();
    for group in &groups {
        for term in std::iter::once(&group.whole).chain(&group.parts) {
            if !terms.contains(term) {
                terms.push(term.clone());
            }
        }
    }

    let mut symbol_candidates: Vec<String> = Vec::new();
    let whole_query = (!query.contains(char::is_whitespace)).then_some(query);
    let code_words = query.split_whitespace().filter(|word| is_code_like(word));
    for candidate in whole_query.into_iter().chain(code_words) {
        let candidate = candidate.strip_suffix("()").unwrap_or(candidate);
        if candidate.is_empty() || symbol_candidates.iter().any(|c| c == candidate) {
            continue;
        }
        symbol_candidates.push(candidate.to_string());
        if symbol_candidates.len() == MAX_SYMBOL_CANDIDATES {
            break;
        }
    }

    Some(QueryPlan {
        groups,
        symbol_candidates,
        terms,
    })
}
