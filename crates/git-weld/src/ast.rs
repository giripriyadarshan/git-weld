mod items;

pub use crate::merge::Version;
pub(crate) use items::Segment;

use items::{Item, Reference};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    ops::{Range, RangeInclusive},
    path::Path,
};
use tree_sitter::{Node, Parser, Point, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Language {
    Rust,
    Python,
}

impl Language {
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            _ => None,
        }
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisError {
    UnsupportedFileType,
    InvalidUtf8 {
        version: Version,
    },
    Syntax {
        version: Version,
        line: usize,
        column: usize,
    },
    Parser {
        language: Language,
        message: String,
    },
}

impl fmt::Display for AnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFileType => {
                formatter.write_str("unsupported file type (supported: .rs, .py)")
            }
            Self::InvalidUtf8 { version } => write!(formatter, "{version} is not valid UTF-8"),
            Self::Syntax {
                version,
                line,
                column,
            } => write!(
                formatter,
                "{version} has a syntax error at line {line}, column {column}"
            ),
            Self::Parser { language, message } => {
                write!(formatter, "the {language} parser failed: {message}")
            }
        }
    }
}

impl Error for AnalysisError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Analysis {
    pub language: Language,
    /// The items that both sides changed, in the order they first appear.
    pub diverged_items: Vec<DivergedItem>,
    outlines: [Outline; 3],
}

impl Analysis {
    /// The items of one version.
    pub fn outline(&self, version: Version) -> &Outline {
        &self.outlines[version as usize]
    }
}

/// The items of one version as a tree. The root (index 0) is the whole file;
/// the children of a container are the members of its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outline {
    items: Vec<OutlineItem>,
    references: Vec<Reference>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineItem {
    pub(crate) key: Vec<Segment>,
    /// Human-readable path, such as `impl Counter > fn get`.
    pub path: String,
    pub(crate) name: Option<String>,
    pub(crate) signature: Option<String>,
    pub node_kind: String,
    /// Includes the item's leading comments, attributes, and decorators.
    pub bytes: Range<usize>,
    pub(crate) parent: Option<usize>,
    /// Indices of the members, in file order. Only containers have members.
    pub children: Vec<usize>,
    /// Whether the members are compared one by one: impl, trait, mod, and
    /// extern blocks, classes, and the root.
    pub container: bool,
}

impl Outline {
    pub const ROOT: usize = 0;

    fn new(items: &[Item], references: Vec<Reference>) -> Self {
        let positions: HashMap<&[Segment], usize> = items
            .iter()
            .enumerate()
            .map(|(position, item)| (item.key.as_slice(), position))
            .collect();
        let mut outline: Vec<OutlineItem> = items
            .iter()
            .map(|item| OutlineItem {
                key: item.key.clone(),
                path: item.path.clone(),
                name: item.name.clone(),
                signature: item.signature.clone(),
                node_kind: item.location.node_kind.clone(),
                bytes: item.location.bytes.clone(),
                parent: None,
                children: Vec::new(),
                container: item.container,
            })
            .collect();
        for (position, item) in items.iter().enumerate() {
            if let Some((_, parent)) = item.key.split_last()
                && let Some(&parent) = positions.get(parent)
                && parent != position
            {
                outline[position].parent = Some(parent);
                outline[parent].children.push(position);
            }
        }
        Self {
            items: outline,
            references,
        }
    }

    pub fn item(&self, index: usize) -> &OutlineItem {
        &self.items[index]
    }

    pub fn items(&self) -> &[OutlineItem] {
        &self.items
    }

    /// Names referenced inside `bytes`, in source order.
    pub fn referenced_names(&self, bytes: Range<usize>) -> impl Iterator<Item = &str> {
        self.references
            .iter()
            .filter(move |reference| {
                bytes.start <= reference.bytes.start && reference.bytes.end <= bytes.end
            })
            .map(|reference| reference.name.as_str())
    }
}

/// Why a merged result does not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyntaxError {
    InvalidUtf8,
    At { line: usize, column: usize },
    Parser(String),
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => formatter.write_str("it is not valid UTF-8"),
            Self::At { line, column } => {
                write!(
                    formatter,
                    "it has a syntax error at line {line}, column {column}"
                )
            }
            Self::Parser(message) => write!(formatter, "the parser failed: {message}"),
        }
    }
}

impl Error for SyntaxError {}

/// An item that both sides changed relative to the base in different ways.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DivergedItem {
    /// Human-readable path, such as `impl Counter > fn get`.
    pub item: String,
    pub kind: Divergence,
    pub base: Option<Location>,
    pub ours: Option<Location>,
    pub theirs: Option<Location>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Divergence {
    BothModified,
    BothAdded,
    ModifiedInOursDeletedInTheirs,
    DeletedInOursModifiedInTheirs,
}

/// Where an item is in one version. Ranges include the item's leading
/// comments, attributes, and decorators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub node_kind: String,
    pub bytes: Range<usize>,
    pub lines: RangeInclusive<usize>,
}

impl fmt::Display for DivergedItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let change = match self.kind {
            Divergence::BothModified => "modified on both sides",
            Divergence::BothAdded => "added on both sides with different content",
            Divergence::ModifiedInOursDeletedInTheirs => "modified in ours, deleted in theirs",
            Divergence::DeletedInOursModifiedInTheirs => "deleted in ours, modified in theirs",
        };
        write!(formatter, "{}: {change} (", self.item)?;
        let mut separator = "";
        for (version, location) in [
            (Version::Base, &self.base),
            (Version::Ours, &self.ours),
            (Version::Theirs, &self.theirs),
        ] {
            if let Some(location) = location {
                let (first, last) = (location.lines.start(), location.lines.end());
                if first == last {
                    write!(formatter, "{separator}{version} line {first}")?;
                } else {
                    write!(formatter, "{separator}{version} lines {first}-{last}")?;
                }
                separator = ", ";
            }
        }
        formatter.write_str(")")
    }
}

/// Parses all three versions of the file at the logical path `path` and finds
/// the items that both sides changed. Edits that only change formatting do not
/// count as changes.
pub fn analyze(
    path: &Path,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> Result<Analysis, AnalysisError> {
    let language = Language::from_path(path).ok_or(AnalysisError::UnsupportedFileType)?;
    let mut parser = Parser::new();
    parser
        .set_language(&language.grammar())
        .map_err(|error| AnalysisError::Parser {
            language,
            message: error.to_string(),
        })?;

    let mut extract = |version: Version,
                       source: &[u8]|
     -> Result<(Vec<Item>, Vec<Reference>), AnalysisError> {
        let (text, tree) = parse(&mut parser, language, source).map_err(|error| match error {
            SyntaxError::InvalidUtf8 => AnalysisError::InvalidUtf8 { version },
            SyntaxError::At { line, column } => AnalysisError::Syntax {
                version,
                line,
                column,
            },
            SyntaxError::Parser(message) => AnalysisError::Parser { language, message },
        })?;
        Ok((
            items::extract(language, text, &tree),
            items::references(language, text, &tree),
        ))
    };
    let (base, base_refs) = extract(Version::Base, base)?;
    let (ours, ours_refs) = extract(Version::Ours, ours)?;
    let (theirs, theirs_refs) = extract(Version::Theirs, theirs)?;

    Ok(Analysis {
        language,
        diverged_items: compare(&base, &ours, &theirs),
        outlines: [
            Outline::new(&base, base_refs),
            Outline::new(&ours, ours_refs),
            Outline::new(&theirs, theirs_refs),
        ],
    })
}

/// Checks that `source`, such as a stitched merge result, parses as
/// `language` without syntax errors.
pub fn check_syntax(language: Language, source: &[u8]) -> Result<(), SyntaxError> {
    let mut parser = Parser::new();
    parser
        .set_language(&language.grammar())
        .map_err(|error| SyntaxError::Parser(error.to_string()))?;
    parse(&mut parser, language, source).map(|_| ())
}

fn parse<'source>(
    parser: &mut Parser,
    language: Language,
    source: &'source [u8],
) -> Result<(&'source str, Tree), SyntaxError> {
    let text = std::str::from_utf8(source).map_err(|_| SyntaxError::InvalidUtf8)?;
    let tree = parser
        .parse(text, None)
        .ok_or_else(|| SyntaxError::Parser("parsing did not complete".to_owned()))?;
    let root = tree.root_node();
    let error = if root.has_error() {
        Some(first_error(root).unwrap_or_else(|| root.start_position()))
    } else if language == Language::Python {
        first_empty_block(root)
    } else {
        None
    };
    match error {
        Some(point) => Err(SyntaxError::At {
            line: point.row + 1,
            column: point.column + 1,
        }),
        None => Ok((text, tree)),
    }
}

// tree-sitter-python accepts a block without statements, such as a class
// whose only method was removed, but Python rejects it ("expected an indented
// block").
fn first_empty_block(root: Node) -> Option<Point> {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.kind() == "block"
            && !node
                .named_children(&mut node.walk())
                .any(|child| !child.is_extra())
        {
            return Some(node.start_position());
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return None;
            }
        }
    }
}

fn first_error(root: Node) -> Option<Point> {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            return Some(node.start_position());
        }
        if node.has_error() && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return None;
            }
        }
    }
}

fn compare(base: &[Item], ours: &[Item], theirs: &[Item]) -> Vec<DivergedItem> {
    let (base_index, ours_index, theirs_index) = (index(base), index(ours), index(theirs));
    let mut seen = HashSet::new();
    let mut deleted: Vec<&[Segment]> = Vec::new();
    let mut diverged = Vec::new();

    for key in base
        .iter()
        .chain(ours)
        .chain(theirs)
        .map(|item| item.key.as_slice())
    {
        if !seen.insert(key) || deleted.iter().any(|prefix| key.starts_with(prefix)) {
            continue;
        }
        let (old, new_ours, new_theirs) = (
            base_index.get(key).copied(),
            ours_index.get(key).copied(),
            theirs_index.get(key).copied(),
        );
        let kind = match (old, new_ours, new_theirs) {
            (Some(old), Some(new_ours), Some(new_theirs))
                if old.fingerprint != new_ours.fingerprint
                    && old.fingerprint != new_theirs.fingerprint
                    && new_ours.fingerprint != new_theirs.fingerprint =>
            {
                Divergence::BothModified
            }
            (None, Some(new_ours), Some(new_theirs))
                if new_ours.fingerprint != new_theirs.fingerprint =>
            {
                Divergence::BothAdded
            }
            (Some(_), Some(_), None) if adds_or_modifies(key, &base_index, ours) => {
                deleted.push(key);
                Divergence::ModifiedInOursDeletedInTheirs
            }
            (Some(_), None, Some(_)) if adds_or_modifies(key, &base_index, theirs) => {
                deleted.push(key);
                Divergence::DeletedInOursModifiedInTheirs
            }
            _ => continue,
        };
        let path = new_ours
            .or(new_theirs)
            .or(old)
            .map(|item| item.path.clone());
        diverged.push(DivergedItem {
            item: path.unwrap_or_default(),
            kind,
            base: old.map(|item| item.location.clone()),
            ours: new_ours.map(|item| item.location.clone()),
            theirs: new_theirs.map(|item| item.location.clone()),
        });
    }
    diverged
}

fn index(items: &[Item]) -> HashMap<&[Segment], &Item> {
    items
        .iter()
        .map(|item| (item.key.as_slice(), item))
        .collect()
}

// Whether `side` added or modified the item at `key` or anything inside it.
// Removing children of an item that the other side deleted is compatible.
fn adds_or_modifies(key: &[Segment], base: &HashMap<&[Segment], &Item>, side: &[Item]) -> bool {
    side.iter()
        .filter(|item| item.key.starts_with(key))
        .any(|item| {
            base.get(item.key.as_slice())
                .is_none_or(|original| original.fingerprint != item.fingerprint)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_BASE: &str = "fn alpha() -> u32 {\n    1\n}\n\nfn beta() -> u32 {\n    2\n}\n";
    const PYTHON_BASE: &str = "\
import os


def alpha():
    return 1


class Greeter:
    def hello(self):
        return \"hello\"

    def bye(self):
        return \"bye\"
";

    fn analyze_text(
        path: &str,
        base: &str,
        ours: &str,
        theirs: &str,
    ) -> Result<Analysis, AnalysisError> {
        analyze(
            Path::new(path),
            base.as_bytes(),
            ours.as_bytes(),
            theirs.as_bytes(),
        )
    }

    fn summary(path: &str, base: &str, ours: &str, theirs: &str) -> Vec<(String, Divergence)> {
        analyze_text(path, base, ours, theirs)
            .unwrap()
            .diverged_items
            .into_iter()
            .map(|diverged| (diverged.item, diverged.kind))
            .collect()
    }

    fn diverged(item: &str, kind: Divergence) -> (String, Divergence) {
        (item.to_owned(), kind)
    }

    fn location(
        source: &str,
        snippet: &str,
        node_kind: &str,
        lines: RangeInclusive<usize>,
    ) -> Location {
        let start = source.find(snippet).expect("snippet is in the source");
        Location {
            node_kind: node_kind.to_owned(),
            bytes: start..start + snippet.len(),
            lines,
        }
    }

    #[test]
    fn parses_rust_source_into_a_syntax_tree() {
        let mut parser = Parser::new();
        parser.set_language(&Language::Rust.grammar()).unwrap();
        let tree = parser.parse("fn main() {}\n", None).unwrap();
        let root = tree.root_node();

        assert_eq!(root.kind(), "source_file");
        assert!(!root.has_error());
        assert_eq!(root.named_child(0).unwrap().kind(), "function_item");
    }

    #[test]
    fn parses_python_source_into_a_syntax_tree() {
        let mut parser = Parser::new();
        parser.set_language(&Language::Python.grammar()).unwrap();
        let tree = parser.parse("def main():\n    pass\n", None).unwrap();
        let root = tree.root_node();

        assert_eq!(root.kind(), "module");
        assert!(!root.has_error());
        assert_eq!(root.named_child(0).unwrap().kind(), "function_definition");
    }

    #[test]
    fn selects_the_language_from_the_logical_path() {
        for (path, language) in [
            ("src/lib.rs", Some(Language::Rust)),
            ("-leading-hyphen.rs", Some(Language::Rust)),
            ("package/module with spaces.py", Some(Language::Python)),
            ("README.md", None),
            ("Makefile", None),
            ("lib.rs.orig", None),
            (".rs", None),
        ] {
            assert_eq!(Language::from_path(Path::new(path)), language, "{path}");
        }
    }

    #[test]
    fn rejects_unsupported_file_types() {
        let error = analyze_text("notes.txt", "a\n", "b\n", "c\n").unwrap_err();

        assert_eq!(error, AnalysisError::UnsupportedFileType);
        assert_eq!(
            error.to_string(),
            "unsupported file type (supported: .rs, .py)"
        );
    }

    #[test]
    fn rejects_input_that_is_not_utf8() {
        let error = analyze(
            Path::new("lib.rs"),
            b"fn a() {}\n",
            b"fn a() {}\n",
            b"fn a() { \"\xff\"; }\n",
        )
        .unwrap_err();

        assert_eq!(
            error,
            AnalysisError::InvalidUtf8 {
                version: Version::Theirs
            }
        );
        assert_eq!(error.to_string(), "theirs is not valid UTF-8");
    }

    #[test]
    fn reports_where_a_version_has_a_syntax_error() {
        let error = analyze_text(
            "lib.rs",
            "fn a() {}\n",
            "fn a() {\n    let x = ;\n}\n",
            "fn a() {}\n",
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                AnalysisError::Syntax {
                    version: Version::Ours,
                    line: 2,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(
            error
                .to_string()
                .starts_with("ours has a syntax error at line 2, column "),
            "{error}"
        );

        let error =
            analyze_text("app.py", "def a(:\n    pass\n", "x = 1\n", "x = 2\n").unwrap_err();
        assert!(
            matches!(
                error,
                AnalysisError::Syntax {
                    version: Version::Base,
                    line: 1,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn rejects_python_blocks_without_statements_like_python_does() {
        for source in [
            "class A:\n\n",
            "def f():\n    # Only a comment.\n",
            "if x:\nelse:\n    pass\n",
        ] {
            let error = check_syntax(Language::Python, source.as_bytes()).unwrap_err();
            assert!(
                matches!(error, SyntaxError::At { .. }),
                "{source:?}: {error:?}"
            );
            assert!(
                analyze_text("app.py", source, "x = 1\n", "x = 2\n").is_err(),
                "{source:?}"
            );
        }
        assert_eq!(
            check_syntax(Language::Python, b"class A:\n    pass\n"),
            Ok(())
        );
        assert_eq!(check_syntax(Language::Python, b"class A: pass\n"), Ok(()));
        assert_eq!(check_syntax(Language::Rust, b"fn f() {}\n"), Ok(()));
        assert_eq!(
            check_syntax(Language::Rust, b"fn f( {}\n")
                .unwrap_err()
                .to_string(),
            "it has a syntax error at line 1, column 6"
        );
        assert_eq!(
            check_syntax(Language::Rust, b"\xff").unwrap_err(),
            SyntaxError::InvalidUtf8
        );
    }

    #[test]
    fn reports_an_item_edited_on_both_sides_with_its_range_in_each_version() {
        let ours = RUST_BASE.replace("    1\n", "    10\n");
        let theirs = format!(
            "const LIMIT: u32 = 3;\n\n{}",
            RUST_BASE.replace("    1\n", "    100\n")
        );

        let analysis = analyze_text("src/lib.rs", RUST_BASE, &ours, &theirs).unwrap();

        assert_eq!(analysis.language, Language::Rust);
        assert_eq!(
            analysis.diverged_items,
            [DivergedItem {
                item: "fn alpha".to_owned(),
                kind: Divergence::BothModified,
                base: Some(location(
                    RUST_BASE,
                    "fn alpha() -> u32 {\n    1\n}",
                    "function_item",
                    1..=3
                )),
                ours: Some(location(
                    &ours,
                    "fn alpha() -> u32 {\n    10\n}",
                    "function_item",
                    1..=3
                )),
                theirs: Some(location(
                    &theirs,
                    "fn alpha() -> u32 {\n    100\n}",
                    "function_item",
                    3..=5
                )),
            }]
        );
        assert_eq!(
            analysis.diverged_items[0].to_string(),
            "fn alpha: modified on both sides (base lines 1-3, ours lines 1-3, theirs lines 3-5)"
        );
    }

    #[test]
    fn ignores_edits_to_different_items_and_identical_edits() {
        let alpha = RUST_BASE.replace("    1\n", "    10\n");
        let beta = RUST_BASE.replace("    2\n", "    20\n");

        assert_eq!(summary("lib.rs", RUST_BASE, &alpha, &beta), []);
        assert_eq!(summary("lib.rs", RUST_BASE, &alpha, &alpha), []);
    }

    #[test]
    fn reports_additions_with_different_content() {
        let added = |body: &str| format!("{RUST_BASE}\nfn gamma() -> u32 {{\n    {body}\n}}\n");
        let (three, four) = (added("3"), added("4"));

        let analysis = analyze_text("lib.rs", RUST_BASE, &three, &four).unwrap();
        assert_eq!(analysis.diverged_items.len(), 1);
        let item = &analysis.diverged_items[0];
        assert_eq!(item.item, "fn gamma");
        assert_eq!(item.kind, Divergence::BothAdded);
        assert_eq!(item.base, None);
        assert_eq!(item.ours.as_ref().unwrap().lines, 9..=11);
        assert_eq!(item.theirs.as_ref().unwrap().lines, 9..=11);
        assert_eq!(
            item.to_string(),
            "fn gamma: added on both sides with different content (ours lines 9-11, theirs lines 9-11)"
        );

        assert_eq!(summary("lib.rs", RUST_BASE, &three, &three), []);
        assert_eq!(summary("lib.rs", RUST_BASE, &three, RUST_BASE), []);
    }

    #[test]
    fn reports_deletions_that_meet_modifications() {
        let deleted = RUST_BASE.replace("fn alpha() -> u32 {\n    1\n}\n\n", "");
        let modified = RUST_BASE.replace("    1\n", "    10\n");
        let beta = RUST_BASE.replace("    2\n", "    20\n");

        let analysis = analyze_text("lib.rs", RUST_BASE, &deleted, &modified).unwrap();
        assert_eq!(analysis.diverged_items.len(), 1);
        assert_eq!(
            analysis.diverged_items[0].kind,
            Divergence::DeletedInOursModifiedInTheirs
        );
        assert_eq!(analysis.diverged_items[0].ours, None);
        assert_eq!(
            analysis.diverged_items[0].to_string(),
            "fn alpha: deleted in ours, modified in theirs (base lines 1-3, theirs lines 1-3)"
        );

        assert_eq!(
            summary("lib.rs", RUST_BASE, &modified, &deleted),
            [diverged(
                "fn alpha",
                Divergence::ModifiedInOursDeletedInTheirs
            )]
        );
        assert_eq!(summary("lib.rs", RUST_BASE, &deleted, &deleted), []);
        assert_eq!(summary("lib.rs", RUST_BASE, &deleted, &beta), []);
    }

    #[test]
    fn compares_methods_inside_impl_blocks_separately() {
        let base = "\
struct Counter {
    value: u32,
}

impl Counter {
    fn get(&self) -> u32 {
        self.value
    }

    fn reset(&mut self) {
        self.value = 0;
    }
}
";
        let get = |value: &str| base.replace("        self.value\n", &format!("        {value}\n"));
        let reset = base.replace("self.value = 0;", "self.value = 1;");

        assert_eq!(summary("lib.rs", base, &get("self.value + 1"), &reset), []);
        assert_eq!(
            summary(
                "lib.rs",
                base,
                &get("self.value + 1"),
                &get("self.value * 2")
            ),
            [diverged("impl Counter > fn get", Divergence::BothModified)]
        );
    }

    #[test]
    fn reports_a_deleted_container_once_when_the_other_side_changed_its_contents() {
        let base = "impl Counter {\n    fn get(&self) -> u32 {\n        1\n    }\n\n    fn reset(&mut self) {}\n}\n";
        let without_impl = "";
        let with_method = base.replace(
            "}\n}\n",
            "}\n\n    fn double(&self) -> u32 {\n        2\n    }\n}\n",
        );
        let edited_methods = base.replace("        1\n", "        10\n").replace(
            "    fn reset(&mut self) {}\n",
            "    fn reset(&mut self) {\n        todo!()\n    }\n",
        );
        let without_reset = base.replace("\n    fn reset(&mut self) {}\n", "");

        assert_eq!(
            summary("lib.rs", base, without_impl, &with_method),
            [diverged(
                "impl Counter",
                Divergence::DeletedInOursModifiedInTheirs
            )]
        );
        assert_eq!(
            summary("lib.rs", base, &edited_methods, without_impl),
            [diverged(
                "impl Counter",
                Divergence::ModifiedInOursDeletedInTheirs
            )]
        );
        assert_eq!(summary("lib.rs", base, without_impl, &without_reset), []);
    }

    #[test]
    fn ignores_rust_formatting_only_edits() {
        let base = "fn alpha(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let reformatted = "fn alpha(\n    a: u32,\n    b: u32,\n) -> u32 {\n    a+b\n}\n";
        let edited = base.replace("a + b", "a * b");

        assert_eq!(summary("lib.rs", base, reformatted, &edited), []);
    }

    #[test]
    fn reports_a_python_function_edited_on_both_sides_with_its_range_in_each_version() {
        let ours = PYTHON_BASE.replace("return 1", "return 10");
        let theirs = PYTHON_BASE
            .replace("import os\n", "import os\nimport sys\n")
            .replace("return 1", "return 100");

        let analysis = analyze_text("app.py", PYTHON_BASE, &ours, &theirs).unwrap();

        assert_eq!(analysis.language, Language::Python);
        assert_eq!(
            analysis.diverged_items,
            [DivergedItem {
                item: "def alpha".to_owned(),
                kind: Divergence::BothModified,
                base: Some(location(
                    PYTHON_BASE,
                    "def alpha():\n    return 1",
                    "function_definition",
                    4..=5
                )),
                ours: Some(location(
                    &ours,
                    "def alpha():\n    return 10",
                    "function_definition",
                    4..=5
                )),
                theirs: Some(location(
                    &theirs,
                    "def alpha():\n    return 100",
                    "function_definition",
                    5..=6
                )),
            }]
        );
    }

    #[test]
    fn compares_python_methods_and_module_items_separately() {
        let hello = |value: &str| PYTHON_BASE.replace("return \"hello\"", value);
        let bye = PYTHON_BASE.replace("return \"bye\"", "return \"goodbye\"");
        let alpha = PYTHON_BASE.replace("return 1", "return 2");

        assert_eq!(
            summary("app.py", PYTHON_BASE, &hello("return \"hi\""), &bye),
            []
        );
        assert_eq!(
            summary("app.py", PYTHON_BASE, &hello("return \"hi\""), &alpha),
            []
        );
        assert_eq!(
            summary(
                "app.py",
                PYTHON_BASE,
                &hello("return \"hi\""),
                &hello("return \"hey\"")
            ),
            [diverged(
                "class Greeter > def hello",
                Divergence::BothModified
            )]
        );
    }

    #[test]
    fn reports_python_additions_and_deletions() {
        let added = |value: &str| format!("{PYTHON_BASE}\n\ndef gamma():\n    return {value}\n");
        let deleted = PYTHON_BASE.replace("def alpha():\n    return 1\n\n\n", "");
        let modified = PYTHON_BASE.replace("return 1", "return 10");

        assert_eq!(
            summary("app.py", PYTHON_BASE, &added("3"), &added("4")),
            [diverged("def gamma", Divergence::BothAdded)]
        );
        assert_eq!(summary("app.py", PYTHON_BASE, &added("3"), &added("3")), []);
        assert_eq!(
            summary("app.py", PYTHON_BASE, &deleted, &modified),
            [diverged(
                "def alpha",
                Divergence::DeletedInOursModifiedInTheirs
            )]
        );
        assert_eq!(
            summary("app.py", PYTHON_BASE, &modified, &deleted),
            [diverged(
                "def alpha",
                Divergence::ModifiedInOursDeletedInTheirs
            )]
        );
        assert_eq!(summary("app.py", PYTHON_BASE, &deleted, &deleted), []);
    }

    #[test]
    fn keeps_python_decorated_definitions_apart_by_position() {
        let base = "\
class Box:
    @property
    def size(self):
        return self._size

    @size.setter
    def size(self, value):
        self._size = value
";
        let setter = |value: &str| base.replace("self._size = value", value);
        let getter = base.replace("return self._size", "return self._size or 0");
        let ours = setter("self._size = value * 2");

        let analysis =
            analyze_text("box.py", base, &ours, &setter("self._size = abs(value)")).unwrap();
        assert_eq!(analysis.diverged_items.len(), 1);
        assert_eq!(analysis.diverged_items[0].item, "class Box > def size #2");
        assert_eq!(
            analysis.diverged_items[0].ours,
            Some(location(
                &ours,
                "@size.setter\n    def size(self, value):\n        self._size = value * 2",
                "decorated_definition",
                6..=8
            ))
        );
        assert_eq!(summary("box.py", base, &getter, &ours), []);
    }

    #[test]
    fn ignores_python_formatting_only_edits() {
        let base = "def alpha(a, b):\n    return a + b\n";
        let reformatted = "def alpha(\n  a,\n  b,\n):\n  return a+b\n";
        let edited = base.replace("a + b", "a * b");

        assert_eq!(summary("app.py", base, reformatted, &edited), []);
    }
}
