//! Language labels for repository files, selected by file name or extension.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Language {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub aliases: &'static [&'static str],
    pub file_names: &'static [&'static str],
}

const fn language(
    name: &'static str,
    extensions: &'static [&'static str],
    aliases: &'static [&'static str],
) -> Language {
    Language {
        name,
        extensions,
        aliases,
        file_names: &[],
    }
}

pub const LANGUAGES: &[Language] = &[
    language("java", &["java"], &[]),
    language("kotlin", &["kt", "kts"], &[]),
    language("groovy", &["groovy", "gradle"], &[]),
    language("typescript", &["ts", "tsx", "mts", "cts"], &[]),
    language("javascript", &["js", "jsx", "mjs", "cjs"], &[]),
    language("html", &["html"], &[]),
    language("css", &["css"], &[]),
    language("markdown", &["md", "markdown"], &[]),
    language("python", &["py", "pyi"], &[]),
    language("rust", &["rs"], &["rs"]),
    language("go", &["go"], &["golang"]),
    language("c", &["c", "h"], &[]),
    language("cpp", &["cc", "cpp", "cxx", "hh", "hpp", "hxx"], &["c++"]),
    language("csharp", &["cs"], &["c#", "dotnet"]),
    language("ruby", &["rb", "rake", "gemspec"], &["rb"]),
    language("php", &["php"], &[]),
    language("scala", &["scala", "sc"], &[]),
    language("swift", &["swift"], &[]),
    language("dart", &["dart"], &[]),
    language("shell", &["sh", "bash", "zsh"], &["bash", "sh"]),
    language("protobuf", &["proto"], &["proto"]),
    language("hcl", &["hcl", "tf", "tfvars"], &["terraform"]),
    Language {
        name: "dockerfile",
        extensions: &["dockerfile"],
        aliases: &["docker"],
        file_names: &["dockerfile", "containerfile"],
    },
    language("vue", &["vue"], &[]),
    language("svelte", &["svelte"], &[]),
    language("toml", &["toml"], &[]),
    language("yaml", &["yaml", "yml"], &[]),
    language("json", &["json"], &[]),
    language("sql", &["sql"], &[]),
];

pub fn language_for_path(path: &str) -> Option<&'static Language> {
    let path = Path::new(path);
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    if let Some(definition) = file_name.as_deref().and_then(|file_name| {
        LANGUAGES
            .iter()
            .find(|language| language.file_names.contains(&file_name))
    }) {
        return Some(definition);
    }
    let extension = normalized_extension(path)?;
    LANGUAGES
        .iter()
        .find(|language| language.extensions.contains(&extension.as_str()))
}

/// Language by name or alias, else by file extension (`ts`, `py`, `cs`).
pub fn language_by_name(name: &str) -> Option<&'static Language> {
    let normalized = name.trim().to_ascii_lowercase();
    let name = normalized.as_str();
    LANGUAGES
        .iter()
        .find(|language| language.name == name || language.aliases.contains(&name))
        .or_else(|| {
            LANGUAGES
                .iter()
                .find(|language| language.extensions.contains(&name))
        })
}

fn normalized_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
        .map(|extension| extension.to_ascii_lowercase())
}
