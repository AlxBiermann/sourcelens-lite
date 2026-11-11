pub const CHUNKER_VERSION: i64 = 1;
pub const MAX_CHUNK_CHARS: u32 = 2000;
pub const MIN_CHUNK_CHARS: u32 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub kind: &'static str,
    pub symbol_path: Vec<String>,
    pub names: Vec<String>,
    pub start_line: u32,
    pub end_line: u32,
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

pub fn chunk_file(
    _path: &str,
    _language: Option<&str>,
    _content: &str,
) -> (Vec<Chunk>, Vec<Symbol>) {
    todo!("chunker")
}
