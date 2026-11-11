use sourcelens::index::chunker::{chunk_file, Chunk, Symbol};
use sourcelens::index::languages::language_for_path;

fn chunk(path: &str, content: &str) -> (Vec<Chunk>, Vec<Symbol>) {
    let language = language_for_path(path).map(|language| language.name);
    let (chunks, symbols) = chunk_file(path, language, content);
    assert_byte_offsets(content, &chunks);
    (chunks, symbols)
}

/// Byte offsets select exactly the chunk's lines.
fn assert_byte_offsets(content: &str, chunks: &[Chunk]) {
    let lines: Vec<&str> = content.lines().collect();
    for chunk in chunks {
        let expected = lines[(chunk.start_line - 1) as usize..chunk.end_line as usize].join("\n");
        assert_eq!(&content[chunk.start_byte..chunk.end_byte], expected);
    }
}

fn assert_disjoint(chunks: &[Chunk]) {
    for (index, left) in chunks.iter().enumerate() {
        for right in &chunks[index + 1..] {
            assert!(
                left.end_line < right.start_line || right.end_line < left.start_line,
                "chunks overlap: {}-{} and {}-{}",
                left.start_line,
                left.end_line,
                right.start_line,
                right.end_line
            );
        }
    }
}

fn covering(chunks: &[Chunk], start: u32, end: u32) -> Vec<&Chunk> {
    chunks
        .iter()
        .filter(|chunk| chunk.start_line <= start && chunk.end_line >= end)
        .collect()
}

#[test]
fn small_impl_is_one_chunk_with_method_symbols() {
    let (chunks, symbols) = chunk("src/small_impl.rs", include_str!("fixtures/small_impl.rs"));
    assert_disjoint(&chunks);

    let implementation = symbols
        .iter()
        .find(|symbol| symbol.kind == "impl")
        .expect("impl symbol");
    assert_eq!(
        covering(&chunks, implementation.start_line, implementation.end_line).len(),
        1
    );

    let methods: Vec<&Symbol> = symbols
        .iter()
        .filter(|symbol| symbol.kind == "method")
        .collect();
    let mut names: Vec<&str> = methods.iter().map(|symbol| symbol.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["increment", "new", "value"]);
    for method in methods {
        assert_eq!(method.depth, 1, "{}", method.name);
        let signature = method.signature.as_deref().expect("method signature");
        assert!(signature.starts_with("fn "), "{signature}");
        assert!(!signature.contains('{'), "{signature}");
    }
}

#[test]
fn big_class_splits_into_header_and_methods() {
    let (chunks, symbols) = chunk("src/big_class.ts", include_str!("fixtures/big_class.ts"));
    assert_disjoint(&chunks);

    let class = symbols
        .iter()
        .find(|symbol| symbol.kind == "class" && symbol.name == "UserRepository")
        .expect("class symbol");
    let methods: Vec<&Symbol> = symbols
        .iter()
        .filter(|symbol| symbol.kind == "method")
        .collect();
    assert_eq!(methods.len(), 5);
    let first_method = methods
        .iter()
        .map(|symbol| symbol.start_line)
        .min()
        .expect("methods");

    assert!(
        chunks.iter().any(|chunk| chunk.kind == "class"
            && chunk.start_line == class.start_line
            && chunk.end_line < first_method),
        "no class header chunk"
    );
    for method in methods {
        assert_eq!(
            covering(&chunks, method.start_line, method.end_line).len(),
            1,
            "{}",
            method.name
        );
    }
}

#[test]
fn python_signature_stops_at_body() {
    let (_, symbols) = chunk("pkg/sig.py", include_str!("fixtures/sig.py"));
    let foo = symbols
        .iter()
        .find(|symbol| symbol.name == "foo")
        .expect("foo symbol");
    assert_eq!(foo.kind, "function");
    assert_eq!(foo.signature.as_deref(), Some("def foo(a, b) -> int:"));
}

#[test]
fn small_go_declarations_merge_and_func_stays_alone() {
    let (chunks, symbols) = chunk("cmd/consts.go", include_str!("fixtures/consts.go"));
    assert_disjoint(&chunks);
    assert!(chunks.len() < 4, "{} chunks", chunks.len());

    let func = symbols
        .iter()
        .find(|symbol| symbol.name == "uploadAll")
        .expect("func symbol");
    assert_eq!(func.kind, "function");
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk.start_line == func.start_line && chunk.end_line == func.end_line),
        "func is not its own chunk"
    );
}

#[test]
fn plain_text_uses_line_windows_without_symbols() {
    let (chunks, symbols) = chunk("docs/notes.txt", include_str!("fixtures/notes.txt"));
    assert!(!chunks.is_empty());
    assert!(chunks.iter().all(|chunk| chunk.kind == "fallback"));
    assert!(symbols.is_empty());
    assert_disjoint(&chunks);
}
