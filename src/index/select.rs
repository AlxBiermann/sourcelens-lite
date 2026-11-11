//! Which repository files are worth indexing.

use crate::model::SkipReason;

const VENDORED_PATH_SEGMENTS: &[&str] = &[
    "node_modules",
    "bower_components",
    "vendor",
    "third_party",
    "__pycache__",
    ".venv",
    "venv",
];

const LOCKFILE_NAMES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "flake.lock",
    "packages.lock.json",
    "mix.lock",
    "pubspec.lock",
];

const MINIFIED_FILE_SUFFIXES: &[&str] = &[".min.js", ".min.css", ".map", ".bundle.js"];

const GENERATED_FILE_SUFFIXES: &[&str] = &[
    ".pb.go",
    "_pb2.py",
    "_pb2_grpc.py",
    ".pb.cc",
    ".pb.h",
    ".g.cs",
    ".g.dart",
    ".freezed.dart",
];

/// Bytes inspected for NUL bytes when detecting binary files.
const BINARY_SNIFF_BYTES: usize = 8192;
/// Files at least this large are checked for minified content by average line length.
const MINIFIED_MIN_FILE_BYTES: usize = 1024;
/// Average bytes per line above which a file is treated as minified/bundled output.
const MINIFIED_MIN_AVERAGE_LINE_BYTES: usize = 1000;
/// Generated-file markers are only looked for in this many leading lines.
const GENERATED_MARKER_LINES: usize = 10;

/// `false` for vendored directories, lockfiles, minified or generated file names and anything under `.git`.
pub fn path_allowed(path: &str) -> bool {
    if path
        .split('/')
        .any(|segment| segment == ".git" || VENDORED_PATH_SEGMENTS.contains(&segment))
    {
        return false;
    }
    let file_name = path.rsplit('/').next().unwrap_or(path);
    if LOCKFILE_NAMES.contains(&file_name) {
        return false;
    }
    !MINIFIED_FILE_SUFFIXES
        .iter()
        .chain(GENERATED_FILE_SUFFIXES)
        .any(|suffix| file_name.ends_with(suffix))
}

/// Why the content should not be indexed: binary, minified or generated.
pub fn content_skip_reason(bytes: &[u8]) -> Option<SkipReason> {
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    if sniff.contains(&0) {
        return Some(SkipReason::Binary);
    }
    if bytes.len() >= MINIFIED_MIN_FILE_BYTES {
        let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
        let line_count = newlines + usize::from(bytes.last() != Some(&b'\n'));
        if bytes.len() / line_count.max(1) > MINIFIED_MIN_AVERAGE_LINE_BYTES {
            return Some(SkipReason::Minified);
        }
    }
    let generated = bytes
        .split(|&b| b == b'\n')
        .take(GENERATED_MARKER_LINES)
        .any(|line| {
            contains(line, b"@generated")
                || (contains(line, b"Code generated") && contains(line, b"DO NOT EDIT"))
        });
    generated.then_some(SkipReason::Generated)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
