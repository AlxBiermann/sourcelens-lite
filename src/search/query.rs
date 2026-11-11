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

pub fn camel_parts(_identifier: &str) -> Vec<String> {
    todo!("store-search")
}

pub fn plan(_query: &str) -> Option<QueryPlan> {
    todo!("store-search")
}
