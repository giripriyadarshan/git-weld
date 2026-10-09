//! The provider-agnostic semantic state: a neutral, deterministic description
//! of every item both sides changed, as plain text ([`fmt::Display`]) or as
//! JSON ([`Serialize`]).

use crate::{
    ast::{Analysis, Conflict, ConflictKind, Language, Location},
    merge::Versions,
};
use serde::Serialize;
use similar::TextDiff;
use std::{
    fmt,
    ops::{Range, RangeInclusive},
    path::Path,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SemanticState {
    /// The logical path of the merged file (`%P`).
    pub file: String,
    pub language: Language,
    pub conflicts: Vec<ConflictState>,
}

/// One conflicting item. A missing version (`null` in JSON) means the item
/// does not exist on that side: it was added on both sides or deleted on one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConflictState {
    /// Path of the item, such as `impl Counter > fn get`.
    pub item: String,
    /// The tree-sitter node type, such as `function_item`.
    pub node_type: String,
    /// One neutral sentence per version saying what happened to the item.
    pub summary: String,
    pub base: Option<Version>,
    pub ours: Option<Version>,
    pub theirs: Option<Version>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Version {
    #[serde(skip)]
    pub lines: RangeInclusive<usize>,
    /// The item's code, including its comments, attributes, and decorators,
    /// with the common indentation removed and `\n` line endings.
    pub code: String,
    /// Unified diff from the base version, when both versions exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<String>,
}

impl SemanticState {
    pub fn new(path: &Path, analysis: &Analysis, versions: &Versions) -> Self {
        Self {
            file: path.display().to_string(),
            language: analysis.language,
            conflicts: analysis
                .conflicts
                .iter()
                .map(|conflict| ConflictState::new(conflict, versions))
                .collect(),
        }
    }
}

impl ConflictState {
    fn new(conflict: &Conflict, versions: &Versions) -> Self {
        let base = conflict
            .base
            .as_ref()
            .map(|location| Version::new(&versions.base, location, None));
        let side = |source: &[u8], location: &Option<Location>| {
            location
                .as_ref()
                .map(|location| Version::new(source, location, base.as_ref()))
        };
        let ours = side(&versions.ours, &conflict.ours);
        let theirs = side(&versions.theirs, &conflict.theirs);
        let node_type = [&conflict.base, &conflict.ours, &conflict.theirs]
            .into_iter()
            .flatten()
            .next()
            .map(|location| location.node_kind.clone())
            .unwrap_or_default();
        Self {
            summary: summary(conflict.kind, &node_type, &conflict.item),
            item: conflict.item.clone(),
            node_type,
            base,
            ours,
            theirs,
        }
    }
}

impl Version {
    fn new(source: &[u8], location: &Location, base: Option<&Version>) -> Self {
        let code = snippet(source, &location.bytes);
        let changes = base
            .map(|base| diff(&base.code, &code))
            .filter(|changes| !changes.is_empty());
        Self {
            lines: location.lines.clone(),
            code,
            changes,
        }
    }
}

// Modeled on the blueprint's example: "Node type `struct_item` 'User' in
// Base had field 'x'. Branch A modified ... Branch B removed ...".
fn summary(kind: ConflictKind, node_type: &str, item: &str) -> String {
    let (base, ours, theirs) = match kind {
        ConflictKind::BothModified => ("exists in base", "modified it", "modified it differently"),
        ConflictKind::BothAdded => (
            "does not exist in base",
            "added it",
            "added it with different content",
        ),
        ConflictKind::ModifiedInOursDeletedInTheirs => {
            ("exists in base", "modified it", "deleted it")
        }
        ConflictKind::DeletedInOursModifiedInTheirs => {
            ("exists in base", "deleted it", "modified it")
        }
    };
    format!("Node type `{node_type}` `{item}` {base}. Ours {ours}. Theirs {theirs}.")
}

// Starts at the beginning of the item's first line when only indentation
// precedes the item, so every line keeps its indentation before dedenting.
fn snippet(source: &[u8], bytes: &Range<usize>) -> String {
    let end = bytes.end.min(source.len());
    let start = bytes.start.min(end);
    let line_start = source[..start]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |newline| newline + 1);
    let start = if source[line_start..start]
        .iter()
        .all(|&byte| byte == b' ' || byte == b'\t')
    {
        line_start
    } else {
        start
    };
    dedent(&String::from_utf8_lossy(&source[start..end]).replace("\r\n", "\n"))
}

fn dedent(text: &str) -> String {
    let indentation = |line: &str| {
        let content = line.trim_start_matches([' ', '\t']);
        line.len() - content.len()
    };
    let common = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| &line[..indentation(line)])
        .reduce(|common, indent| {
            let shared = common
                .bytes()
                .zip(indent.bytes())
                .take_while(|(left, right)| left == right)
                .count();
            &common[..shared]
        })
        .unwrap_or_default();
    text.lines()
        .map(|line| {
            line.strip_prefix(common)
                .unwrap_or_else(|| line.trim_start_matches([' ', '\t']))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn diff(old: &str, new: &str) -> String {
    let (old, new) = (format!("{old}\n"), format!("{new}\n"));
    let diff = TextDiff::from_lines(&old, &new);
    let mut unified = diff.unified_diff();
    unified.context_radius(3);
    unified.to_string().trim_end_matches('\n').to_owned()
}

impl fmt::Display for SemanticState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.conflicts.len();
        writeln!(
            formatter,
            "File `{}` ({}): {count} {} changed on both sides.",
            self.file,
            self.language,
            if count == 1 { "item" } else { "items" }
        )?;
        for (index, conflict) in self.conflicts.iter().enumerate() {
            writeln!(formatter)?;
            writeln!(
                formatter,
                "Item {} of {count}: {}",
                index + 1,
                conflict.summary
            )?;
            for (side, version) in [
                ("Base", &conflict.base),
                ("Ours", &conflict.ours),
                ("Theirs", &conflict.theirs),
            ] {
                let Some(version) = version else {
                    writeln!(formatter, "{side}: not present")?;
                    continue;
                };
                let (first, last) = (version.lines.start(), version.lines.end());
                if first == last {
                    writeln!(formatter, "{side}, line {first}:")?;
                } else {
                    writeln!(formatter, "{side}, lines {first}-{last}:")?;
                }
                write_indented(formatter, &version.code)?;
                if let Some(changes) = &version.changes {
                    writeln!(formatter, "{side} changes from base:")?;
                    write_indented(formatter, changes)?;
                }
            }
        }
        Ok(())
    }
}

fn write_indented(formatter: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    for line in text.lines() {
        if line.is_empty() {
            writeln!(formatter)?;
        } else {
            writeln!(formatter, "    {line}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use serde_json::json;

    const BASE: &str = "\
struct Counter {
    value: u32,
}

impl Counter {
    /// Returns the value.
    fn get(&self) -> u32 {
        self.value
    }
}
";

    fn state(path: &str, base: &str, ours: &str, theirs: &str) -> SemanticState {
        let versions = Versions {
            base: base.as_bytes().to_vec(),
            ours: ours.as_bytes().to_vec(),
            theirs: theirs.as_bytes().to_vec(),
        };
        let analysis = ast::analyze(
            Path::new(path),
            &versions.base,
            &versions.ours,
            &versions.theirs,
        )
        .unwrap();
        SemanticState::new(Path::new(path), &analysis, &versions)
    }

    #[test]
    fn describes_an_item_modified_on_both_sides_as_plain_text() {
        let ours = BASE.replace("        self.value\n", "        self.value + 1\n");
        let theirs = BASE.replace(
            "        self.value\n",
            "        // Never zero.\n        self.value.max(1)\n",
        );

        let state = state("src/counter.rs", BASE, &ours, &theirs);

        assert_eq!(
            state.to_string(),
            "\
File `src/counter.rs` (Rust): 1 item changed on both sides.

Item 1 of 1: Node type `function_item` `impl Counter > fn get` exists in base. Ours modified it. Theirs modified it differently.
Base, lines 6-9:
    /// Returns the value.
    fn get(&self) -> u32 {
        self.value
    }
Ours, lines 6-9:
    /// Returns the value.
    fn get(&self) -> u32 {
        self.value + 1
    }
Ours changes from base:
    @@ -1,4 +1,4 @@
     /// Returns the value.
     fn get(&self) -> u32 {
    -    self.value
    +    self.value + 1
     }
Theirs, lines 6-10:
    /// Returns the value.
    fn get(&self) -> u32 {
        // Never zero.
        self.value.max(1)
    }
Theirs changes from base:
    @@ -1,4 +1,5 @@
     /// Returns the value.
     fn get(&self) -> u32 {
    -    self.value
    +    // Never zero.
    +    self.value.max(1)
     }
"
        );
    }

    #[test]
    fn serializes_the_same_description_as_json() {
        let ours = BASE.replace("        self.value\n", "        self.value + 1\n");
        let theirs = BASE.replace("        self.value\n", "        self.value * 2\n");

        let state = state("src/counter.rs", BASE, &ours, &theirs);

        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            json!({
                "file": "src/counter.rs",
                "language": "Rust",
                "conflicts": [{
                    "item": "impl Counter > fn get",
                    "node_type": "function_item",
                    "summary": "Node type `function_item` `impl Counter > fn get` exists in base. \
                                Ours modified it. Theirs modified it differently.",
                    "base": {
                        "code": "/// Returns the value.\nfn get(&self) -> u32 {\n    self.value\n}",
                    },
                    "ours": {
                        "code": "/// Returns the value.\nfn get(&self) -> u32 {\n    self.value + 1\n}",
                        "changes": "@@ -1,4 +1,4 @@\n /// Returns the value.\n fn get(&self) -> u32 {\n\
                                    -    self.value\n+    self.value + 1\n }",
                    },
                    "theirs": {
                        "code": "/// Returns the value.\nfn get(&self) -> u32 {\n    self.value * 2\n}",
                        "changes": "@@ -1,4 +1,4 @@\n /// Returns the value.\n fn get(&self) -> u32 {\n\
                                    -    self.value\n+    self.value * 2\n }",
                    },
                }],
            })
        );
    }

    #[test]
    fn describes_additions_and_deletions_neutrally() {
        let base = "def alpha():\n    return 1\n";
        let added = |value: &str| format!("{base}\n\ndef gamma():\n    return {value}\n");
        let modified = base.replace("return 1", "return 10");

        let both_added = state("app.py", base, &added("3"), &added("4"));
        let deleted_in_ours = state("app.py", base, "", &modified);
        let deleted_in_theirs = state("app.py", base, &modified, "");

        let conflict = &both_added.conflicts[0];
        assert_eq!(
            conflict.summary,
            "Node type `function_definition` `def gamma` does not exist in base. \
             Ours added it. Theirs added it with different content."
        );
        assert_eq!(conflict.base, None);
        assert_eq!(
            conflict.ours,
            Some(Version {
                lines: 5..=6,
                code: "def gamma():\n    return 3".to_owned(),
                changes: None,
            })
        );
        assert!(both_added.to_string().contains(
            "Item 1 of 1: Node type `function_definition` `def gamma` does not exist in base. \
             Ours added it. Theirs added it with different content.\n\
             Base: not present\n\
             Ours, lines 5-6:\n    def gamma():\n        return 3\n\
             Theirs, lines 5-6:\n    def gamma():\n        return 4\n"
        ));

        assert_eq!(
            deleted_in_ours.conflicts[0].summary,
            "Node type `function_definition` `def alpha` exists in base. \
             Ours deleted it. Theirs modified it."
        );
        assert_eq!(deleted_in_ours.conflicts[0].ours, None);
        assert!(
            deleted_in_ours
                .to_string()
                .contains("\nOurs: not present\n")
        );
        assert_eq!(
            deleted_in_theirs.conflicts[0].summary,
            "Node type `function_definition` `def alpha` exists in base. \
             Ours modified it. Theirs deleted it."
        );
        assert_eq!(
            serde_json::to_value(&deleted_in_theirs).unwrap()["conflicts"][0]["theirs"],
            json!(null)
        );
    }

    #[test]
    fn keeps_relative_indentation_and_normalizes_line_endings() {
        let base = "class Box:\r\n    def size(self):\r\n        if self.empty:\r\n            return 0\r\n        return self._size\r\n";
        let ours = base.replace("return 0", "return -1");
        let theirs = base.replace("return 0", "return None");

        let state = state("box.py", base, &ours, &theirs);

        let conflict = &state.conflicts[0];
        assert_eq!(conflict.item, "class Box > def size");
        assert_eq!(
            conflict.base.as_ref().unwrap().code,
            "def size(self):\n    if self.empty:\n        return 0\n    return self._size"
        );
        assert_eq!(
            conflict.ours.as_ref().unwrap().changes.as_deref(),
            Some(
                "@@ -1,4 +1,4 @@\n def size(self):\n     if self.empty:\n\
                 -        return 0\n+        return -1\n     return self._size"
            )
        );
    }

    #[test]
    fn keeps_items_in_analysis_order_and_is_deterministic() {
        let base = "fn alpha() -> u32 {\n    1\n}\n\nfn beta() -> u32 {\n    2\n}\n";
        let ours = base
            .replace("    1\n", "    10\n")
            .replace("    2\n", "    20\n");
        let theirs = base
            .replace("    1\n", "    100\n")
            .replace("    2\n", "    200\n");

        let first = state("lib.rs", base, &ours, &theirs);
        let second = state("lib.rs", base, &ours, &theirs);

        assert_eq!(first, second);
        assert_eq!(first.to_string(), second.to_string());
        let items: Vec<_> = first
            .conflicts
            .iter()
            .map(|conflict| conflict.item.as_str())
            .collect();
        assert_eq!(items, ["fn alpha", "fn beta"]);
        assert!(
            first.to_string().starts_with(
                "File `lib.rs` (Rust): 2 items changed on both sides.\n\nItem 1 of 2: "
            )
        );
    }
}
