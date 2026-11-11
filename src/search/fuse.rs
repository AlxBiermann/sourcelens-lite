//! Weighted reciprocal rank fusion of the ranked channel lists.

use std::collections::HashMap;

const RRF_K: f64 = 60.0;

/// `(channel, weight, ranked chunk ids)` → `(chunk id, score, channels containing it)`, best first.
/// `score = Σ weight / (60 + rank)` with ranks starting at 1.
pub fn fuse(lists: &[(&'static str, f64, Vec<i64>)]) -> Vec<(i64, f64, Vec<&'static str>)> {
    let mut fused: HashMap<i64, (f64, Vec<&'static str>)> = HashMap::new();
    for (channel, weight, ids) in lists {
        for (index, &id) in ids.iter().enumerate() {
            let entry = fused.entry(id).or_insert_with(|| (0.0, Vec::new()));
            if entry.1.contains(channel) {
                continue;
            }
            entry.0 += weight / (RRF_K + (index + 1) as f64);
            entry.1.push(channel);
        }
    }
    let mut out: Vec<(i64, f64, Vec<&'static str>)> = fused
        .into_iter()
        .map(|(id, (score, channels))| (id, score, channels))
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}
