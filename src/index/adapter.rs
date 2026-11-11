#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterChunk {
    pub kind: String,
    pub name: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// Start lines of the direct body members of this declaration (1-based), used as split points.
    pub child_boundaries: Vec<u32>,
    pub signature: Option<String>,
}

pub fn parse(_language: &str, _path: &str, _source: &str) -> Option<Vec<AdapterChunk>> {
    todo!("chunker")
}
