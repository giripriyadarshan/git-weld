mod items;

use items::{Item, Segment};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    ops::{Range, RangeInclusive},
    path::Path,
};
use tree_sitter::{Node, Parser, Point, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Base,
    Ours,
    Theirs,
}

impl fmt::Display for Side {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Base => "base",
            Self::Ours => "ours",
            Self::Theirs => "theirs",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisError {
    UnsupportedFileType,
    InvalidUtf8 {
        side: Side,
    },
    Syntax {
        side: Side,
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
            Self::InvalidUtf8 { side } => write!(formatter, "{side} is not valid UTF-8"),
            Self::Syntax { side, line, column } => write!(
                formatter,
                "{side} has a syntax error at line {line}, column {column}"
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
    pub conflicts: Vec<Conflict>,
}

/// An item that both sides changed relative to the base in different ways.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// Human-readable path, such as `impl Counter > fn get`.
    pub item: String,
    pub kind: ConflictKind,
    pub base: Option<Location>,
    pub ours: Option<Location>,
    pub theirs: Option<Location>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
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

impl fmt::Display for Conflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let change = match self.kind {
            ConflictKind::BothModified => "modified on both sides",
            ConflictKind::BothAdded => "added on both sides with different content",
            ConflictKind::ModifiedInOursDeletedInTheirs => "modified in ours, deleted in theirs",
            ConflictKind::DeletedInOursModifiedInTheirs => "deleted in ours, modified in theirs",
        };
        write!(formatter, "{}: {change} (", self.item)?;
        let mut separator = "";
        for (side, location) in [
            (Side::Base, &self.base),
            (Side::Ours, &self.ours),
            (Side::Theirs, &self.theirs),
        ] {
            if let Some(location) = location {
                let (first, last) = (location.lines.start(), location.lines.end());
                if first == last {
                    write!(formatter, "{separator}{side} line {first}")?;
                } else {
                    write!(formatter, "{separator}{side} lines {first}-{last}")?;
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

    let mut extract = |side: Side, source: &[u8]| -> Result<Vec<Item>, AnalysisError> {
        let (text, tree) = parse(&mut parser, language, side, source)?;
        Ok(items::extract(language, text, &tree))
    };
    let base = extract(Side::Base, base)?;
    let ours = extract(Side::Ours, ours)?;
    let theirs = extract(Side::Theirs, theirs)?;

    Ok(Analysis {
        language,
        conflicts: compare(&base, &ours, &theirs),
    })
}

fn parse<'source>(
    parser: &mut Parser,
    language: Language,
    side: Side,
    source: &'source [u8],
) -> Result<(&'source str, Tree), AnalysisError> {
    let text = std::str::from_utf8(source).map_err(|_| AnalysisError::InvalidUtf8 { side })?;
    let tree = parser
        .parse(text, None)
        .ok_or_else(|| AnalysisError::Parser {
            language,
            message: "parsing did not complete".to_owned(),
        })?;
    let root = tree.root_node();
    if root.has_error() {
        let point = first_error(root).unwrap_or_else(|| root.start_position());
        return Err(AnalysisError::Syntax {
            side,
            line: point.row + 1,
            column: point.column + 1,
        });
    }
    Ok((text, tree))
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

fn compare(base: &[Item], ours: &[Item], theirs: &[Item]) -> Vec<Conflict> {
    let (base_index, ours_index, theirs_index) = (index(base), index(ours), index(theirs));
    let mut seen = HashSet::new();
    let mut deleted: Vec<&[Segment]> = Vec::new();
    let mut conflicts = Vec::new();

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
                ConflictKind::BothModified
            }
            (None, Some(new_ours), Some(new_theirs))
                if new_ours.fingerprint != new_theirs.fingerprint =>
            {
                ConflictKind::BothAdded
            }
            (Some(_), Some(_), None) if adds_or_modifies(key, &base_index, ours) => {
                deleted.push(key);
                ConflictKind::ModifiedInOursDeletedInTheirs
            }
            (Some(_), None, Some(_)) if adds_or_modifies(key, &base_index, theirs) => {
                deleted.push(key);
                ConflictKind::DeletedInOursModifiedInTheirs
            }
            _ => continue,
        };
        let path = new_ours
            .or(new_theirs)
            .or(old)
            .map(|item| item.path.clone());
        conflicts.push(Conflict {
            item: path.unwrap_or_default(),
            kind,
            base: old.map(|item| item.location.clone()),
            ours: new_ours.map(|item| item.location.clone()),
            theirs: new_theirs.map(|item| item.location.clone()),
        });
    }
    conflicts
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

    fn summary(path: &str, base: &str, ours: &str, theirs: &str) -> Vec<(String, ConflictKind)> {
        analyze_text(path, base, ours, theirs)
            .unwrap()
            .conflicts
            .into_iter()
            .map(|conflict| (conflict.item, conflict.kind))
            .collect()
    }

    fn conflict(item: &str, kind: ConflictKind) -> (String, ConflictKind) {
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

        assert_eq!(error, AnalysisError::InvalidUtf8 { side: Side::Theirs });
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
                    side: Side::Ours,
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
                    side: Side::Base,
                    line: 1,
                    ..
                }
            ),
            "{error:?}"
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
            analysis.conflicts,
            [Conflict {
                item: "fn alpha".to_owned(),
                kind: ConflictKind::BothModified,
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
            analysis.conflicts[0].to_string(),
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
        assert_eq!(analysis.conflicts.len(), 1);
        let conflict = &analysis.conflicts[0];
        assert_eq!(conflict.item, "fn gamma");
        assert_eq!(conflict.kind, ConflictKind::BothAdded);
        assert_eq!(conflict.base, None);
        assert_eq!(conflict.ours.as_ref().unwrap().lines, 9..=11);
        assert_eq!(conflict.theirs.as_ref().unwrap().lines, 9..=11);
        assert_eq!(
            conflict.to_string(),
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
        assert_eq!(analysis.conflicts.len(), 1);
        assert_eq!(
            analysis.conflicts[0].kind,
            ConflictKind::DeletedInOursModifiedInTheirs
        );
        assert_eq!(analysis.conflicts[0].ours, None);
        assert_eq!(
            analysis.conflicts[0].to_string(),
            "fn alpha: deleted in ours, modified in theirs (base lines 1-3, theirs lines 1-3)"
        );

        assert_eq!(
            summary("lib.rs", RUST_BASE, &modified, &deleted),
            [conflict(
                "fn alpha",
                ConflictKind::ModifiedInOursDeletedInTheirs
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
            [conflict(
                "impl Counter > fn get",
                ConflictKind::BothModified
            )]
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
            [conflict(
                "impl Counter",
                ConflictKind::DeletedInOursModifiedInTheirs
            )]
        );
        assert_eq!(
            summary("lib.rs", base, &edited_methods, without_impl),
            [conflict(
                "impl Counter",
                ConflictKind::ModifiedInOursDeletedInTheirs
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
            analysis.conflicts,
            [Conflict {
                item: "def alpha".to_owned(),
                kind: ConflictKind::BothModified,
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
            [conflict(
                "class Greeter > def hello",
                ConflictKind::BothModified
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
            [conflict("def gamma", ConflictKind::BothAdded)]
        );
        assert_eq!(summary("app.py", PYTHON_BASE, &added("3"), &added("3")), []);
        assert_eq!(
            summary("app.py", PYTHON_BASE, &deleted, &modified),
            [conflict(
                "def alpha",
                ConflictKind::DeletedInOursModifiedInTheirs
            )]
        );
        assert_eq!(
            summary("app.py", PYTHON_BASE, &modified, &deleted),
            [conflict(
                "def alpha",
                ConflictKind::ModifiedInOursDeletedInTheirs
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
        assert_eq!(analysis.conflicts.len(), 1);
        assert_eq!(analysis.conflicts[0].item, "class Box > def size #2");
        assert_eq!(
            analysis.conflicts[0].ours,
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
