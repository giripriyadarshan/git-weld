use super::{Language, Location};
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{HashMap, HashSet},
    fmt::Write,
    ops::RangeInclusive,
};
use tree_sitter::{Node, Tree};

const TOP_LEVEL: &str = "(top level)";

// One level of an item's identity, such as `fn get` inside `impl Counter`.
// `occurrence` separates repeated names in the same body, such as a Python
// property getter and its setter.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct Segment {
    label: String,
    name: String,
    occurrence: usize,
}

// Containers (impl, trait, mod and extern blocks, and classes) leave their
// child items out of `fingerprint`; every child is compared on its own.
#[derive(Debug)]
pub(super) struct Item {
    pub(super) key: Vec<Segment>,
    pub(super) path: String,
    pub(super) location: Location,
    pub(super) fingerprint: String,
}

struct Body<'tree> {
    node: Node<'tree>,
    owner: Node<'tree>,
    trivia: Vec<Node<'tree>>,
    item: usize,
}

struct Declaration<'tree> {
    label: String,
    name: Name,
    body: Option<Node<'tree>>,
}

#[derive(Default)]
struct Name {
    key: String,
    display: String,
}

struct NameBuilder<'source> {
    language: Language,
    source: &'source str,
    name: Name,
}

impl NameBuilder<'_> {
    fn node(&mut self, node: Option<Node>) {
        if let Some(node) = node {
            write_node(
                &mut self.name.key,
                self.language,
                self.source,
                node,
                &HashSet::new(),
            );
            self.name
                .display
                .push_str(&collapse_whitespace(&text(self.source, node)));
        }
    }

    fn text(&mut self, text: &str) {
        push_field(&mut self.name.key, 'S', text);
        self.name.display.push_str(text);
    }
}

pub(super) fn extract(language: Language, source: &str, tree: &Tree) -> Vec<Item> {
    let root = tree.root_node();
    let mut items = vec![Item {
        key: Vec::new(),
        path: TOP_LEVEL.to_owned(),
        location: Location {
            node_kind: root.kind().to_owned(),
            bytes: 0..source.len(),
            lines: 1..=source.lines().count().max(1),
        },
        fingerprint: String::new(),
    }];
    let mut bodies = vec![Body {
        node: root,
        owner: root,
        trivia: Vec::new(),
        item: 0,
    }];

    while let Some(body) = bodies.pop() {
        let parent_key = items[body.item].key.clone();
        let parent_path = (body.item != 0).then(|| items[body.item].path.clone());
        let mut owned = HashSet::new();
        let mut trivia = Vec::new();
        let mut occurrences = HashMap::new();
        let mut cursor = body.node.walk();

        for child in body.node.named_children(&mut cursor) {
            if is_trivia(language, child) {
                trivia.push(child);
                continue;
            }

            let declaration = describe(language, source, child);
            let count = occurrences
                .entry((declaration.label.clone(), declaration.name.key.clone()))
                .or_insert(0);
            let occurrence = *count;
            *count += 1;

            let mut segment = if declaration.name.display.is_empty() {
                declaration.label.clone()
            } else {
                format!("{} {}", declaration.label, declaration.name.display)
            };
            if occurrence > 0 {
                segment = format!("{segment} #{}", occurrence + 1);
            }
            let path = match &parent_path {
                Some(parent) => format!("{parent} > {segment}"),
                None => segment,
            };
            let mut key = parent_key.clone();
            key.push(Segment {
                label: declaration.label,
                name: declaration.name.key,
                occurrence,
            });

            let start = trivia.first().copied().unwrap_or(child);
            owned.extend(trivia.iter().map(Node::id));
            owned.insert(child.id());
            let leading = std::mem::take(&mut trivia);
            let fingerprint = match declaration.body {
                Some(_) => String::new(),
                None => fingerprint(language, source, &leading, child, &HashSet::new()),
            };
            items.push(Item {
                key,
                path,
                location: Location {
                    node_kind: child.kind().to_owned(),
                    bytes: start.start_byte()..child.end_byte(),
                    lines: line_span(start, child),
                },
                fingerprint,
            });

            if let Some(inner) = declaration.body {
                bodies.push(Body {
                    node: inner,
                    owner: child,
                    trivia: leading,
                    item: items.len() - 1,
                });
            }
        }

        // Trivia after the last item stays with the enclosing container.
        items[body.item].fingerprint =
            fingerprint(language, source, &body.trivia, body.owner, &owned);
    }

    items.sort_by_key(|item| (item.location.bytes.start, Reverse(item.location.bytes.end)));
    items
}

// Comments (and Rust outer attributes) belong to the item that follows them.
fn is_trivia(language: Language, node: Node) -> bool {
    node.is_extra() || (language == Language::Rust && node.kind() == "attribute_item")
}

fn describe<'tree>(language: Language, source: &str, node: Node<'tree>) -> Declaration<'tree> {
    let mut name = NameBuilder {
        language,
        source,
        name: Name::default(),
    };
    let described = match language {
        Language::Rust => describe_rust(&mut name, node),
        Language::Python => describe_python(&mut name, node),
    };
    match described {
        Some((label, body)) => Declaration {
            label: label.to_owned(),
            name: name.name,
            body,
        },
        None => Declaration {
            label: node.kind().to_owned(),
            name: Name::default(),
            body: None,
        },
    }
}

fn describe_rust<'tree>(
    name: &mut NameBuilder,
    node: Node<'tree>,
) -> Option<(&'static str, Option<Node<'tree>>)> {
    let field = |field: &str| node.child_by_field_name(field);
    let named = match node.kind() {
        "function_item" | "function_signature_item" => Some("fn"),
        "struct_item" => Some("struct"),
        "enum_item" => Some("enum"),
        "union_item" => Some("union"),
        "trait_item" => Some("trait"),
        "mod_item" => Some("mod"),
        "const_item" => Some("const"),
        "static_item" => Some("static"),
        "type_item" | "associated_type" => Some("type"),
        "macro_definition" => Some("macro_rules!"),
        "extern_crate_declaration" => Some("extern crate"),
        _ => None,
    };

    let label = if let Some(label) = named {
        name.node(field("name"));
        label
    } else {
        match node.kind() {
            "impl_item" => {
                if let Some(implemented) = field("trait") {
                    name.node(Some(implemented));
                    name.text(" for ");
                }
                name.node(field("type"));
                "impl"
            }
            "foreign_mod_item" => {
                let mut cursor = node.walk();
                let abi = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "extern_modifier")
                    .and_then(|modifier| modifier.named_child(0));
                name.node(abi);
                "extern"
            }
            "use_declaration" => {
                match field("argument") {
                    Some(argument) if argument.kind() == "scoped_use_list" => {
                        name.node(argument.child_by_field_name("path"));
                        name.text("::{..}");
                    }
                    Some(argument) if argument.kind() == "use_list" => name.text("{..}"),
                    argument => name.node(argument),
                }
                "use"
            }
            "macro_invocation" => {
                name.node(field("macro"));
                name.text("!");
                "macro"
            }
            "expression_statement" => {
                let invocation = node
                    .named_child(0)
                    .filter(|child| child.kind() == "macro_invocation")?;
                name.node(invocation.child_by_field_name("macro"));
                name.text("!");
                "macro"
            }
            "inner_attribute_item" => {
                let path = node
                    .named_child(0)
                    .filter(|attribute| attribute.kind() == "attribute")
                    .and_then(|attribute| attribute.named_child(0));
                name.node(path);
                "inner attribute"
            }
            _ => return None,
        }
    };

    let body = matches!(
        node.kind(),
        "impl_item" | "trait_item" | "mod_item" | "foreign_mod_item"
    )
    .then(|| field("body"))
    .flatten()
    .filter(|body| body.kind() == "declaration_list");
    Some((label, body))
}

fn describe_python<'tree>(
    name: &mut NameBuilder,
    node: Node<'tree>,
) -> Option<(&'static str, Option<Node<'tree>>)> {
    let field = |field: &str| node.child_by_field_name(field);
    match node.kind() {
        // Decorators stay part of the item; identity comes from the definition.
        "decorated_definition" => describe_python(name, field("definition")?),
        "function_definition" => {
            name.node(field("name"));
            Some(("def", None))
        }
        "class_definition" => {
            name.node(field("name"));
            Some(("class", field("body")))
        }
        "import_statement" => {
            let mut cursor = node.walk();
            for (index, imported) in node.children_by_field_name("name", &mut cursor).enumerate() {
                if index > 0 {
                    name.text(", ");
                }
                name.node(Some(imported));
            }
            Some(("import", None))
        }
        "future_import_statement" => {
            name.text("__future__");
            Some(("from", None))
        }
        "import_from_statement" => {
            name.node(field("module_name"));
            Some(("from", None))
        }
        "expression_statement" => {
            let expression = node.named_child(0)?;
            match expression.kind() {
                "assignment" | "augmented_assignment" => {
                    name.node(expression.child_by_field_name("left"));
                    Some(("variable", None))
                }
                "call" => {
                    name.node(expression.child_by_field_name("function"));
                    Some(("call", None))
                }
                _ => None,
            }
        }
        "if_statement" => {
            name.node(field("condition"));
            Some(("if", None))
        }
        "type_alias_statement" => {
            name.node(field("left"));
            Some(("type", None))
        }
        _ => None,
    }
}

fn line_span(start: Node, end: Node) -> RangeInclusive<usize> {
    let first = start.start_position().row + 1;
    let end = end.end_position();
    let last = if end.column == 0 && end.row >= first {
        end.row
    } else {
        end.row + 1
    };
    first..=last.max(first)
}

fn fingerprint(
    language: Language,
    source: &str,
    trivia: &[Node],
    node: Node,
    skip: &HashSet<usize>,
) -> String {
    let mut output = String::new();
    for &leading in trivia {
        write_node(&mut output, language, source, leading, skip);
    }
    write_node(&mut output, language, source, node, skip);
    output
}

struct Frame<'tree> {
    node: Node<'tree>,
    end: usize,
    commas: usize,
    // Set after a comma that may be an optional trailing comma. It collects
    // the comments that follow until the next token decides.
    pending: Option<String>,
}

impl<'tree> Frame<'tree> {
    fn new(node: Node<'tree>) -> Self {
        Self {
            node,
            end: node.start_byte(),
            commas: 0,
            pending: None,
        }
    }
}

// Serializes the syntax tree without its layout. Whitespace between tokens,
// line continuations, Python `;` separators, spacing inside comments, CRLF line
// endings, and optional trailing commas do not affect the result. Nodes in
// `skip` (a container's child items) are left out.
fn write_node(
    output: &mut String,
    language: Language,
    source: &str,
    root: Node,
    skip: &HashSet<usize>,
) {
    if is_ignored(language, root) {
        return;
    }
    if is_atomic(language, root) {
        write_leaf(output, source, root);
        return;
    }

    push_field(output, 'O', root.kind());
    let mut frames = vec![Frame::new(root)];
    let mut cursor = root.walk();
    let mut at_child = cursor.goto_first_child();

    loop {
        if at_child {
            let node = cursor.node();
            let frame = frames.last_mut().expect("the parent node has a frame");
            let gap = gap(source, frame.end, node.start_byte());
            frame.end = node.end_byte();
            let skipped = skip.contains(&node.id());

            if node.is_extra() || skipped {
                let target = match frame.pending.as_mut() {
                    Some(pending) => pending,
                    None => &mut *output,
                };
                push_gap(target, gap);
                if !skipped && !is_ignored(language, node) {
                    write_leaf(target, source, node);
                }
            } else {
                if let Some(pending) = frame.pending.take() {
                    if !(is_closer(node) && trailing_comma_is_optional(language, frame)) {
                        write_comma(output);
                    }
                    output.push_str(&pending);
                }
                push_gap(output, gap);

                if is_comma(node) {
                    frame.commas += 1;
                    if may_drop_trailing_comma(language, frame.node) {
                        frame.pending = Some(String::new());
                    } else {
                        write_comma(output);
                    }
                } else if !is_ignored(language, node) {
                    if is_atomic(language, node) {
                        write_leaf(output, source, node);
                    } else {
                        push_field(output, 'O', node.kind());
                        frames.push(Frame::new(node));
                        if cursor.goto_first_child() {
                            continue;
                        }
                        let frame = frames.pop().expect("the frame was just pushed");
                        close_frame(output, language, source, frame);
                    }
                }
            }
        }

        loop {
            if cursor.goto_next_sibling() {
                at_child = true;
                break;
            }
            let frame = frames.pop().expect("every open node has a frame");
            close_frame(output, language, source, frame);
            if frames.is_empty() {
                return;
            }
            cursor.goto_parent();
        }
    }
}

fn close_frame(output: &mut String, language: Language, source: &str, frame: Frame) {
    if let Some(pending) = &frame.pending {
        if !trailing_comma_is_optional(language, &frame) {
            write_comma(output);
        }
        output.push_str(pending);
    }
    push_gap(output, gap(source, frame.end, frame.node.end_byte()));
    output.push('C');
}

fn is_atomic(language: Language, node: Node) -> bool {
    node.child_count() == 0
        || node.is_extra()
        || matches!(
            (language, node.kind()),
            (
                Language::Rust,
                "string_literal" | "raw_string_literal" | "char_literal"
            ) | (Language::Python, "string")
        )
}

fn is_ignored(language: Language, node: Node) -> bool {
    language == Language::Python
        && (node.kind() == "line_continuation" || (!node.is_named() && node.kind() == ";"))
}

fn is_comma(node: Node) -> bool {
    !node.is_named() && node.kind() == ","
}

fn is_closer(node: Node) -> bool {
    !node.is_named() && matches!(node.kind(), ")" | "]" | "}" | ">" | "|")
}

// Trailing commas can matter in Rust macro token trees and in Python 2 print
// statements, so they are always kept there.
fn may_drop_trailing_comma(language: Language, parent: Node) -> bool {
    match language {
        Language::Rust => !parent.kind().starts_with("token_"),
        Language::Python => !matches!(parent.kind(), "print_statement" | "exec_statement"),
    }
}

// A trailing comma after two or more elements never changes the meaning. After
// a single element it can (`(u8,)` is a tuple type and `x[a,]` indexes with a
// tuple), so there it is only dropped in lists where it is always optional.
fn trailing_comma_is_optional(language: Language, frame: &Frame) -> bool {
    frame.commas >= 2
        || match language {
            Language::Rust => matches!(
                frame.node.kind(),
                "arguments"
                    | "parameters"
                    | "closure_parameters"
                    | "type_arguments"
                    | "type_parameters"
                    | "field_declaration_list"
                    | "ordered_field_declaration_list"
                    | "field_initializer_list"
                    | "enum_variant_list"
                    | "use_list"
                    | "where_clause"
                    | "array_expression"
                    | "tuple_struct_pattern"
                    | "struct_pattern"
                    | "slice_pattern"
                    | "match_arm"
                    | "last_match_arm"
            ),
            Language::Python => matches!(
                frame.node.kind(),
                "argument_list"
                    | "parameters"
                    | "lambda_parameters"
                    | "list"
                    | "set"
                    | "dictionary"
                    | "import_from_statement"
                    | "future_import_statement"
                    | "list_pattern"
                    | "type_parameter"
            ),
        }
}

fn write_leaf(output: &mut String, source: &str, node: Node) {
    let raw = text(source, node);
    let content = if node.is_extra() {
        Cow::Owned(collapse_whitespace(&raw))
    } else if raw.contains('\r') {
        Cow::Owned(raw.replace("\r\n", "\n"))
    } else {
        raw
    };
    push_field(output, 'L', node.kind());
    push_field(output, '=', &content);
}

fn write_comma(output: &mut String) {
    push_field(output, 'L', ",");
    push_field(output, '=', ",");
}

// Text between child nodes that no child covers (hidden grammar tokens).
fn gap(source: &str, start: usize, end: usize) -> Option<String> {
    let text = source.get(start..end)?;
    (!text.trim().is_empty()).then(|| collapse_whitespace(text))
}

fn push_gap(output: &mut String, gap: Option<String>) {
    if let Some(gap) = gap {
        push_field(output, 'G', &gap);
    }
}

// Length prefixes keep the encoding unambiguous for any token text.
fn push_field(output: &mut String, tag: char, value: &str) {
    // Writing to a String cannot fail.
    let _ = write!(output, "{tag}{}:", value.len());
    output.push_str(value);
}

fn text<'source>(source: &'source str, node: Node) -> Cow<'source, str> {
    let range = node.byte_range();
    match source.get(range.clone()) {
        Some(text) => Cow::Borrowed(text),
        None => String::from_utf8_lossy(source.as_bytes().get(range).unwrap_or_default()),
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tree_sitter::Parser;

    fn items(language: Language, source: &str) -> Vec<Item> {
        let mut parser = Parser::new();
        parser.set_language(&language.grammar()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "invalid test input:\n{source}"
        );
        extract(language, source, &tree)
    }

    fn outline(language: Language, source: &str) -> Vec<(String, String, String)> {
        items(language, source)
            .into_iter()
            .map(|item| {
                let start = item.location.bytes.start;
                let first_line = source[start..].lines().next().unwrap_or_default();
                (item.path, item.location.node_kind, first_line.to_owned())
            })
            .collect()
    }

    // Keys and fingerprints of every item, independent of item order.
    fn content(language: Language, source: &str) -> BTreeMap<Vec<Segment>, String> {
        items(language, source)
            .into_iter()
            .map(|item| (item.key, item.fingerprint))
            .collect()
    }

    fn outline_row(path: &str, kind: &str, first_line: &str) -> (String, String, String) {
        (path.to_owned(), kind.to_owned(), first_line.to_owned())
    }

    #[test]
    fn extracts_rust_items_with_attached_comments_and_attributes() {
        let source = "\
//! Crate docs.
#![allow(dead_code)]

use std::fmt;
use std::{collections::HashMap, io};

/// A counter.
#[derive(Debug)]
pub struct Counter {
    value: u32,
}

impl fmt::Display for Counter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, \"{}\", self.value)
    }
}

mod tests {
    #[test]
    fn works() {}
}

thread_local! {
    static DEPTH: u32 = 0;
}
";
        assert_eq!(
            outline(Language::Rust, source),
            [
                outline_row("(top level)", "source_file", "//! Crate docs."),
                outline_row(
                    "inner attribute allow",
                    "inner_attribute_item",
                    "//! Crate docs."
                ),
                outline_row("use std::fmt", "use_declaration", "use std::fmt;"),
                outline_row(
                    "use std::{..}",
                    "use_declaration",
                    "use std::{collections::HashMap, io};"
                ),
                outline_row("struct Counter", "struct_item", "/// A counter."),
                outline_row(
                    "impl fmt::Display for Counter",
                    "impl_item",
                    "impl fmt::Display for Counter {"
                ),
                outline_row(
                    "impl fmt::Display for Counter > fn fmt",
                    "function_item",
                    "fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {"
                ),
                outline_row("mod tests", "mod_item", "mod tests {"),
                outline_row("mod tests > fn works", "function_item", "#[test]"),
                outline_row("macro thread_local!", "macro_invocation", "thread_local! {"),
            ]
        );
    }

    #[test]
    fn reports_item_ranges_and_lines() {
        let source = "fn a() {}\n\n/// Docs.\n#[inline]\nfn b() {\n    a();\n}\n";
        let items = items(Language::Rust, source);
        let b = items.iter().find(|item| item.path == "fn b").unwrap();
        let start = source.find("/// Docs.").unwrap();
        assert_eq!(b.location.bytes, start..source.len() - 1);
        assert_eq!(b.location.lines, 3..=7);
        assert_eq!(items[0].location.bytes, 0..source.len());
        assert_eq!(items[0].location.lines, 1..=7);
    }

    #[test]
    fn extracts_python_items_with_decorators_and_module_statements() {
        let source = "\
\"\"\"Module docs.\"\"\"
from __future__ import annotations

import os, sys
from typing import List

__all__ = [\"Greeter\"]


@dataclass
class Greeter:
    name: str = \"world\"

    # Greets.
    def hello(self):
        return f\"hello {self.name}\"

    @property
    def size(self):
        return 1

    @size.setter
    def size(self, value):
        pass


def main():
    Greeter().hello()


if __name__ == \"__main__\":
    main()
";
        assert_eq!(
            outline(Language::Python, source),
            [
                outline_row("(top level)", "module", "\"\"\"Module docs.\"\"\""),
                outline_row(
                    "expression_statement",
                    "expression_statement",
                    "\"\"\"Module docs.\"\"\""
                ),
                outline_row(
                    "from __future__",
                    "future_import_statement",
                    "from __future__ import annotations"
                ),
                outline_row("import os, sys", "import_statement", "import os, sys"),
                outline_row(
                    "from typing",
                    "import_from_statement",
                    "from typing import List"
                ),
                outline_row(
                    "variable __all__",
                    "expression_statement",
                    "__all__ = [\"Greeter\"]"
                ),
                outline_row("class Greeter", "decorated_definition", "@dataclass"),
                outline_row(
                    "class Greeter > variable name",
                    "expression_statement",
                    "name: str = \"world\""
                ),
                outline_row(
                    "class Greeter > def hello",
                    "function_definition",
                    "# Greets."
                ),
                outline_row(
                    "class Greeter > def size",
                    "decorated_definition",
                    "@property"
                ),
                outline_row(
                    "class Greeter > def size #2",
                    "decorated_definition",
                    "@size.setter"
                ),
                outline_row("def main", "function_definition", "def main():"),
                outline_row(
                    "if __name__ == \"__main__\"",
                    "if_statement",
                    "if __name__ == \"__main__\":"
                ),
            ]
        );
    }

    #[test]
    fn containers_exclude_their_child_items() {
        let base = "impl Counter {\n    fn get(&self) -> u32 {\n        1\n    }\n}\n";
        let edited = base.replace("        1\n", "        2\n");
        let (base, edited) = (items(Language::Rust, base), items(Language::Rust, &edited));
        let fingerprint = |items: &[Item], path: &str| {
            items
                .iter()
                .find(|item| item.path == path)
                .unwrap()
                .fingerprint
                .clone()
        };
        assert_eq!(
            fingerprint(&base, "impl Counter"),
            fingerprint(&edited, "impl Counter")
        );
        assert_ne!(
            fingerprint(&base, "impl Counter > fn get"),
            fingerprint(&edited, "impl Counter > fn get")
        );
    }

    #[test]
    fn formatting_changes_do_not_change_rust_items() {
        for (original, reformatted) in [
            (
                "fn f(a: u32, b: u32) -> u32 {\n    a + b\n}\n",
                "fn f(a:u32,b:u32)->u32{a+b}\n",
            ),
            (
                "fn f() {\n    g(a, b);\n}\n",
                "fn f() {\n    g(\n        a,\n        b,\n    );\n}\n",
            ),
            (
                "fn f() {\n    g(a);\n}\n",
                "fn f() {\n    g(\n        a,\n    );\n}\n",
            ),
            ("struct S {\n    a: u8,\n}\n", "struct S { a: u8 }\n"),
            ("enum E {\n    A,\n    B,\n}\n", "enum E { A, B }\n"),
            (
                "use std::{fmt, io};\n",
                "use std::{\n    fmt,\n    io,\n};\n",
            ),
            (
                "fn f(x: u8) -> u8 {\n    match x {\n        0 => 1,\n        _ => 2,\n    }\n}\n",
                "fn f(x: u8) -> u8 { match x { 0 => 1, _ => 2 } }\n",
            ),
            (
                "fn f(x: u8) {\n    match x {\n        0 => {\n            g();\n        },\n        _ => {}\n    }\n}\n",
                "fn f(x: u8) {\n    match x {\n        0 => {\n            g();\n        }\n        _ => {}\n    }\n}\n",
            ),
            (
                "fn f<T>()\nwhere\n    T: Clone,\n{\n}\n",
                "fn f<T>() where T: Clone {}\n",
            ),
            ("// A  note.\nfn f() {}\n", "//   A note.   \nfn f() {}\n"),
            (
                "/// Docs\n/// here.\nfn f() {}\n",
                "///   Docs\n///  here.\nfn f() {}\n",
            ),
            ("fn f() {\r\n    1;\r\n}\r\n", "fn f() {\n    1;\n}\n"),
            ("fn a() {}\nfn b() {}\n", "fn b() {}\n\n\nfn a() {}\n"),
            (
                "fn f() {\n    let x = vec![1, 2];\n}\n",
                "fn f() {\n    let x = vec![1,2];\n}\n",
            ),
        ] {
            assert_eq!(
                content(Language::Rust, original),
                content(Language::Rust, reformatted),
                "{original:?} should match {reformatted:?}"
            );
        }
    }

    #[test]
    fn formatting_changes_do_not_change_python_items() {
        for (original, reformatted) in [
            (
                "def f(a, b):\n    return a + b\n",
                "def f(a,b):\n  return a+b\n",
            ),
            ("x = f(a)\n", "x = f(\n    a,\n)\n"),
            ("x = f(a, b)\n", "x = f(\n    a,\n    b,\n)\n"),
            ("items = [1, 2]\n", "items = [\n    1,\n    2,\n]\n"),
            ("def f(a):\n    pass\n", "def f(\n    a,\n):\n    pass\n"),
            (
                "from x import (a, b)\n",
                "from x import (\n    a,\n    b,\n)\n",
            ),
            ("x = 1 + 2\n", "x = 1 + \\\n    2\n"),
            ("a = 1\nb = 2\n", "a = 1; b = 2\n"),
            ("# A  note.\nx = 1\n", "#   A note.\nx = 1\n"),
            (
                "def f():\n    return 1\n\n\ndef g():\n    pass\n",
                "def g():\n    pass\ndef f():\n    return 1\n",
            ),
            (
                "class C:\n    def f(self):\n        pass\n",
                "class C:\n\n    def f(self):\n        pass\n\n\n",
            ),
        ] {
            assert_eq!(
                content(Language::Python, original),
                content(Language::Python, reformatted),
                "{original:?} should match {reformatted:?}"
            );
        }
    }

    #[test]
    fn meaningful_edits_change_rust_items() {
        for (original, edited) in [
            (
                "fn f() -> &'static str {\n    \"a b\"\n}\n",
                "fn f() -> &'static str {\n    \"a  b\"\n}\n",
            ),
            (
                "fn f() -> &'static str {\n    r\"a b\"\n}\n",
                "fn f() -> &'static str {\n    r\"a\n b\"\n}\n",
            ),
            (
                "fn f() {\n    m!(a, b,);\n}\n",
                "fn f() {\n    m!(a, b);\n}\n",
            ),
            ("type T = (u8,);\n", "type T = (u8);\n"),
            (
                "fn f() {\n    let (a,) = x;\n}\n",
                "fn f() {\n    let (a) = x;\n}\n",
            ),
            ("// A note.\nfn f() {}\n", "// Another note.\nfn f() {}\n"),
            ("// A note.\nfn f() {}\n", "/// A note.\nfn f() {}\n"),
            ("#[inline]\nfn f() {}\n", "fn f() {}\n"),
            (
                "fn f() {\n    a(); b();\n}\n",
                "fn f() {\n    b(); a();\n}\n",
            ),
        ] {
            assert_ne!(
                content(Language::Rust, original),
                content(Language::Rust, edited),
                "{original:?} should differ from {edited:?}"
            );
        }
    }

    #[test]
    fn meaningful_edits_change_python_items() {
        for (original, edited) in [
            (
                "def f():\n    if a:\n        b()\n    c()\n",
                "def f():\n    if a:\n        b()\n        c()\n",
            ),
            ("x = y[a,]\n", "x = y[a]\n"),
            ("x = (a,)\n", "x = (a)\n"),
            ("x = a,\n", "x = a\n"),
            (
                "def f():\n    \"\"\"A  b.\"\"\"\n",
                "def f():\n    \"\"\"A b.\"\"\"\n",
            ),
            ("x = 'a'\n", "x = \"a\"\n"),
            ("x = f\"{a = }\"\n", "x = f\"{a=}\"\n"),
            ("# A note.\nx = 1\n", "# Another note.\nx = 1\n"),
            ("@cache\ndef f():\n    pass\n", "def f():\n    pass\n"),
        ] {
            assert_ne!(
                content(Language::Python, original),
                content(Language::Python, edited),
                "{original:?} should differ from {edited:?}"
            );
        }
    }
}
