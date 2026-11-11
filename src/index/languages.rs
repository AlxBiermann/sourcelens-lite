pub struct Language {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub aliases: &'static [&'static str],
    pub file_names: &'static [&'static str],
}

pub fn language_for_path(_path: &str) -> Option<&'static Language> {
    todo!("chunker")
}

pub fn language_by_name(_name: &str) -> Option<&'static Language> {
    todo!("chunker")
}
