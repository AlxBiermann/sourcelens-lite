use rusqlite::Connection;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub id: i64,
    pub repo_id: i64,
    pub file_id: i64,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub depth: u32,
}

pub fn lookup(
    _conn: &Connection,
    _repo_ids: &[i64],
    _candidate: &str,
    _kind: Option<&str>,
    _limit: usize,
) -> rusqlite::Result<Vec<SymbolRow>> {
    todo!("store-search")
}

pub fn containing_chunk(
    _conn: &Connection,
    _file_id: i64,
    _line: u32,
) -> rusqlite::Result<Option<i64>> {
    todo!("store-search")
}
