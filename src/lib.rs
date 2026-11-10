//! Visual Basic .NET parser plugin - full-parse lightweight mode.
//!
//! Handles `.vb` files. The plugin parses raw source directly so no
//! Python-hosted Tree-sitter/CST path is used for beta parser hosting.

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

struct VbNetParser;

const TRIVIA: &[&str] = &["comment", "whitespace"];

#[derive(Debug, Clone)]
struct RawNode {
    id: String,
    node_type: String,
    label: String,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
    children: Vec<RawNode>,
    parent_type: Option<String>,
}

impl RawNode {
    fn semantic(self) -> SemanticNode {
        let mut builder = SemanticNodeBuilder::new(
            self.id,
            self.node_type,
            self.label,
            self.start_line,
            self.start_col,
            self.end_line,
            self.end_col,
            "",
        )
        .children(self.children.into_iter().map(RawNode::semantic).collect());
        if let Some(parent_type) = self.parent_type {
            builder = builder.parent_type(parent_type);
        }
        builder.build()
    }
}

/// Byte offset where a `'` comment starts, honouring double-quoted string
/// literals (VB escapes a quote inside a string as `""`, which simply toggles
/// the in-string state twice). An apostrophe inside a string is content.
fn comment_start(line: &str) -> Option<usize> {
    let mut in_string = false;
    for (offset, ch) in line.char_indices() {
        match ch {
            '"' | '\u{201C}' | '\u{201D}' => in_string = !in_string,
            '\'' | '\u{2018}' | '\u{2019}' if !in_string => return Some(offset),
            _ => {}
        }
    }
    None
}

/// The code part of a physical line: everything before a real comment.
/// Always a prefix of `line`, so byte offsets into it are offsets into `line`.
fn strip_inline_comment(line: &str) -> &str {
    match comment_start(line) {
        Some(offset) => &line[..offset],
        None => line,
    }
}

/// UTF-8 byte column of the first non-whitespace character.
fn indent_col(line: &str) -> u32 {
    (line.len() - line.trim_start().len()) as u32
}

fn lower_trimmed(line: &str) -> String {
    strip_inline_comment(line).trim().to_ascii_lowercase()
}

fn is_class_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "namespace_statement"
            | "class_statement"
            | "module_statement"
            | "structure_statement"
            | "interface_statement"
            | "enum_statement"
    )
}

fn is_method_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "sub_statement" | "function_statement" | "property_statement"
    )
}

const MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "friend",
    "shared",
    "static",
    "overrides",
    "overridable",
    "notoverridable",
    "mustoverride",
    "overloads",
    "shadows",
    "notinheritable",
    "mustinherit",
    "partial",
    "async",
    "iterator",
    "readonly",
    "writeonly",
    "default",
    "widening",
    "narrowing",
];

/// A declaration header split into byte ranges of the original line.
/// Modifiers and the signature (parameters, `As` clause, `Of`, `Handles`,
/// `Implements`, ...) are semantic: they are kept so that e.g.
/// `Public` -> `Private` or `x As Integer` -> `x As Long` is a real change.
struct DeclHeader {
    /// Source modifiers in order, with their byte range.
    modifiers: Vec<(usize, usize)>,
    node_type: &'static str,
    terminator: &'static str,
    name: String,
    /// Byte range of the signature after the name (may be empty).
    signature: (usize, usize),
}

fn is_word_end(rest: &str, len: usize) -> bool {
    rest[len..]
        .chars()
        .next()
        .map(|ch| ch.is_whitespace() || ch == '(')
        .unwrap_or(true)
}

fn starts_with_word(rest: &str, word: &str) -> bool {
    rest.len() >= word.len()
        && rest.as_bytes()[..word.len()].eq_ignore_ascii_case(word.as_bytes())
        && is_word_end(rest, word.len())
}

fn parse_header(line: &str) -> Option<DeclHeader> {
    let code = strip_inline_comment(line);
    let code_end = code.trim_end().len();
    let mut pos = indent_col(code) as usize;
    let mut modifiers = Vec::new();
    loop {
        let rest = &code[pos..code_end];
        let Some(modifier) = MODIFIERS.iter().find(|m| starts_with_word(rest, m)) else {
            break;
        };
        modifiers.push((pos, pos + modifier.len()));
        pos += modifier.len();
        pos += code[pos..code_end].len() - code[pos..code_end].trim_start().len();
    }
    let rest = &code[pos..code_end];
    for (keyword, node_type, terminator) in [
        ("namespace", "namespace_statement", "end namespace"),
        ("module", "module_statement", "end module"),
        ("class", "class_statement", "end class"),
        ("structure", "structure_statement", "end structure"),
        ("interface", "interface_statement", "end interface"),
        ("enum", "enum_statement", "end enum"),
        ("sub", "sub_statement", "end sub"),
        ("function", "function_statement", "end function"),
        ("property", "property_statement", "end property"),
    ] {
        if !(starts_with_word(rest, keyword) && rest.len() > keyword.len()) {
            continue;
        }
        let after_kw = pos + keyword.len();
        let name_start = after_kw
            + (code[after_kw..code_end].len() - code[after_kw..code_end].trim_start().len());
        let name: String = code[name_start..code_end]
            .chars()
            .take_while(|ch| {
                ch.is_alphanumeric() || *ch == '_' || *ch == '.' || *ch == '[' || *ch == ']'
            })
            .collect();
        let name_end = name_start + name.len();
        return Some(DeclHeader {
            modifiers,
            node_type,
            terminator,
            name: if name.is_empty() {
                "(anonymous)".to_string()
            } else {
                name
            },
            signature: (name_end, code_end),
        });
    }
    None
}

/// Collapse whitespace runs outside string literals so layout-only edits
/// inside a signature stay style-only while every token is preserved.
fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut pending_space = false;
    for ch in text.trim().chars() {
        if ch == '"' {
            in_string = !in_string;
        }
        if !in_string && ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

/// Whether a declaration header opens a block terminated by `End <kind>`.
/// `MustOverride` members, interface members and auto-implemented
/// properties are single-line declarations with no body.
fn has_body(
    header: &DeclHeader,
    line: &str,
    lines: &[&str],
    next: usize,
    parent: Option<&str>,
) -> bool {
    let has_modifier = |word: &str| {
        header
            .modifiers
            .iter()
            .any(|&(s, e)| line[s..e].eq_ignore_ascii_case(word))
    };
    if is_method_like(header.node_type)
        && (has_modifier("mustoverride") || parent == Some("interface_statement"))
    {
        return false;
    }
    if header.node_type == "property_statement" {
        // A property block's first significant line is a Get or Set accessor.
        let next_code = lines[next.min(lines.len())..]
            .iter()
            .map(|l| strip_inline_comment(l).trim())
            .find(|l| !l.is_empty());
        let Some(next_code) = next_code else {
            return false;
        };
        let mut rest = next_code;
        while let Some(m) = MODIFIERS.iter().find(|m| starts_with_word(rest, m)) {
            rest = rest[m.len()..].trim_start();
        }
        return starts_with_word(rest, "get") || starts_with_word(rest, "set");
    }
    true
}

fn statement_kind(line: &str) -> Option<&'static str> {
    let lower = lower_trimmed(line);
    if lower.is_empty() || lower.starts_with('\'') || lower.starts_with("rem ") {
        return None;
    }
    if lower.starts_with("imports ") {
        return Some("imports_statement");
    }
    if lower.starts_with("option ") {
        return Some("option_statement");
    }
    if lower.starts_with("return ") {
        return Some("return_statement");
    }
    if lower.starts_with("throw ") {
        return Some("throw_statement");
    }
    if lower.contains('=') {
        return Some("assignment_statement");
    }
    if lower.contains('(') && lower.contains(')') {
        return Some("invocation_expression");
    }
    Some("expression_statement")
}

fn parse_nodes(
    lines: &[&str],
    index: &mut usize,
    terminator: Option<&str>,
    parent_class: Option<&str>,
    parent_kind: Option<&str>,
    id_prefix: &str,
) -> Vec<RawNode> {
    let mut nodes = Vec::new();
    while *index < lines.len() {
        let line = lines[*index];
        let lower = lower_trimmed(line);
        if let Some(end) = terminator {
            if lower.starts_with(end) {
                *index += 1;
                break;
            }
        }
        if lower.is_empty() || lower.starts_with('\'') || lower.starts_with("rem ") {
            *index += 1;
            continue;
        }

        let line_index = *index;
        if let Some(header) = parse_header(line) {
            *index += 1;
            let node_id = format!("{}.{}", id_prefix, line_index);
            let node_type = header.node_type;
            let label = header.name.clone();
            let line_no = line_index as u32;
            let mut children = Vec::new();
            if let (Some(&(first, _)), Some(&(_, last))) =
                (header.modifiers.first(), header.modifiers.last())
            {
                let text = header
                    .modifiers
                    .iter()
                    .map(|&(s, e)| &line[s..e])
                    .collect::<Vec<_>>()
                    .join(" ");
                children.push(RawNode {
                    id: format!("{}.m", node_id),
                    node_type: "modifier_list".to_string(),
                    label: text,
                    start_line: line_no,
                    start_col: first as u32,
                    end_line: line_no,
                    end_col: last as u32,
                    children: Vec::new(),
                    parent_type: None,
                });
            }
            let (sig_start, sig_end) = header.signature;
            let signature = collapse_ws(&line[sig_start..sig_end]);
            if !signature.is_empty() {
                let lead =
                    line[sig_start..sig_end].len() - line[sig_start..sig_end].trim_start().len();
                children.push(RawNode {
                    id: format!("{}.s", node_id),
                    node_type: "signature".to_string(),
                    label: signature,
                    start_line: line_no,
                    start_col: (sig_start + lead) as u32,
                    end_line: line_no,
                    end_col: sig_end as u32,
                    children: Vec::new(),
                    parent_type: None,
                });
            }
            let (end_line, end_col) = if has_body(&header, line, lines, *index, parent_kind) {
                let child_parent = if is_class_like(node_type) {
                    Some(label.as_str())
                } else {
                    parent_class
                };
                children.extend(parse_nodes(
                    lines,
                    index,
                    Some(header.terminator),
                    child_parent,
                    Some(node_type),
                    &node_id,
                ));
                // parse_nodes consumes the matching End <declaration> line before returning.
                // Use that real closing source line for the declaration extent rather than
                // stopping at the last body child and borrowing the opening line's length.
                let end_index = (*index)
                    .saturating_sub(1)
                    .min(lines.len().saturating_sub(1));
                (
                    end_index as u32,
                    lines.get(end_index).map(|value| value.len()).unwrap_or(0) as u32,
                )
            } else {
                (line_no, line.len() as u32)
            };
            let mut node = RawNode {
                id: node_id,
                node_type: node_type.to_string(),
                label,
                start_line: line_no,
                start_col: indent_col(line),
                end_line,
                end_col,
                children,
                parent_type: None,
            };
            if is_method_like(node_type) {
                node.parent_type = parent_class.map(str::to_string);
            }
            nodes.push(node);
            continue;
        }

        if let Some(node_type) = statement_kind(line) {
            nodes.push(RawNode {
                id: format!("{}.{}", id_prefix, line_index),
                node_type: node_type.to_string(),
                label: strip_inline_comment(line).trim().to_string(),
                start_line: line_index as u32,
                start_col: indent_col(line),
                end_line: line_index as u32,
                end_col: line.len() as u32,
                children: Vec::new(),
                parent_type: None,
            });
        }
        *index += 1;
    }
    nodes
}

fn process_impl(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut index = 0;
    let children = parse_nodes(&lines, &mut index, None, None, None, "0");
    let root = RawNode {
        id: "0".to_string(),
        node_type: "source_file".to_string(),
        label: "source_file".to_string(),
        start_line: 0,
        start_col: 0,
        end_line: lines.len().saturating_sub(1) as u32,
        end_col: lines.last().map(|line| line.len()).unwrap_or(0) as u32,
        children,
        parent_type: None,
    };
    match serde_json::to_string(&root.semantic()) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for VbNetParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }

    fn grammar_id() -> String {
        "vbnet".to_string()
    }

    fn detect_language(filename: String, _content: String) -> String {
        if filename.to_lowercase().ends_with(".vb") {
            return "vbnet".to_string();
        }
        String::new()
    }

    fn preprocess_source(source: String) -> String {
        source
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "Module HelloWorld\n    Sub Greet(name As String)\n        Console.WriteLine(\"Hello, \" & name)\n    End Sub\n\n    Function Add(a As Integer, b As Integer) As Integer\n        Return a + b\n    End Function\nEnd Module\n".to_string(),
            new: "Module HelloWorld\n    Sub Greet(name As String)\n        Console.WriteLine($\"Hello, {name}!\")\n    End Sub\n\n    Function Add(a As Integer, b As Integer) As Integer\n        Return a + b\n    End Function\n\n    Function Multiply(a As Integer, b As Integer) As Integer\n        Return a * b\n    End Function\nEnd Module\n".to_string(),
        }
    }

    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }

    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }

    fn language_ids() -> Vec<String> {
        vec!["vbnet".to_string()]
    }

    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }

    fn priority() -> i32 {
        0
    }
}

export!(VbNetParser);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!VbNetParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = VbNetParser::grammar_id();
        let ids = VbNetParser::language_ids();
        assert!(ids.contains(&gid));
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            VbNetParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn detect_language_known_ext() {
        let r = VbNetParser::detect_language("test.vb".to_string(), "".to_string());
        assert_eq!(r.as_str(), "vbnet");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            VbNetParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_accepts_raw_example_source() {
        let example = VbNetParser::example(VbNetParser::grammar_id());
        let out = process_impl(&example.old);
        t::assert_valid_json(&out, "process(raw example)");
        assert!(out.contains("module_statement"));
        assert!(out.contains("sub_statement"));
        assert!(out.contains("function_statement"));
    }

    #[test]
    fn function_range_includes_end_function_line() {
        let source = "Module HelloWorld\n\
    Function Multiply(a As Integer, b As Integer) As Integer\n\
        Return a * b\n\
    End Function\n\
End Module\n";
        let out = process_impl(source);
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        let function = &value["children"][0]["children"][0];

        assert_eq!(function["node_type"], "function_statement");
        assert_eq!(function["position"]["start_line"], 1);
        assert_eq!(function["position"]["end_line"], 3);
        assert_eq!(function["position"]["end_col"], "End Function".len() as u64);
    }

    fn tree(source: &str) -> serde_json::Value {
        serde_json::from_str(&process_impl(source)).unwrap()
    }

    fn find<'a>(
        node: &'a serde_json::Value,
        node_type: &str,
        out: &mut Vec<&'a serde_json::Value>,
    ) {
        if node["node_type"] == node_type {
            out.push(node);
        }
        for child in node["children"].as_array().into_iter().flatten() {
            find(child, node_type, out);
        }
    }

    fn all<'a>(value: &'a serde_json::Value, node_type: &str) -> Vec<&'a serde_json::Value> {
        let mut out = Vec::new();
        find(value, node_type, &mut out);
        out
    }

    fn pos(node: &serde_json::Value) -> (u64, u64, u64, u64) {
        let p = &node["position"];
        (
            p["start_line"].as_u64().unwrap(),
            p["start_col"].as_u64().unwrap(),
            p["end_line"].as_u64().unwrap(),
            p["end_col"].as_u64().unwrap(),
        )
    }

    // Issue #6: an apostrophe inside a string literal is content, not a comment.
    #[test]
    fn apostrophe_inside_string_is_not_a_comment() {
        let old = "Public Module Probe\n    Public Function Message() As String\n        Return \"can't\"\n    End Function\nEnd Module\n";
        let new = "Public Module Probe\n    Public Function Message() As String\n        Return \"can't ever\"\n    End Function\nEnd Module\n";
        let (o, n) = (tree(old), tree(new));
        let ro = all(&o, "return_statement");
        let rn = all(&n, "return_statement");
        assert_eq!(ro[0]["label"], "Return \"can't\"");
        assert_eq!(rn[0]["label"], "Return \"can't ever\"");
        assert_eq!(pos(rn[0]), (2, 8, 2, 27));
        assert_ne!(o["structural_hash"], n["structural_hash"]);

        let o = tree("Dim x = \"don't safe\"\n");
        let n = tree("Dim x = \"don't danger\"\n");
        assert_eq!(o["children"][0]["label"], "Dim x = \"don't safe\"");
        assert_ne!(o["structural_hash"], n["structural_hash"]);
    }

    #[test]
    fn real_comments_are_still_trivia() {
        let a = tree("Dim x = \"a\"\"b\" ' note one\n");
        let b = tree("Dim x = \"a\"\"b\"   ' note two\n");
        assert_eq!(a["children"][0]["label"], "Dim x = \"a\"\"b\"");
        assert_eq!(a["structural_hash"], b["structural_hash"]);
        let c = tree("Public Sub F() ' it's a comment\nEnd Sub\n");
        let d = tree("Public Sub F() ' changed comment\nEnd Sub\n");
        assert_eq!(c["structural_hash"], d["structural_hash"]);
    }

    // Issue #6: access modifiers are part of the declaration's semantics.
    #[test]
    fn access_modifier_change_is_semantic() {
        let old = "Public Module Probe\n    Public Function Answer() As Integer\n        Return 1\n    End Function\nEnd Module\n";
        let new = "Public Module Probe\n    Private Function Answer() As Integer\n        Return 1\n    End Function\nEnd Module\n";
        let (o, n) = (tree(old), tree(new));
        let fo = all(&o, "function_statement")[0];
        let fnew = all(&n, "function_statement")[0];
        assert_eq!(fo["label"], "Answer");
        assert_eq!(fnew["label"], "Answer");
        let mo = all(fo, "modifier_list")[0];
        let mn = all(fnew, "modifier_list")[0];
        assert_eq!(mo["label"], "Public");
        assert_eq!(mn["label"], "Private");
        assert_eq!(pos(mn), (1, 4, 1, 11));
        assert_ne!(fo["structural_hash"], fnew["structural_hash"]);

        let o = tree("Public Sub F()\nEnd Sub\n");
        let n = tree("Private Sub F()\nEnd Sub\n");
        assert_ne!(o["structural_hash"], n["structural_hash"]);
        let m = tree("Protected Friend Overridable Sub F()\nEnd Sub\n");
        assert_eq!(
            all(&m, "modifier_list")[0]["label"],
            "Protected Friend Overridable"
        );
    }

    // Issue #6: parameter and return types are part of the signature.
    #[test]
    fn signature_change_is_semantic() {
        let o = tree("Public Function F(x As Integer) As Integer\nReturn x\nEnd Function\n");
        let n = tree("Public Function F(x As Long) As Integer\nReturn x\nEnd Function\n");
        let so = all(&o, "signature")[0];
        let sn = all(&n, "signature")[0];
        assert_eq!(so["label"], "(x As Integer) As Integer");
        assert_eq!(sn["label"], "(x As Long) As Integer");
        assert_eq!(pos(sn), (0, 17, 0, 39));
        assert_ne!(o["structural_hash"], n["structural_hash"]);
        // Whitespace-only edits inside the signature stay style-only.
        let w = tree("Public Function F( x  As Integer )   As Integer\nReturn x\nEnd Function\n");
        assert_eq!(
            all(&w, "signature")[0]["label"],
            "( x As Integer ) As Integer"
        );
        let r = tree("Public Function F(x As Integer) As Integer ' doc\nReturn x\nEnd Function\n");
        assert_eq!(r["structural_hash"], o["structural_hash"]);
    }

    #[test]
    fn identity_is_deterministic() {
        let src = "Public Module Probe\n    Public Function Answer() As Integer\n        Return 1\n    End Function\nEnd Module\n";
        assert_eq!(process_impl(src), process_impl(src));
    }

    #[test]
    fn bodyless_declarations_do_not_swallow_siblings() {
        let src = "Public Interface IThing\n    Sub Run()\n    Function Get() As Integer\nEnd Interface\n\
Public MustInherit Class Base\n    Public MustOverride Sub Go()\n    Public Property Name As String\n    Public Sub Done()\n    End Sub\nEnd Class\n";
        let t = tree(src);
        assert_eq!(t["children"].as_array().unwrap().len(), 2);
        let subs = all(&t, "sub_statement");
        assert_eq!(subs.len(), 3);
        assert_eq!(pos(subs[0]), (1, 4, 1, 13));
        let done = subs.iter().find(|s| s["label"] == "Done").unwrap();
        assert_eq!(done["parent_type"], "Base");
        assert_eq!(pos(done), (7, 4, 8, 11));
        let class = all(&t, "class_statement")[0];
        assert_eq!(pos(class), (4, 0, 9, 9));
    }

    #[test]
    fn property_block_with_accessors_keeps_body() {
        let src = "Class C\n    Public Property X As Integer\n        Get\n            Return 1\n        End Get\n    End Property\nEnd Class\n";
        let t = tree(src);
        let p = all(&t, "property_statement")[0];
        assert_eq!(pos(p), (1, 4, 5, 16));
        assert_eq!(all(p, "return_statement").len(), 1);
    }

    // Issue #7: columns are UTF-8 byte offsets, never inside a character.
    #[test]
    fn unicode_indentation_uses_byte_columns() {
        let src = "\u{2003}Dim x = 1";
        let t = tree(src);
        let node = &t["children"][0];
        assert_eq!(node["node_type"], "assignment_statement");
        assert_eq!(pos(node), (0, 3, 0, 12));
        assert!(src.is_char_boundary(3));

        let src = "\u{3000}\u{3000}Public Sub F()\n\u{3000}\u{3000}\u{3000}Return\n\u{3000}\u{3000}End Sub";
        let t = tree(src);
        let sub = all(&t, "sub_statement")[0];
        assert_eq!(pos(sub), (0, 6, 2, 13));
        assert_eq!(pos(all(sub, "modifier_list")[0]), (0, 6, 0, 12));
        assert_eq!(pos(&sub["children"][2]), (1, 9, 1, 15));
    }

    // CRLF convention: rows split on LF; columns exclude the trailing CR.
    #[test]
    fn crlf_columns_exclude_carriage_return() {
        let t = tree("Public Sub F()\r\n    Run()\r\nEnd Sub\r\n");
        let sub = all(&t, "sub_statement")[0];
        assert_eq!(pos(sub), (0, 0, 2, 7));
        assert_eq!(all(sub, "invocation_expression")[0]["label"], "Run()");
    }

    #[test]
    fn node_ids_are_unique() {
        let src = "Public Module M\n    Public Sub A(x As Integer)\n        Run(x)\n    End Sub\n    Private Sub B()\n    End Sub\nEnd Module\n";
        let t = tree(src);
        let mut ids = Vec::new();
        fn collect(n: &serde_json::Value, ids: &mut Vec<String>) {
            ids.push(n["id"].as_str().unwrap().to_string());
            for c in n["children"].as_array().into_iter().flatten() {
                collect(c, ids);
            }
        }
        collect(&t, &mut ids);
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}
