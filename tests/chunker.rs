use std::time::{Duration, Instant};

use sourcelens::index::chunker::{chunk_file, Chunk, Symbol};
use sourcelens::index::languages::{language_by_name, language_for_path};

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

fn symbol<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
    symbols
        .iter()
        .find(|symbol| symbol.name == name)
        .unwrap_or_else(|| panic!("no symbol {name}: {symbols:?}"))
}

fn line_of(content: &str, needle: &str) -> u32 {
    content
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle}")) as u32
        + 1
}

fn ranges(chunks: &[Chunk]) -> Vec<(&'static str, u32, u32)> {
    chunks
        .iter()
        .map(|chunk| (chunk.kind, chunk.start_line, chunk.end_line))
        .collect()
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

fn csharp_method(name: &str) -> String {
    format!(
        "        public Order {name}(Cart cart)\n        {{\n            var order = new Order(cart.Id, cart.Customer, cart.Currency);\n            foreach (var line in cart.Lines) {{ order.Add(line.Sku, line.Quantity, line.UnitPrice); }}\n            return order;\n        }}\n\n"
    )
}

#[test]
fn csharp_block_namespace_is_not_indexed_twice() {
    let mut content = String::from(
        "using System;\n\nnamespace Shop.Billing\n{\n    public class OrderService\n    {\n",
    );
    for name in [
        "ProcessOrder",
        "CancelOrder",
        "RefundOrder",
        "ShipOrder",
        "MergeOrder",
        "SplitOrder",
        "ArchiveOrder",
        "RestoreOrder",
    ] {
        content.push_str(&csharp_method(name));
    }
    content.push_str("    }\n}\n");
    let (chunks, symbols) = chunk("src/OrderService.cs", &content);
    assert_disjoint(&chunks);

    let process = symbol(&symbols, "ProcessOrder");
    assert_eq!(
        process.qualified_name,
        "Shop.Billing.OrderService.ProcessOrder"
    );
    assert_eq!(
        covering(&chunks, process.start_line, process.end_line).len(),
        1
    );
    assert_eq!(
        symbol(&symbols, "OrderService").qualified_name,
        "Shop.Billing.OrderService"
    );
    // The namespace contributes its header line only; no chunk spans the whole namespace.
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.start_line > 3 || chunk.end_line < 5),
        "{:?}",
        ranges(&chunks)
    );

    let small = "namespace Shop\n{\n    public class Cart\n    {\n        public int Count() { return 0; }\n    }\n}\n";
    let (chunks, symbols) = chunk("src/Cart.cs", small);
    assert_disjoint(&chunks);
    assert_eq!(symbol(&symbols, "Count").qualified_name, "Shop.Cart.Count");
}

#[test]
fn php_braced_namespace_is_not_indexed_twice() {
    let content = "<?php\nnamespace App\\Http {\n    class Kernel {\n        public function handle($request) {\n            return $this->router->dispatch($request);\n        }\n    }\n}\n";
    let (chunks, symbols) = chunk("app/Http/Kernel.php", content);
    assert_disjoint(&chunks);
    assert_eq!(symbol(&symbols, "Kernel").qualified_name, "App.Http.Kernel");
    assert_eq!(symbol(&symbols, "Kernel").depth, 0);
    assert_eq!(
        symbol(&symbols, "handle").qualified_name,
        "App.Http.Kernel.handle"
    );
    let kernel = symbol(&symbols, "Kernel");
    assert_eq!(
        covering(&chunks, kernel.start_line, kernel.end_line).len(),
        1
    );
}

#[test]
fn cpp_out_of_line_methods_and_root_templates_are_symbols() {
    let content = "#include \"widget.h\"\n\nvoid Widget::draw() {\n    canvas_.clear();\n}\n\nint* ui::Widget::resize(int w) {\n    width_ = w;\n    return &width_;\n}\n\ntemplate <typename T>\nT clamp(T v, T lo, T hi) {\n    return v < lo ? lo : (v > hi ? hi : v);\n}\n\ntemplate <class T>\nclass Box {\n    T value;\n};\n";
    let (chunks, symbols) = chunk("src/widget.cpp", content);
    assert_disjoint(&chunks);
    assert!(
        symbols.iter().all(|symbol| symbol.name != "Widget"),
        "{symbols:?}"
    );

    let draw = symbol(&symbols, "draw");
    assert_eq!(draw.kind, "function");
    assert_eq!(draw.qualified_name, "Widget.draw");
    assert_eq!(draw.depth, 0);
    assert_eq!(
        symbol(&symbols, "resize").qualified_name,
        "ui.Widget.resize"
    );

    let clamp = symbol(&symbols, "clamp");
    assert_eq!(clamp.kind, "function");
    assert_eq!(clamp.start_line, line_of(content, "template <typename T>"));
    let boxed = symbol(&symbols, "Box");
    assert_eq!(boxed.kind, "class");
    assert_eq!(boxed.start_line, line_of(content, "template <class T>"));
}

#[test]
fn constants_are_symbols_without_changing_chunks() {
    let body = (1..=12)
        .map(|step| {
            format!("    total += attempt_step({step}, MAX_RETRIES, GREETING.len() as u32);\n")
        })
        .collect::<String>();
    let rust = format!("use std::fmt;\n\npub const MAX_RETRIES: u32 = 5;\nstatic GREETING: &str = \"hi\";\n\npub fn retry() -> u32 {{\n    let mut total = 0;\n{body}    total\n}}\n");
    let (chunks, symbols) = chunk("src/retry.rs", &rust);
    let function_start = line_of(&rust, "pub fn retry");
    assert_eq!(
        ranges(&chunks),
        [
            ("imports", 1, 1),
            ("block", 3, 4),
            ("function", function_start, function_start + 15)
        ]
    );
    assert_eq!(chunks[1].names, ["MAX_RETRIES", "GREETING"]);
    let max = symbol(&symbols, "MAX_RETRIES");
    assert_eq!((max.kind, max.start_line, max.end_line), ("constant", 3, 3));
    assert_eq!(
        max.signature.as_deref(),
        Some("pub const MAX_RETRIES: u32 = 5;")
    );
    assert_eq!(symbol(&symbols, "GREETING").kind, "constant");

    let go = include_str!("fixtures/consts.go");
    let (chunks, symbols) = chunk("cmd/consts.go", go);
    let func = line_of(go, "func uploadAll");
    assert_eq!(
        ranges(&chunks),
        [
            ("package", 1, 1),
            ("block", 3, 12),
            ("function", func, func + 39)
        ]
    );
    let constants: Vec<&Symbol> = symbols
        .iter()
        .filter(|symbol| symbol.kind == "constant")
        .collect();
    assert_eq!(constants.len(), 10);
    assert_eq!(
        symbol(&symbols, "DefaultRequestTimeoutSeconds").qualified_name,
        "fixture.DefaultRequestTimeoutSeconds"
    );

    let ts = "export const DEFAULT_TIMEOUT_MS = 30_000;\nexport const routes = {\n  home: \"/\",\n  orders: \"/orders\",\n};\n\nexport function route(name: string): string {\n  return routes[name];\n}\n\nclass Client {\n  retries = 3;\n  send() {\n    return DEFAULT_TIMEOUT_MS;\n  }\n}\n";
    let (chunks, symbols) = chunk("src/routes.ts", ts);
    assert_disjoint(&chunks);
    assert!(chunks
        .iter()
        .all(|chunk| !matches!(chunk.kind, "constant" | "property")));
    assert_eq!(symbol(&symbols, "DEFAULT_TIMEOUT_MS").kind, "constant");
    let routes = symbol(&symbols, "routes");
    assert_eq!(
        (routes.kind, routes.start_line, routes.end_line),
        ("constant", 2, 5)
    );
    let retries = symbol(&symbols, "retries");
    assert_eq!(
        (retries.kind, retries.qualified_name.as_str()),
        ("property", "Client.retries")
    );

    let python = "MAX_ATTEMPTS = 5\ntimeout: float = 2.5\n\n\nclass Worker:\n    queue = \"default\"\n\n    def run(self):\n        return MAX_ATTEMPTS\n";
    let (chunks, symbols) = chunk("app/worker.py", python);
    assert_disjoint(&chunks);
    assert!(chunks
        .iter()
        .all(|chunk| !matches!(chunk.kind, "constant" | "property")));
    assert_eq!(symbol(&symbols, "MAX_ATTEMPTS").kind, "constant");
    assert_eq!(symbol(&symbols, "timeout").kind, "constant");
    let queue = symbol(&symbols, "queue");
    assert_eq!(
        (queue.kind, queue.qualified_name.as_str()),
        ("property", "Worker.queue")
    );
}

/// Chunk ranges of `content` and of the same text with every `const`/`constexpr` keyword blanked
/// out (same lines and columns), so the constant symbols cannot have moved a chunk boundary.
fn assert_same_chunks_without_const(path: &str, content: &str) {
    let plain = content
        .replace("constexpr", "         ")
        .replace("const ", "      ");
    let (with_const, _) = chunk(path, content);
    let (without_const, plain_symbols) = chunk(path, &plain);
    assert_eq!(ranges(&with_const), ranges(&without_const));
    // Without the keyword only the macros are constants, so the comparison is meaningful.
    let is_macro = |symbol: &Symbol| {
        symbol
            .signature
            .as_deref()
            .is_some_and(|signature| signature.starts_with("#define"))
    };
    assert!(plain_symbols
        .iter()
        .all(|symbol| symbol.kind != "constant" || is_macro(symbol)));
}

#[test]
fn c_cpp_and_php_constants_are_symbols_without_changing_chunks() {
    let constants = |symbols: &[Symbol]| {
        let mut names: Vec<String> = symbols
            .iter()
            .filter(|symbol| symbol.kind == "constant")
            .map(|symbol| symbol.name.clone())
            .collect();
        names.sort();
        names
    };

    let c = "#ifndef GEOMETRY_H\n#define GEOMETRY_H\n#include <stdint.h>\n\n#define MAX_SHAPES 128\n#define AREA(w, h) ((w) * (h))\nstatic const int DEFAULT_WIDTH = 640;\nconst char *const UNIT_NAME = \"px\";\nextern const int EXTERNAL_LIMIT;\nint counter = 0;\n\nuint32_t size_area(uint32_t w, uint32_t h) {\n    return AREA(w, h);\n}\n#endif\n";
    let (chunks, symbols) = chunk("geo/geometry.c", c);
    assert_disjoint(&chunks);
    assert!(chunks.iter().all(|chunk| chunk.kind != "constant"));
    // Include guards, function-like macros, declarations without a value and variables are not.
    assert_eq!(constants(&symbols), ["DEFAULT_WIDTH", "MAX_SHAPES", "UNIT_NAME"]);
    let width = symbol(&symbols, "DEFAULT_WIDTH");
    assert_eq!(width.start_line, line_of(c, "DEFAULT_WIDTH"));
    assert_eq!(
        width.signature.as_deref(),
        Some("static const int DEFAULT_WIDTH = 640;")
    );
    assert_eq!(
        symbol(&symbols, "MAX_SHAPES").signature.as_deref(),
        Some("#define MAX_SHAPES 128")
    );
    assert_same_chunks_without_const("geo/geometry.c", c);

    let cpp = "namespace ui {\nconstexpr int kMaxWidgets = 64;\nconst double kScale = 1.5;\n#define UI_VERSION \"2.1\"\nint instances = 0;\nvoid draw();\n}\n\nstatic const int kDefaultHeight = 480;\n";
    let (chunks, symbols) = chunk("ui/widgets.cpp", cpp);
    assert_disjoint(&chunks);
    assert!(chunks.iter().all(|chunk| chunk.kind != "constant"));
    assert_eq!(
        constants(&symbols),
        ["UI_VERSION", "kDefaultHeight", "kMaxWidgets", "kScale"]
    );
    assert_eq!(
        symbol(&symbols, "kMaxWidgets").qualified_name,
        "ui.kMaxWidgets"
    );
    assert_same_chunks_without_const("ui/widgets.cpp", cpp);

    let php = "<?php\nnamespace Acme\\Invoices;\n\nconst DEFAULT_LOCALE = 'de';\n\nclass InvoiceRenderer\n{\n    const DEFAULT_CURRENCY = 'EUR';\n    public const ROUNDING = 2, PRECISION = 4;\n\n    public function render(array $invoice): string\n    {\n        return implode(\"\\n\", $invoice);\n    }\n}\n";
    let (chunks, symbols) = chunk("src/Invoice.php", php);
    assert_disjoint(&chunks);
    assert!(chunks.iter().all(|chunk| chunk.kind != "constant"));
    assert_eq!(
        constants(&symbols),
        ["DEFAULT_CURRENCY", "DEFAULT_LOCALE", "PRECISION", "ROUNDING"]
    );
    let currency = symbol(&symbols, "DEFAULT_CURRENCY");
    assert_eq!(
        (currency.qualified_name.as_str(), currency.start_line),
        (
            "Acme.Invoices.InvoiceRenderer.DEFAULT_CURRENCY",
            line_of(php, "DEFAULT_CURRENCY")
        )
    );
    assert_eq!(
        symbol(&symbols, "DEFAULT_LOCALE").qualified_name,
        "Acme.Invoices.DEFAULT_LOCALE"
    );
    assert!(symbols.iter().any(|s| s.name == "render" && s.kind == "method"));
}

#[test]
fn small_sibling_does_not_swallow_a_large_declaration() {
    let fixture = include_str!("fixtures/consts.go");
    let lines: Vec<&str> = fixture.lines().collect();
    // package line, blank line, the first five constants, blank line, then `uploadAll`.
    let content = [&lines[..7], &lines[12..]].concat().join("\n") + "\n";
    let (chunks, symbols) = chunk("cmd/fixture.go", &content);
    assert_disjoint(&chunks);

    let func = symbol(&symbols, "uploadAll");
    assert!(func
        .signature
        .as_deref()
        .is_some_and(|signature| signature.starts_with("func uploadAll(")));
    let chunk = chunks
        .iter()
        .find(|chunk| chunk.start_line == func.start_line && chunk.end_line == func.end_line)
        .unwrap_or_else(|| panic!("func is not its own chunk: {:?}", ranges(&chunks)));
    assert_eq!(chunk.kind, "function");
    assert_eq!(chunk.symbol_path, ["uploadAll"]);
    assert_eq!(chunk.names, ["uploadAll"]);
}

#[test]
fn c_typedef_struct_is_one_chunk_named_after_the_typedef() {
    let fields = (1..=30)
        .map(|index| format!("    unsigned int field_number_{index};\n"))
        .collect::<String>();
    let content = format!("/** Size of a widget. */\ntypedef struct {{\n    int width;\n    int height;\n{fields}}} Size;\n\ntypedef enum {{ RED, GREEN }} Color;\n");
    let (chunks, symbols) = chunk("src/size.c", &content);
    assert_disjoint(&chunks);
    let names: Vec<(&str, &str)> = symbols
        .iter()
        .map(|symbol| (symbol.kind, symbol.qualified_name.as_str()))
        .collect();
    assert_eq!(names, [("struct", "Size"), ("enum", "Color")]);
    let size = symbol(&symbols, "Size");
    assert_eq!(size.start_line, 1);
    assert_eq!(size.signature.as_deref(), Some("typedef struct"));
    assert_eq!(covering(&chunks, 1, size.end_line).len(), 1);
}

#[test]
fn markdown_prose_outside_sections_is_indexed() {
    let setup = "Set SETUP_TOKEN from the server log before you start.\n\n- step one\n- step two\n";
    let (chunks, _) = chunk("docs/setup.md", setup);
    assert_disjoint(&chunks);
    assert_eq!(covering(&chunks, 1, 1).len(), 1, "{:?}", ranges(&chunks));

    let setext = "Title\n=====\n\nIntro text for the project.\n\nUsage\n-----\n\nRun the binary.\n";
    let (chunks, symbols) = chunk("README.md", setext);
    assert_disjoint(&chunks);
    let headings: Vec<(&str, &str)> = symbols
        .iter()
        .map(|symbol| (symbol.kind, symbol.name.as_str()))
        .collect();
    assert_eq!(headings, [("heading_1", "Title"), ("heading_2", "Usage")]);
    assert_eq!(symbol(&symbols, "Usage").qualified_name, "Title.Usage");
    for (index, line) in setext.lines().enumerate() {
        if !line.trim().is_empty() {
            let line = index as u32 + 1;
            assert_eq!(covering(&chunks, line, line).len(), 1, "line {line}");
        }
    }

    let prose = "Installation needs a recent toolchain and a database. ".repeat(50);
    let install = format!("## Install\n\n{prose}\n\n```bash\ncargo install sourcelens\n```\n");
    let (chunks, _) = chunk("docs/install.md", &install);
    let fence = chunks
        .iter()
        .find(|chunk| chunk.kind == "code_fence")
        .unwrap_or_else(|| panic!("no fence chunk: {:?}", ranges(&chunks)));
    assert_eq!(fence.symbol_path, ["Install"]);
}

#[test]
fn typescript_arrow_function_signature_and_statement_split() {
    let statements = (1..=30)
        .map(|step| format!("  const result{step} = await ctx.run(\n    \"step-{step}\", input.id, {{ retries: 3, timeoutMs: 1000 }},\n  );\n"))
        .collect::<String>();
    let content = format!("export const createOrder = async (\n  input: OrderInput,\n  ctx: Context,\n): Promise<Order> => {{\n{statements}  return ctx.finish(input);\n}};\n");
    let (chunks, symbols) = chunk("src/orders.ts", &content);
    assert_disjoint(&chunks);

    let create = symbol(&symbols, "createOrder");
    assert_eq!(create.kind, "function");
    assert_eq!(
        create.signature.as_deref(),
        Some("const createOrder = async ( input: OrderInput, ctx: Context, ): Promise<Order> =>")
    );
    let statement_starts: Vec<u32> = content
        .lines()
        .enumerate()
        .filter(|(_, line)| line.starts_with("  const result") || line.starts_with("  return"))
        .map(|(index, _)| index as u32 + 1)
        .collect();
    assert!(chunks.len() > 1, "{:?}", ranges(&chunks));
    assert_eq!(chunks[0].kind, "function");
    for part in &chunks[1..] {
        assert!(
            statement_starts.contains(&part.start_line),
            "part starts mid-statement: {:?}",
            ranges(&chunks)
        );
    }
}

#[test]
fn rust_mod_declaration_is_not_a_symbol() {
    let content = "mod config;\npub mod util;\n\nmod inline {\n    pub fn helper() {}\n}\n";
    let (_, symbols) = chunk("src/lib.rs", content);
    let modules: Vec<&str> = symbols
        .iter()
        .filter(|symbol| symbol.kind == "module")
        .map(|symbol| symbol.name.as_str())
        .collect();
    assert_eq!(modules, ["inline"]);
}

#[test]
fn go_method_receiver_is_part_of_the_qualified_name() {
    let body = "\tvalue, ok := store.items[key]\n\tif !ok {\n\t\treturn \"\", fmt.Errorf(\"missing key %q in store %s\", key, store.name)\n\t}\n\tstore.hits++\n\tstore.lastAccess = time.Now()\n\tstore.log.Printf(\"read %s from %s after %d hits\", key, store.name, store.hits)\n\tstore.metrics.Observe(\"store_read\", store.name, time.Since(store.lastAccess))\n\treturn value, nil\n";
    let content = format!("package store\n\ntype Memory struct{{}}\n\nfunc (store *Memory) Get(key string) (string, error) {{\n{body}}}\n\ntype Disk struct{{}}\n\nfunc (store Disk) Get(key string) (string, error) {{\n{body}}}\n");
    let (chunks, symbols) = chunk("store/store.go", &content);
    let methods: Vec<(&str, u32)> = symbols
        .iter()
        .filter(|symbol| symbol.kind == "method")
        .map(|symbol| (symbol.qualified_name.as_str(), symbol.depth))
        .collect();
    assert_eq!(methods, [("store.Memory.Get", 0), ("store.Disk.Get", 0)]);
    let disk_get = line_of(&content, "func (store Disk)");
    let chunk = chunks
        .iter()
        .find(|chunk| chunk.start_line == disk_get)
        .unwrap_or_else(|| panic!("no method chunk: {:?}", ranges(&chunks)));
    assert_eq!(chunk.kind, "method");
    assert_eq!(chunk.symbol_path, ["Disk", "Get"]);
}

#[test]
fn language_names_match_extensions() {
    for (name, expected) in [
        ("ts", "typescript"),
        ("py", "python"),
        ("cs", "csharp"),
        ("kt", "kotlin"),
        ("golang", "go"),
        ("TypeScript", "typescript"),
        ("c++", "cpp"),
    ] {
        assert_eq!(
            language_by_name(name).map(|language| language.name),
            Some(expected),
            "{name}"
        );
    }
    assert!(language_by_name("klingon").is_none());
}

#[test]
fn cpp_header_with_h_extension_gets_symbols() {
    let content = "#pragma once\n#include <string>\n\nnamespace ui {\n\nclass Widget {\npublic:\n    explicit Widget(int w);\n    void draw() const;\n    int width() const { return width_; }\nprivate:\n    int width_;\n};\n\ntemplate <typename T>\nT clamp(T v, T lo, T hi) {\n    return v < lo ? lo : (v > hi ? hi : v);\n}\n\n}  // namespace ui\n";
    assert_eq!(
        language_for_path("include/widget.h").map(|language| language.name),
        Some("c")
    );
    let (chunks, symbols) = chunk("include/widget.h", content);
    assert_disjoint(&chunks);
    assert!(
        chunks.iter().all(|chunk| chunk.kind != "fallback"),
        "{:?}",
        ranges(&chunks)
    );
    assert_eq!(symbol(&symbols, "Widget").qualified_name, "ui.Widget");
    assert_eq!(symbol(&symbols, "clamp").qualified_name, "ui.clamp");

    let c_header = "#ifndef SIZE_H\n#define SIZE_H\ntypedef struct {\n    int width;\n} Size;\nint area(const Size *size);\n#endif\n";
    let (_, symbols) = chunk("include/size.h", c_header);
    assert_eq!(symbol(&symbols, "Size").kind, "struct");
}

#[test]
fn large_markdown_file_is_chunked_quickly() {
    let content = (1..=20_000)
        .map(|section| format!("## Section {section}\n\nNotes for section {section}.\n\n"))
        .collect::<String>();
    let started = Instant::now();
    let (chunks, symbols) = chunk("CHANGELOG.md", &content);
    // Linear work takes milliseconds; the quadratic version took over ten seconds in debug builds.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(symbols.len(), 20_000);
    assert!(!chunks.is_empty());
}
