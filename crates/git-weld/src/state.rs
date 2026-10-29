//! The provider-agnostic semantic state: a neutral, deterministic description
//! of every chunk that both sides changed, as plain text ([`fmt::Display`]) or
//! as JSON ([`Serialize`]).

use crate::{
    ast::{Analysis, Language, Outline, OutlineItem, Segment, dedent},
    chunk::{Chunk, ChunkItem},
    merge::{LineMerge, Version},
};
use serde::Serialize;
use similar::TextDiff;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    ops::RangeInclusive,
    path::Path,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SemanticState {
    /// The logical path of the merged file (`%P`).
    pub file: String,
    pub language: Language,
    pub chunks: Vec<ChunkState>,
    /// Signatures of items in the same file that the chunks reference.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<SignatureState>,
}

/// The signature of an item referenced by one or more chunks: once when all
/// three versions agree, or once per version when they differ.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SignatureState {
    /// Human-readable path, such as `struct User` or `fn helper`.
    pub item: String,
    /// The signature when it is identical in base, ours, and theirs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// The base signature when the versions differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Our signature when the versions differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours: Option<String>,
    /// Their signature when the versions differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
    /// Indices in [`SemanticState::chunks`] that reference this item.
    #[serde(skip)]
    pub chunks: Vec<usize>,
}

/// One chunk: whole items that both sides changed. A missing version (`null`
/// in JSON) means the chunk is empty in that version: its items were added on
/// both sides or deleted on one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChunkState {
    /// The paths of the chunk's items, such as `impl Counter > fn get`: those
    /// of base, then those that only ours or theirs has.
    pub items: Vec<String>,
    /// One neutral sentence per version saying what happened to the chunk.
    pub summary: String,
    pub base: Option<VersionState>,
    pub ours: Option<VersionState>,
    pub theirs: Option<VersionState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VersionState {
    #[serde(skip)]
    pub lines: RangeInclusive<usize>,
    /// The chunk's code, including comments, attributes, and decorators,
    /// without blank lines around it, with the common indentation removed and
    /// `\n` line endings.
    pub code: String,
    /// Unified diff from the base version, when both versions exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<String>,
}

impl SemanticState {
    /// Describes `chunks` of the merge of the file at `path`, including the
    /// signatures of any items in the same file that `chunks` reference.
    pub fn new(path: &Path, analysis: &Analysis, merge: &LineMerge, chunks: &[&Chunk]) -> Self {
        Self {
            file: path.display().to_string(),
            language: analysis.language,
            chunks: chunks
                .iter()
                .map(|chunk| ChunkState::new(merge, chunk))
                .collect(),
            signatures: hoist_signatures(analysis, merge, chunks),
        }
    }

    /// The hoisted signatures referenced by `chunk` (an index into
    /// [`Self::chunks`]), in state order.
    pub fn signatures_for_chunk(&self, chunk: usize) -> impl Iterator<Item = &SignatureState> {
        self.signatures
            .iter()
            .filter(move |signature| signature.chunks.contains(&chunk))
    }

    /// Builds a state with only the chunks at `indices` and the signatures
    /// those chunks reference.
    pub fn select_chunks(&self, indices: &[usize]) -> Self {
        let chunks = indices
            .iter()
            .filter_map(|&index| self.chunks.get(index).cloned())
            .collect();
        let signatures = self
            .signatures
            .iter()
            .filter_map(|signature| {
                let remapped: Vec<usize> = indices
                    .iter()
                    .enumerate()
                    .filter_map(|(new_index, &old_index)| {
                        signature.chunks.contains(&old_index).then_some(new_index)
                    })
                    .collect();
                (!remapped.is_empty()).then(|| SignatureState {
                    chunks: remapped,
                    ..signature.clone()
                })
            })
            .collect();
        Self {
            file: self.file.clone(),
            language: self.language,
            chunks,
            signatures,
        }
    }
}

fn hoist_signatures(
    analysis: &Analysis,
    merge: &LineMerge,
    chunks: &[&Chunk],
) -> Vec<SignatureState> {
    let outlines = Version::ALL.map(|version| analysis.outline(version));
    let by_key: [HashMap<&[Segment], &OutlineItem>; 3] = outlines.map(|outline| {
        outline
            .items()
            .iter()
            .skip(1)
            .map(|item| (item.key.as_slice(), item))
            .collect()
    });

    let mut ordered_keys: Vec<&[Segment]> = Vec::new();
    let mut seen_keys: HashSet<&[Segment]> = HashSet::new();
    for outline in outlines {
        for item in outline.items().iter().skip(1) {
            if item.signature.is_some() && seen_keys.insert(item.key.as_slice()) {
                ordered_keys.push(item.key.as_slice());
            }
        }
    }
    if ordered_keys.is_empty() {
        return Vec::new();
    }

    let mut referencing: HashMap<&[Segment], Vec<usize>> = HashMap::new();
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        let chunk_paths = chunk.item_paths();
        let mut covered: HashSet<&[Segment]> = HashSet::new();
        let mut names: HashSet<&str> = HashSet::new();

        for version in Version::ALL {
            let outline = analysis.outline(version);
            let span = merge.span_bytes(version, chunk.regions.clone());
            for name in outline.referenced_names(span.clone()) {
                names.insert(name);
            }
            for item in outline.items().iter().skip(1) {
                let inside_span = !span.is_empty()
                    && !item.bytes.is_empty()
                    && span.start <= item.bytes.start
                    && item.bytes.end <= span.end;
                let encloses_span = item.container
                    && !span.is_empty()
                    && !item.bytes.is_empty()
                    && item.bytes.start <= span.start
                    && span.end <= item.bytes.end;
                if inside_span || encloses_span || chunk_paths.contains(&item.path) {
                    covered.insert(item.key.as_slice());
                }
            }
        }
        if names.is_empty() {
            continue;
        }

        let matched: HashSet<&[Segment]> = ordered_keys
            .iter()
            .copied()
            .filter(|&key| !covered.contains(key))
            .filter(|&key| {
                by_key.iter().any(|version_map| {
                    version_map.get(key).is_some_and(|item| {
                        item.signature.is_some()
                            && item
                                .name
                                .as_deref()
                                .is_some_and(|name| names.contains(name))
                    })
                })
            })
            .collect();

        for &key in &matched {
            let parent_already_hoisted =
                by_key.iter().zip(outlines).any(|(version_map, outline)| {
                    version_map
                        .get(key)
                        .and_then(|item| item.parent)
                        .filter(|&parent| parent != Outline::ROOT)
                        .map(|parent| outline.item(parent))
                        .is_some_and(|parent| {
                            parent.signature.is_some() && matched.contains(parent.key.as_slice())
                        })
                });
            if !parent_already_hoisted {
                referencing.entry(key).or_default().push(chunk_index);
            }
        }
    }

    ordered_keys
        .into_iter()
        .filter_map(|key| {
            let chunks = referencing.remove(key)?;
            let base_item = by_key[Version::Base as usize].get(key).copied();
            let ours_item = by_key[Version::Ours as usize].get(key).copied();
            let theirs_item = by_key[Version::Theirs as usize].get(key).copied();
            let item = ours_item
                .or(theirs_item)
                .or(base_item)
                .map(|item| item.path.clone())?;
            let base = base_item.and_then(|item| item.signature.clone());
            let ours = ours_item.and_then(|item| item.signature.clone());
            let theirs = theirs_item.and_then(|item| item.signature.clone());
            if base.is_some() && base == ours && base == theirs {
                Some(SignatureState {
                    item,
                    signature: base,
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks,
                })
            } else {
                Some(SignatureState {
                    item,
                    signature: None,
                    base,
                    ours,
                    theirs,
                    chunks,
                })
            }
        })
        .collect()
}

impl ChunkState {
    fn new(merge: &LineMerge, chunk: &Chunk) -> Self {
        let base = VersionState::new(merge, chunk, Version::Base, None);
        let ours = VersionState::new(merge, chunk, Version::Ours, base.as_ref());
        let theirs = VersionState::new(merge, chunk, Version::Theirs, base.as_ref());
        let present = [base.is_some(), ours.is_some(), theirs.is_some()];
        Self {
            summary: summary(Version::ALL.map(|version| chunk.items(version)), present),
            items: chunk.item_paths(),
            base,
            ours,
            theirs,
        }
    }
}

impl VersionState {
    fn new(
        merge: &LineMerge,
        chunk: &Chunk,
        version: Version,
        base: Option<&VersionState>,
    ) -> Option<Self> {
        let lines = chunk.lines(merge, version);
        let blank = |line: usize| {
            merge
                .slice(version, line..line + 1)
                .iter()
                .all(u8::is_ascii_whitespace)
        };
        let first = lines.clone().find(|&line| !blank(line))?;
        let last = lines.rev().find(|&line| !blank(line))?;
        let text = String::from_utf8_lossy(merge.slice(version, first..last + 1));
        let code = dedent(&text.replace("\r\n", "\n"));
        let changes = base
            .map(|base| diff(&base.code, &code))
            .filter(|changes| !changes.is_empty());
        Some(Self {
            lines: first + 1..=last + 1,
            code,
            changes,
        })
    }
}

// Modeled on the blueprint's example: "Node type `struct_item` 'User' in
// Base had field 'x'. Branch A modified ... Branch B removed ...".
fn summary(items: [&[ChunkItem]; 3], present: [bool; 3]) -> String {
    fn paths(items: &[ChunkItem]) -> Vec<&str> {
        items.iter().map(|item| item.path.as_str()).collect()
    }
    let mut versions = (0..3)
        .filter(|&version| present[version])
        .map(|version| paths(items[version]));
    let first = versions.next().unwrap_or_default();
    if first.is_empty() || versions.any(|other| other != first) {
        let list = |version: usize| {
            if present[version] && !items[version].is_empty() {
                listing(items[version], |item| {
                    format!("`{}` (`{}`)", item.path, item.node_kind)
                })
            } else {
                "nothing".to_owned()
            }
        };
        return format!(
            "The chunk holds different items in each version. Base: {}. Ours: {}. Theirs: {}.",
            list(0),
            list(1),
            list(2)
        );
    }

    let subject = items[(0..3).find(|&version| present[version]).unwrap_or_default()];
    let plural = subject.len() > 1;
    let it = if plural { "them" } else { "it" };
    let base = match (present[0], plural) {
        (true, false) => "exists in base",
        (true, true) => "exist in base",
        (false, false) => "does not exist in base",
        (false, true) => "do not exist in base",
    };
    let action = |side_present: bool| match (present[0], side_present) {
        (true, true) => "modified",
        (true, false) => "deleted",
        (false, true) => "added",
        (false, false) => "did not add",
    };
    let (ours, theirs) = (action(present[1]), action(present[2]));
    let theirs = match theirs {
        "modified" if ours == "modified" => format!("modified {it} differently"),
        "added" if ours == "added" => format!("added {it} with different content"),
        theirs => format!("{theirs} {it}"),
    };
    let noun = if plural { "Node types" } else { "Node type" };
    let subject = listing(subject, |item| {
        format!("`{}` `{}`", item.node_kind, item.path)
    });
    format!("{noun} {subject} {base}. Ours {ours} {it}. Theirs {theirs}.")
}

fn listing(items: &[ChunkItem], describe: impl Fn(&ChunkItem) -> String) -> String {
    let described: Vec<String> = items.iter().map(describe).collect();
    match described.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
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
        let count = self.chunks.len();
        writeln!(
            formatter,
            "File `{}` ({}): {count} {} changed on both sides.",
            self.file,
            self.language,
            if count == 1 { "chunk" } else { "chunks" }
        )?;
        for (index, chunk) in self.chunks.iter().enumerate() {
            writeln!(formatter)?;
            writeln!(
                formatter,
                "Chunk {} of {count}: {}",
                index + 1,
                chunk.summary
            )?;
            for (label, version) in [
                ("Base", &chunk.base),
                ("Ours", &chunk.ours),
                ("Theirs", &chunk.theirs),
            ] {
                let Some(version) = version else {
                    writeln!(formatter, "{label}: not present")?;
                    continue;
                };
                let (first, last) = (version.lines.start(), version.lines.end());
                if first == last {
                    writeln!(formatter, "{label}, line {first}:")?;
                } else {
                    writeln!(formatter, "{label}, lines {first}-{last}:")?;
                }
                write_indented(formatter, &version.code)?;
                if let Some(changes) = &version.changes {
                    writeln!(formatter, "{label} changes from base:")?;
                    write_indented(formatter, changes)?;
                }
            }
        }
        for signature in &self.signatures {
            writeln!(formatter)?;
            writeln!(formatter, "Referenced signature `{}`:", signature.item)?;
            if let Some(code) = &signature.signature {
                write_indented(formatter, code)?;
            } else {
                for (label, version) in [
                    ("Base", &signature.base),
                    ("Ours", &signature.ours),
                    ("Theirs", &signature.theirs),
                ] {
                    match version {
                        Some(code) => {
                            writeln!(formatter, "{label}:")?;
                            write_indented(formatter, code)?;
                        }
                        None => writeln!(formatter, "{label}: not present")?,
                    }
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
    use crate::{ast, chunk, merge::Versions};
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
        let merge = LineMerge::new(&versions).unwrap();
        let analysis = ast::analyze(
            Path::new(path),
            &versions.base,
            &versions.ours,
            &versions.theirs,
        )
        .unwrap();
        let chunks = chunk::chunks(&merge, Some(&analysis));
        let decidable: Vec<&Chunk> = chunks.iter().filter(|chunk| chunk.decidable).collect();
        SemanticState::new(Path::new(path), &analysis, &merge, &decidable)
    }

    #[test]
    fn describes_a_chunk_modified_on_both_sides_as_plain_text() {
        let ours = BASE.replace("        self.value\n", "        self.value + 1\n");
        let theirs = BASE.replace(
            "        self.value\n",
            "        // Never zero.\n        self.value.max(1)\n",
        );

        let state = state("src/counter.rs", BASE, &ours, &theirs);

        assert_eq!(
            state.to_string(),
            "\
File `src/counter.rs` (Rust): 1 chunk changed on both sides.

Chunk 1 of 1: Node type `function_item` `impl Counter > fn get` exists in base. Ours modified it. Theirs modified it differently.
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
                "chunks": [{
                    "items": ["impl Counter > fn get"],
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

        let chunk = &both_added.chunks[0];
        assert_eq!(chunk.items, ["def gamma"]);
        assert_eq!(
            chunk.summary,
            "Node type `function_definition` `def gamma` does not exist in base. \
             Ours added it. Theirs added it with different content."
        );
        assert_eq!(chunk.base, None);
        assert_eq!(
            chunk.ours,
            Some(VersionState {
                lines: 5..=6,
                code: "def gamma():\n    return 3".to_owned(),
                changes: None,
            })
        );
        assert!(both_added.to_string().contains(
            "Chunk 1 of 1: Node type `function_definition` `def gamma` does not exist in base. \
             Ours added it. Theirs added it with different content.\n\
             Base: not present\n\
             Ours, lines 5-6:\n    def gamma():\n        return 3\n\
             Theirs, lines 5-6:\n    def gamma():\n        return 4\n"
        ));

        assert_eq!(
            deleted_in_ours.chunks[0].summary,
            "Node type `function_definition` `def alpha` exists in base. \
             Ours deleted it. Theirs modified it."
        );
        assert_eq!(deleted_in_ours.chunks[0].ours, None);
        assert!(
            deleted_in_ours
                .to_string()
                .contains("\nOurs: not present\n")
        );
        assert_eq!(
            deleted_in_theirs.chunks[0].summary,
            "Node type `function_definition` `def alpha` exists in base. \
             Ours modified it. Theirs deleted it."
        );
        assert_eq!(
            serde_json::to_value(&deleted_in_theirs).unwrap()["chunks"][0]["theirs"],
            json!(null)
        );
    }

    #[test]
    fn keeps_relative_indentation_and_normalizes_line_endings() {
        let base = "class Box:\r\n    def size(self):\r\n        if self.empty:\r\n            return 0\r\n        return self._size\r\n";
        let ours = base.replace("return 0", "return -1");
        let theirs = base.replace("return 0", "return None");

        let state = state("box.py", base, &ours, &theirs);

        let chunk = &state.chunks[0];
        assert_eq!(chunk.items, ["class Box > def size"]);
        assert_eq!(
            chunk.base.as_ref().unwrap().code,
            "def size(self):\n    if self.empty:\n        return 0\n    return self._size"
        );
        assert_eq!(
            chunk.ours.as_ref().unwrap().changes.as_deref(),
            Some(
                "@@ -1,4 +1,4 @@\n def size(self):\n     if self.empty:\n\
                 -        return 0\n+        return -1\n     return self._size"
            )
        );
    }

    #[test]
    fn keeps_chunks_in_file_order_and_is_deterministic() {
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
            .chunks
            .iter()
            .map(|chunk| chunk.items.clone())
            .collect();
        assert_eq!(items, [["fn alpha"], ["fn beta"]]);
        assert!(first.to_string().starts_with(
            "File `lib.rs` (Rust): 2 chunks changed on both sides.\n\nChunk 1 of 2: "
        ));
    }

    #[test]
    fn describes_chunks_of_several_items_and_items_that_differ_between_versions() {
        let base = "fn alpha() -> u32 { 1 }\nfn beta() -> u32 { 2 }\n";
        let ours = base.replace("{ 1 }", "{ 10 }").replace("{ 2 }", "{ 20 }");
        let theirs = base.replace("{ 1 }", "{ 11 }").replace("{ 2 }", "{ 22 }");
        assert_eq!(
            state("lib.rs", base, &ours, &theirs).chunks[0].summary,
            "Node types `function_item` `fn alpha` and `function_item` `fn beta` exist in \
             base. Ours modified them. Theirs modified them differently."
        );

        let renamed = |name: &str| {
            base.replace(
                "fn beta() -> u32 { 2 }",
                &format!("fn {name}() -> u32 {{ 2 }}"),
            )
        };
        let chunk = &state("lib.rs", base, &renamed("gamma"), &renamed("delta")).chunks[0];
        assert_eq!(chunk.items, ["fn beta", "fn gamma", "fn delta"]);
        assert_eq!(
            chunk.summary,
            "The chunk holds different items in each version. Base: `fn beta` \
             (`function_item`). Ours: `fn gamma` (`function_item`). Theirs: `fn delta` \
             (`function_item`)."
        );
    }

    #[test]
    fn leaves_blank_lines_around_a_chunk_out_of_its_code() {
        let base = "def alpha():\n    return 1\n";
        let added = |value: &str| format!("{base}\n\ndef gamma():\n    return {value}\n\n\n");

        let state = state("app.py", base, &added("3"), &added("4"));

        let ours = state.chunks[0].ours.as_ref().unwrap();
        assert_eq!(ours.code, "def gamma():\n    return 3");
        assert_eq!(ours.lines, 5..=6);
    }

    #[test]
    fn hoists_referenced_rust_signatures_once_per_state_and_tracks_changed_types() {
        let base = "\
struct Id(u64);

struct User {
    x: u32,
}

trait Store {
    fn load(&self, id: u64) -> User;
    fn default_x(&self) -> u32 {
        0
    }
}

/// Doubles `value`.
fn helper(value: u32) -> u32 {
    let _ = Id(1);
    value * 2
}

fn first(user: &User, store: &dyn Store) -> u32 {
    let _ = store;
    helper(user.x)
}

fn second(user: &User) -> u32 {
    helper(user.x + 1)
}
";
        let ours = base
            .replace("x: u32,", "x: i32,")
            .replace("helper(user.x)", "helper(user.x as u32 + 10)")
            .replace("helper(user.x + 1)", "helper(user.x as u32 + 20)");
        let theirs = base
            .replace("helper(user.x)", "helper(user.x + 100)")
            .replace("helper(user.x + 1)", "helper(user.x + 200)");

        let state = state("src/lib.rs", base, &ours, &theirs);

        // `Id` is referenced only inside `helper`, so one-level lookup leaves it out.
        assert_eq!(
            state.signatures,
            [
                SignatureState {
                    item: "struct User".to_owned(),
                    signature: None,
                    base: Some("struct User {\n    x: u32,\n}".to_owned()),
                    ours: Some("struct User {\n    x: i32,\n}".to_owned()),
                    theirs: Some("struct User {\n    x: u32,\n}".to_owned()),
                    chunks: vec![0, 1],
                },
                SignatureState {
                    item: "trait Store".to_owned(),
                    signature: Some(
                        "trait Store {\n    fn load(&self, id: u64) -> User;\n    \
                         fn default_x(&self) -> u32;\n}"
                            .to_owned()
                    ),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0],
                },
                SignatureState {
                    item: "fn helper".to_owned(),
                    signature: Some(
                        "/// Doubles `value`.\nfn helper(value: u32) -> u32".to_owned()
                    ),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0, 1],
                },
            ]
        );
        let chunk_1_items: Vec<&str> = state
            .signatures_for_chunk(1)
            .map(|sig| sig.item.as_str())
            .collect();
        assert_eq!(chunk_1_items, ["struct User", "fn helper"]);
        assert!(state.to_string().contains(
            "Referenced signature `struct User`:\n\
             Base:\n    struct User {\n        x: u32,\n    }\n\
             Ours:\n    struct User {\n        x: i32,\n    }\n\
             Theirs:\n    struct User {\n        x: u32,\n    }\n\n\
             Referenced signature `trait Store`:\n    \
             trait Store {\n        fn load(&self, id: u64) -> User;\n        \
             fn default_x(&self) -> u32;\n    }\n\n\
             Referenced signature `fn helper`:\n    \
             /// Doubles `value`.\n    fn helper(value: u32) -> u32\n"
        ));
    }

    #[test]
    fn hoists_referenced_rust_impl_mod_and_extern_container_signatures() {
        let base = "\
struct Counter {
    value: u32,
}

impl Counter {
    fn step(&self) -> u32 {
        self.value + 1
    }
}

mod math {
    pub const SCALE: u32 = 2;
    pub fn scale(x: u32) -> u32 {
        x * SCALE
    }
}

extern \"C\" {
    fn c_hash(x: u32) -> u32;
}

fn compute(counter: &Counter) -> u32 {
    unsafe { c_hash(math::scale(counter.step())) }
}
";
        let ours = base.replace("math::scale(counter.step())", "math::scale(counter.value)");
        let theirs = base.replace(
            "math::scale(counter.step())",
            "math::scale(counter.step() + 1)",
        );

        let state = state("src/lib.rs", base, &ours, &theirs);

        assert_eq!(
            state.signatures,
            [
                SignatureState {
                    item: "struct Counter".to_owned(),
                    signature: Some("struct Counter {\n    value: u32,\n}".to_owned()),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0],
                },
                SignatureState {
                    item: "impl Counter".to_owned(),
                    signature: Some("impl Counter {\n    fn step(&self) -> u32;\n}".to_owned()),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0],
                },
                SignatureState {
                    item: "mod math".to_owned(),
                    signature: Some(
                        "mod math {\n    pub const SCALE: u32 = 2;\n    \
                         pub fn scale(x: u32) -> u32;\n}"
                            .to_owned()
                    ),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0],
                },
                SignatureState {
                    item: "extern \"C\" > fn c_hash".to_owned(),
                    signature: Some("fn c_hash(x: u32) -> u32;".to_owned()),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0],
                },
            ]
        );
    }

    #[test]
    fn hoists_referenced_python_class_and_function_signatures_one_level_deep() {
        let base = "\
def ignored_leaf(x: int) -> int:
    return x


def clamp(amount: int) -> int:
    return ignored_leaf(max(0, amount))


class Account:
    currency: str = \"USD\"

    def __init__(self, balance: int) -> None:
        self.balance = balance

    @property
    def available(self) -> int:
        return self.balance


def withdraw(account: Account, amount: int) -> int:
    return account.available - clamp(amount)
";
        let ours = base.replace(
            "return account.available - clamp(amount)",
            "return max(0, account.available - clamp(amount))",
        );
        let theirs = base
            .replace("currency: str = \"USD\"", "currency: str = \"EUR\"")
            .replace(
                "return account.available - clamp(amount)",
                "return account.available - clamp(amount + 1)",
            );

        let state = state("app.py", base, &ours, &theirs);

        assert_eq!(
            state.signatures,
            [
                SignatureState {
                    item: "def clamp".to_owned(),
                    signature: Some("def clamp(amount: int) -> int:".to_owned()),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0],
                },
                SignatureState {
                    item: "class Account".to_owned(),
                    signature: None,
                    base: Some(
                        "class Account:\n    currency: str = \"USD\"\n    \
                         def __init__(self, balance: int) -> None:\n    \
                         @property\n    def available(self) -> int:"
                            .to_owned()
                    ),
                    ours: Some(
                        "class Account:\n    currency: str = \"USD\"\n    \
                         def __init__(self, balance: int) -> None:\n    \
                         @property\n    def available(self) -> int:"
                            .to_owned()
                    ),
                    theirs: Some(
                        "class Account:\n    currency: str = \"EUR\"\n    \
                         def __init__(self, balance: int) -> None:\n    \
                         @property\n    def available(self) -> int:"
                            .to_owned()
                    ),
                    chunks: vec![0],
                },
            ]
        );
    }
}
