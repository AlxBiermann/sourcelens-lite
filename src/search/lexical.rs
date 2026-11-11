use rusqlite::Connection;

use crate::search::Filter;

pub fn search(
    _conn: &Connection,
    _match_expr: &str,
    _filter: &Filter,
    _limit: usize,
) -> rusqlite::Result<Vec<i64>> {
    todo!("store-search")
}
