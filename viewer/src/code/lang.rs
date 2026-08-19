//! Tree-sitter parsing: what a file's definitions are, what it calls, and how to cut
//! it into pieces small enough to embed.
//!
//! Three languages get a real parse — Rust, Python, and TypeScript. Everything else,
//! and anything that fails to parse, falls back to fixed-size line windows, which is
//! the whole degradation story: a language we cannot read still answers `/code/similar`
//! and `/code/text`, just without symbols.

use tree_sitter::{Node, Parser, Tree};

use crate::db::{CodeChunk, CodeRef, CodeSymbol};

/// Lines per chunk where there is no parse to cut along.
pub const WINDOW_LINES: usize = 50;

/// A gap between definitions shorter than this is left alone: module preamble is
/// mostly imports, and embedding three lines of `use` statements teaches nothing.
const MIN_GAP_LINES: usize = 10;

/// A chunk longer than this is truncated before it is embedded. The encoder's own
/// window is far shorter than a long function; the head is the part that names it.
const MAX_SNIPPET_BYTES: usize = 8_000;

/// The languages we can parse. `Other` is not a failure — it is the window path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Other,
}

impl Lang {
    /// The language a path is written in, by extension alone. Nothing here reads the
    /// file: this decides whether reading it is worth it.
    pub fn of_path(path: &str) -> Self {
        match path.rsplit_once('.').map(|(_, ext)| ext) {
            Some("rs") => Self::Rust,
            Some("py" | "pyi") => Self::Python,
            // The TSX grammar is a superset of the TypeScript one and parses plain
            // JavaScript too, so one grammar covers the whole family.
            Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs") => Self::TypeScript,
            _ => Self::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Other => "other",
        }
    }

    fn grammar(self) -> Option<tree_sitter::Language> {
        match self {
            Self::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            Self::Python => Some(tree_sitter_python::LANGUAGE.into()),
            Self::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
            Self::Other => None,
        }
    }

    /// Node kinds that introduce a named thing worth recording as a definition.
    fn definition_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "function_item",
                "struct_item",
                "enum_item",
                "trait_item",
                "type_item",
                "const_item",
                "static_item",
                "macro_definition",
            ],
            Self::Python => &["function_definition", "class_definition"],
            Self::TypeScript => &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "class_declaration",
                "interface_declaration",
                "type_alias_declaration",
                "enum_declaration",
            ],
            Self::Other => &[],
        }
    }

    /// Definition kinds that carry a body big enough to be its own chunk. A `const`
    /// is a definition worth finding but not a paragraph worth embedding.
    fn chunkable_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["function_item", "struct_item", "enum_item", "trait_item"],
            Self::Python => &["function_definition", "class_definition"],
            Self::TypeScript => &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "class_declaration",
                "interface_declaration",
            ],
            Self::Other => &[],
        }
    }

    /// The short kind name stored against a definition.
    fn kind_label(self, node_kind: &str) -> &'static str {
        match node_kind {
            "function_item" | "function_declaration" | "generator_function_declaration"
            | "function_definition" => "function",
            "method_definition" => "method",
            "struct_item" => "struct",
            "enum_item" | "enum_declaration" => "enum",
            "trait_item" => "trait",
            "class_definition" | "class_declaration" => "class",
            "interface_declaration" => "interface",
            "type_item" | "type_alias_declaration" => "type",
            "const_item" => "const",
            "static_item" => "static",
            "macro_definition" => "macro",
            _ => "symbol",
        }
    }
}

/// Everything one file contributes.
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub chunks: Vec<CodeChunk>,
    pub symbols: Vec<CodeSymbol>,
    pub refs: Vec<CodeRef>,
    /// True when a grammar actually ran. False means these are window chunks.
    pub parsed: bool,
}

/// The source of everything this module produces, as stored against a row.
pub const SOURCE: &str = "treesitter";

/// Cut a file into chunks and pull its definitions and call edges out.
///
/// Never fails: an unknown language, a grammar that will not load, and a file that
/// does not parse all land on the same windowed fallback.
pub fn parse(lang: Lang, source: &str) -> Parsed {
    let Some(grammar) = lang.grammar() else {
        return windowed(source);
    };
    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return windowed(source);
    }
    let Some(tree) = parser.parse(source, None) else {
        return windowed(source);
    };

    let definitions = collect_definitions(lang, &tree, source);
    if definitions.is_empty() {
        // A file with no definitions is a script or a data blob in disguise.
        return windowed(source);
    }

    let symbols: Vec<CodeSymbol> = definitions
        .iter()
        .map(|def| CodeSymbol {
            name: def.name.clone(),
            kind: lang.kind_label(def.node_kind).to_owned(),
            start_line: def.start_line as i64 + 1,
            end_line: def.end_line as i64 + 1,
            source: SOURCE.to_owned(),
        })
        .collect();

    let refs = collect_refs(lang, &tree, source, &definitions);

    let lines: Vec<&str> = source.lines().collect();
    let mut chunks: Vec<CodeChunk> = Vec::new();
    let mut covered: Vec<(usize, usize)> = Vec::new();
    for def in &definitions {
        if !lang.chunkable_kinds().contains(&def.node_kind) || def.nested {
            continue;
        }
        covered.push((def.start_line, def.end_line));
        chunks.push(CodeChunk {
            symbol: Some(def.name.clone()),
            start_line: def.start_line as i64 + 1,
            end_line: def.end_line as i64 + 1,
            snippet: snippet(&lines, def.start_line, def.end_line),
            vector: None,
            model: String::new(),
        });
    }

    // The lines no definition claimed — module preamble, top-level statements — get
    // window chunks, so a question about a file's imports still has something to hit.
    covered.sort_unstable();
    for (from, to) in gaps(&covered, lines.len()) {
        for (start, end) in windows(from, to) {
            chunks.push(CodeChunk {
                symbol: None,
                start_line: start as i64 + 1,
                end_line: end as i64 + 1,
                snippet: snippet(&lines, start, end),
                vector: None,
                model: String::new(),
            });
        }
    }
    chunks.sort_by_key(|chunk| chunk.start_line);

    Parsed { chunks, symbols, refs, parsed: true }
}

/// One definition, flattened out of the tree.
#[derive(Debug, Clone)]
struct Definition {
    name: String,
    node_kind: &'static str,
    /// Zero-based, inclusive.
    start_line: usize,
    end_line: usize,
    /// True when another definition encloses this one. Nested definitions are real
    /// symbols but not their own chunks: the enclosing one already carries the text.
    nested: bool,
}

fn collect_definitions(lang: Lang, tree: &Tree, source: &str) -> Vec<Definition> {
    let kinds = lang.definition_kinds();
    let mut found = Vec::new();
    let mut cursor = tree.walk();
    let mut stack = vec![(tree.root_node(), false)];
    while let Some((node, inside_definition)) = stack.pop() {
        let is_definition = kinds.contains(&node.kind());
        if is_definition
            && let Some(name) = definition_name(node, source)
        {
            let node_kind = kinds
                .iter()
                .copied()
                .find(|kind| *kind == node.kind())
                .unwrap_or("symbol");
            found.push(Definition {
                name,
                node_kind,
                start_line: node.start_position().row,
                end_line: node.end_position().row,
                nested: inside_definition,
            });
        }
        let child_inside = inside_definition || is_definition;
        for child in node.children(&mut cursor) {
            stack.push((child, child_inside));
        }
    }
    found.sort_by_key(|def| (def.start_line, def.end_line));
    found
}

/// The identifier a definition node introduces.
fn definition_name(node: Node<'_>, source: &str) -> Option<String> {
    // Every grammar here calls it `name`, except TypeScript's `method_definition`,
    // whose key may be a computed property or a string literal.
    let name = node.child_by_field_name("name")?;
    let text = name.utf8_text(source.as_bytes()).ok()?.trim();
    let text = text.trim_matches(|c| c == '"' || c == '\'' || c == '`');
    (!text.is_empty()).then(|| text.to_owned())
}

/// Call edges and bare name uses, each attributed to the definition that encloses it.
///
/// The enclosing definition is found by interval containment over the definitions we
/// already have — the same range-to-symbol map the SCIP overlay needs, built once.
fn collect_refs(
    lang: Lang,
    tree: &Tree,
    source: &str,
    definitions: &[Definition],
) -> Vec<CodeRef> {
    let call_kinds: &[&str] = match lang {
        Lang::Rust => &["call_expression", "macro_invocation"],
        Lang::Python => &["call"],
        Lang::TypeScript => &["call_expression", "new_expression"],
        Lang::Other => return Vec::new(),
    };

    let mut refs: Vec<CodeRef> = Vec::new();
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if call_kinds.contains(&node.kind())
            && let Some(name) = callee_name(node, source)
        {
            let line = node.start_position().row;
            refs.push(CodeRef {
                name,
                caller: enclosing(definitions, line),
                line: line as i64 + 1,
                kind: "call".to_owned(),
                source: SOURCE.to_owned(),
            });
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    refs.sort_by_key(|r| (r.line, r.name.clone()));
    refs.dedup_by(|a, b| a.line == b.line && a.name == b.name && a.caller == b.caller);
    refs
}

/// The name being called, reduced to its last segment.
///
/// `self.retry()`, `crate::net::retry()`, and `retry()` all answer `retry`. Losing the
/// path is deliberate: this layer resolves names heuristically, and the SCIP overlay
/// is what replaces it when a real indexer has run.
fn callee_name(node: Node<'_>, source: &str) -> Option<String> {
    let target = node
        .child_by_field_name("function")
        .or_else(|| node.child_by_field_name("macro"))
        .or_else(|| node.child_by_field_name("constructor"))?;
    let text = target.utf8_text(source.as_bytes()).ok()?;
    let last = text
        .rsplit(&['.', ':'][..])
        .find(|part| !part.is_empty())?
        .trim();
    let clean: String = last
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    (!clean.is_empty()).then_some(clean)
}

/// The innermost definition whose range contains `line`.
fn enclosing(definitions: &[Definition], line: usize) -> Option<String> {
    definitions
        .iter()
        .filter(|def| def.start_line <= line && line <= def.end_line)
        // Innermost wins: the narrowest range that still contains the line.
        .min_by_key(|def| def.end_line - def.start_line)
        .map(|def| def.name.clone())
}

/// Fixed windows over a whole file, for everything with no grammar.
fn windowed(source: &str) -> Parsed {
    let lines: Vec<&str> = source.lines().collect();
    let chunks = windows(0, lines.len())
        .into_iter()
        .map(|(start, end)| CodeChunk {
            symbol: None,
            start_line: start as i64 + 1,
            end_line: end as i64 + 1,
            snippet: snippet(&lines, start, end),
            vector: None,
            model: String::new(),
        })
        .collect();
    Parsed { chunks, parsed: false, ..Default::default() }
}

/// `[from, to)` cut into windows, as zero-based inclusive line pairs.
fn windows(from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = from;
    while start < to {
        let end = (start + WINDOW_LINES).min(to);
        out.push((start, end - 1));
        start = end;
    }
    out
}

/// The runs of lines no interval in `covered` claims. `covered` must be sorted.
fn gaps(covered: &[(usize, usize)], total: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in covered {
        if *start > cursor && start - cursor >= MIN_GAP_LINES {
            out.push((cursor, *start));
        }
        cursor = cursor.max(end + 1);
    }
    if total > cursor && total - cursor >= MIN_GAP_LINES {
        out.push((cursor, total));
    }
    out
}

/// The text of a zero-based inclusive line range, capped.
fn snippet(lines: &[&str], start: usize, end: usize) -> String {
    let end = end.min(lines.len().saturating_sub(1));
    if start > end {
        return String::new();
    }
    let mut text = lines[start..=end].join("\n");
    if text.len() > MAX_SNIPPET_BYTES {
        let mut cut = MAX_SNIPPET_BYTES;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n…");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rust_file_chunks_one_function_at_a_time() {
        let source = "\
fn alpha() {
    println!(\"a\");
}

fn beta() {
    alpha();
}
";
        let parsed = parse(Lang::Rust, source);
        assert!(parsed.parsed);
        let names: Vec<_> =
            parsed.chunks.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(parsed.chunks[0].start_line, 1);
        assert_eq!(parsed.chunks[0].end_line, 3);
        assert!(parsed.chunks[1].snippet.contains("alpha();"));
    }

    #[test]
    fn a_call_is_attributed_to_the_function_it_sits_in() {
        let source = "\
fn helper() {}

fn caller() {
    helper();
}
";
        let parsed = parse(Lang::Rust, source);
        let call = parsed
            .refs
            .iter()
            .find(|r| r.name == "helper")
            .expect("the call is recorded");
        assert_eq!(call.caller.as_deref(), Some("caller"));
        assert_eq!(call.kind, "call");
    }

    #[test]
    fn a_qualified_call_is_recorded_under_its_last_segment() {
        let source = "\
fn go() {
    crate::net::retry(3);
    self.retry();
}
";
        let parsed = parse(Lang::Rust, source);
        let hits: Vec<_> = parsed.refs.iter().filter(|r| r.name == "retry").collect();
        assert_eq!(hits.len(), 2, "both spellings resolve to `retry`");
        assert!(hits.iter().all(|r| r.caller.as_deref() == Some("go")));
    }

    #[test]
    fn python_classes_and_methods_are_both_symbols() {
        let source = "\
class Widget:
    def draw(self):
        render(self)

def top():
    Widget().draw()
";
        let parsed = parse(Lang::Python, source);
        let kinds: Vec<_> = parsed
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str()))
            .collect();
        assert!(kinds.contains(&("Widget", "class")));
        assert!(kinds.contains(&("draw", "function")));
        assert!(kinds.contains(&("top", "function")));
        // The method is inside the class, so the class alone is the chunk.
        let chunked: Vec<_> = parsed.chunks.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(chunked, vec!["Widget", "top"]);
    }

    #[test]
    fn typescript_functions_and_interfaces_parse() {
        let source = "\
export interface Options { retries: number }

export function connect(options: Options) {
  return open(options);
}
";
        let parsed = parse(Lang::TypeScript, source);
        let names: Vec<_> = parsed.symbols.iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"Options".to_owned()));
        assert!(names.contains(&"connect".to_owned()));
        assert!(parsed.refs.iter().any(|r| r.name == "open"));
    }

    #[test]
    fn a_language_with_no_grammar_falls_back_to_line_windows() {
        let source: String = (0..120).map(|n| format!("line {n}\n")).collect();
        let parsed = parse(Lang::Other, &source);
        assert!(!parsed.parsed);
        assert!(parsed.symbols.is_empty());
        assert_eq!(parsed.chunks.len(), 3, "120 lines at 50 per window");
        assert_eq!(parsed.chunks[0].start_line, 1);
        assert_eq!(parsed.chunks[0].end_line, 50);
        assert_eq!(parsed.chunks[2].start_line, 101);
        assert_eq!(parsed.chunks[2].end_line, 120);
    }

    #[test]
    fn source_that_does_not_parse_still_yields_window_chunks() {
        // Not Rust at all, handed to the Rust grammar.
        let source: String = (0..60).map(|n| format!("{{{{ unbalanced {n}\n")).collect();
        let parsed = parse(Lang::Rust, &source);
        assert!(!parsed.chunks.is_empty());
        assert!(parsed.chunks.iter().all(|c| c.symbol.is_none()));
    }

    #[test]
    fn a_long_preamble_gets_its_own_window_chunk() {
        let mut source = String::new();
        for n in 0..20 {
            source.push_str(&format!("use crate::thing{n};\n"));
        }
        source.push_str("fn only() {}\n");
        let parsed = parse(Lang::Rust, &source);
        assert!(
            parsed.chunks.iter().any(|c| c.symbol.is_none() && c.start_line == 1),
            "the imports are covered by a window chunk"
        );
        assert!(parsed.chunks.iter().any(|c| c.symbol.as_deref() == Some("only")));
    }

    #[test]
    fn extensions_map_to_grammars() {
        assert_eq!(Lang::of_path("src/main.rs"), Lang::Rust);
        assert_eq!(Lang::of_path("app/views.py"), Lang::Python);
        assert_eq!(Lang::of_path("web/app.tsx"), Lang::TypeScript);
        assert_eq!(Lang::of_path("README.md"), Lang::Other);
        assert_eq!(Lang::of_path("Makefile"), Lang::Other);
    }
}
