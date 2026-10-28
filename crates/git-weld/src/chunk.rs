//! AST chunking: every conflict of the line merge widens to the smallest
//! enclosing items, so it can be decided on its own and replaced whole.
//!
//! A chunk is a run of merge regions. Its ends are region boundaries, where
//! base, ours, and theirs line up, so replacing the chunk with one side's lines
//! keeps everything around it as merged. In every version the chunk covers
//! whole items: a conflict inside a function covers that function, a conflict
//! spanning two items covers both, and conflicts that end up in the same items
//! share one chunk. Members of containers (impl, trait, mod, and extern blocks
//! and classes) are chunked on their own unless the conflict touches the
//! container itself, such as its header.

use crate::{
    ast::{Analysis, Outline},
    merge::{LineMerge, Version},
};
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// The merge regions it covers; at least one is a conflict.
    pub regions: Range<usize>,
    /// How many conflicts it covers.
    pub conflicts: usize,
    /// The outermost items it covers in base, ours, and theirs.
    pub items: [Vec<ChunkItem>; 3],
    /// Whether a provider may decide it. A chunk that covers no item, or that
    /// touches text outside every item (such as a comment at the end of the
    /// file), keeps its conflict markers.
    pub decidable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkItem {
    /// Human-readable path, such as `impl Counter > fn get`.
    pub path: String,
    pub node_kind: String,
}

impl Chunk {
    /// The chunk's 0-based, half-open line range in `version`.
    pub fn lines(&self, merge: &LineMerge, version: Version) -> Range<usize> {
        merge.boundary(version, self.regions.start)..merge.boundary(version, self.regions.end)
    }

    pub fn items(&self, version: Version) -> &[ChunkItem] {
        &self.items[version as usize]
    }

    /// The paths of the chunk's items: those of base, then those that only
    /// ours or theirs has.
    pub fn item_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        for item in self.items.iter().flatten() {
            if !paths.contains(&item.path) {
                paths.push(item.path.clone());
            }
        }
        paths
    }
}

/// Groups the conflicts of `merge` into chunks, in file order. Without an
/// analysis (an unsupported or unparsable file), every conflict is a chunk of
/// its own that no provider may decide.
pub fn chunks(merge: &LineMerge, analysis: Option<&Analysis>) -> Vec<Chunk> {
    let conflicts =
        (0..merge.regions().len()).filter(|&index| merge.regions()[index].is_conflict());
    let Some(analysis) = analysis else {
        return conflicts
            .map(|index| Chunk {
                regions: index..index + 1,
                conflicts: 1,
                items: Default::default(),
                decidable: false,
            })
            .collect();
    };

    let chunker = Chunker::new(merge, analysis);
    let mut spans: Vec<Range<usize>> = conflicts.map(|index| index..index + 1).collect();
    loop {
        let mut changed = false;
        for span in &mut spans {
            for version in Version::ALL {
                if let Cover::Bytes(bytes) = chunker.cover(version, span) {
                    let widened = chunker.widen(version, span, &bytes);
                    if widened != *span {
                        *span = widened;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
        spans = merge_overlapping(spans);
    }
    spans.into_iter().map(|span| chunker.chunk(span)).collect()
}

fn merge_overlapping(mut spans: Vec<Range<usize>>) -> Vec<Range<usize>> {
    spans.sort_by_key(|span| (span.start, span.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start < last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

// What a chunk must cover in one version.
#[derive(Debug, PartialEq, Eq)]
enum Cover {
    /// Nothing but whitespace (or no text at all) is in the chunk.
    Nothing,
    /// The chunk must grow to cover these bytes.
    Bytes(Range<usize>),
    /// The chunk touches text outside every item.
    Outside,
}

struct Chunker<'a> {
    merge: &'a LineMerge<'a>,
    analysis: &'a Analysis,
    // The line of every region boundary in each version; never decreasing.
    boundaries: [Vec<usize>; 3],
}

impl<'a> Chunker<'a> {
    fn new(merge: &'a LineMerge<'a>, analysis: &'a Analysis) -> Self {
        let boundaries = Version::ALL.map(|version| {
            (0..=merge.regions().len())
                .map(|index| merge.boundary(version, index))
                .collect()
        });
        Self {
            merge,
            analysis,
            boundaries,
        }
    }

    fn bytes(&self, version: Version, span: &Range<usize>) -> Range<usize> {
        let boundaries = &self.boundaries[version as usize];
        self.merge
            .bytes(version, boundaries[span.start]..boundaries[span.end])
    }

    // The smallest span around `span` whose lines in `version` cover `bytes`.
    fn widen(&self, version: Version, span: &Range<usize>, bytes: &Range<usize>) -> Range<usize> {
        let boundaries = &self.boundaries[version as usize];
        let first = self.merge.line_at(version, bytes.start);
        let end = self
            .merge
            .line_at(version, bytes.end.saturating_sub(1).max(bytes.start))
            + 1;
        let start = boundaries.partition_point(|&line| line <= first) - 1;
        let stop = boundaries.partition_point(|&line| line < end);
        span.start.min(start)..span.end.max(stop)
    }

    fn cover(&self, version: Version, span: &Range<usize>) -> Cover {
        let bytes = self.bytes(version, span);
        let outline = self.analysis.outline(version);
        if bytes.is_empty() {
            return cover_point(outline, bytes.start);
        }
        let text = self.merge.text(version);
        let mut container = Outline::ROOT;
        loop {
            if touches_own_text(text, outline, container, &bytes) {
                return if container == Outline::ROOT {
                    Cover::Outside
                } else {
                    Cover::Bytes(outline.item(container).bytes.clone())
                };
            }
            let hits = intersecting(outline, container, &bytes);
            match hits {
                [] => return Cover::Nothing,
                [only]
                    if outline.item(*only).container
                        && self.within_lines(version, &outline.item(*only).bytes, &bytes) =>
                {
                    container = *only;
                }
                [first, .., last] | [first @ last] => {
                    return Cover::Bytes(
                        outline.item(*first).bytes.start..outline.item(*last).bytes.end,
                    );
                }
            }
        }
    }

    // Whether `span` lies within the lines that `item` touches.
    fn within_lines(&self, version: Version, item: &Range<usize>, span: &Range<usize>) -> bool {
        let first = self.merge.line_at(version, item.start);
        let last = self
            .merge
            .line_at(version, item.end.saturating_sub(1).max(item.start));
        let lines = self.merge.bytes(version, first..last + 1);
        lines.start <= span.start && span.end <= lines.end
    }

    fn chunk(&self, span: Range<usize>) -> Chunk {
        let mut decidable = true;
        let mut covers_items = false;
        let items = Version::ALL.map(|version| {
            match self.cover(version, &span) {
                Cover::Nothing => {}
                Cover::Bytes(_) => covers_items = true,
                Cover::Outside => decidable = false,
            }
            let outline = self.analysis.outline(version);
            outermost_items(outline, &self.bytes(version, &span))
                .into_iter()
                .map(|index| {
                    let item = outline.item(index);
                    ChunkItem {
                        path: item.path.clone(),
                        node_kind: item.node_kind.clone(),
                    }
                })
                .collect()
        });
        Chunk {
            conflicts: self.merge.regions()[span.clone()]
                .iter()
                .filter(|region| region.is_conflict())
                .count(),
            regions: span,
            items,
            decidable: decidable && covers_items,
        }
    }
}

// The members of `container` that share bytes with `bytes`. Members are
// sorted and do not overlap.
fn intersecting<'o>(outline: &'o Outline, container: usize, bytes: &Range<usize>) -> &'o [usize] {
    let children = &outline.item(container).children;
    let first = children.partition_point(|&child| outline.item(child).bytes.end <= bytes.start);
    let count = children[first..]
        .iter()
        .take_while(|&&child| outline.item(child).bytes.start < bytes.end)
        .count();
    &children[first..first + count]
}

// An empty chunk sits between two lines, so it can only be inside an item,
// never inside a token of a container's own text.
fn cover_point(outline: &Outline, point: usize) -> Cover {
    let mut container = Outline::ROOT;
    loop {
        let children = &outline.item(container).children;
        let next = children.partition_point(|&child| outline.item(child).bytes.end <= point);
        let Some(&child) = children
            .get(next)
            .filter(|&&child| outline.item(child).bytes.start < point)
        else {
            return Cover::Nothing;
        };
        if !outline.item(child).container {
            return Cover::Bytes(outline.item(child).bytes.clone());
        }
        container = child;
    }
}

// Whether `bytes` include text of `container` that none of its members
// covers: a header, a closing brace, or trailing comments.
fn touches_own_text(
    text: &[u8],
    outline: &Outline,
    container: usize,
    bytes: &Range<usize>,
) -> bool {
    let item = outline.item(container);
    let end = bytes.end.min(item.bytes.end);
    let mut cursor = bytes.start.max(item.bytes.start);
    for &child in intersecting(outline, container, bytes) {
        let child = &outline.item(child).bytes;
        if cursor < child.start && has_text(&text[cursor..child.start.min(end)]) {
            return true;
        }
        cursor = cursor.max(child.end);
    }
    cursor < end && has_text(&text[cursor..end])
}

fn has_text(bytes: &[u8]) -> bool {
    bytes.iter().any(|byte| !byte.is_ascii_whitespace())
}

// The items inside `bytes` whose container is not inside them, in file order.
fn outermost_items(outline: &Outline, bytes: &Range<usize>) -> Vec<usize> {
    let mut found = Vec::new();
    let mut containers = vec![Outline::ROOT];
    while let Some(container) = containers.pop() {
        for &child in intersecting(outline, container, bytes) {
            let item = &outline.item(child).bytes;
            if bytes.start <= item.start && item.end <= bytes.end {
                found.push(child);
            } else {
                containers.push(child);
            }
        }
    }
    found.sort_unstable();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ast, merge::Versions};
    use std::path::Path;

    struct Case {
        versions: Versions,
        path: &'static str,
    }

    impl Case {
        fn new(path: &'static str, base: &str, ours: &str, theirs: &str) -> Self {
            Self {
                versions: Versions {
                    base: base.as_bytes().to_vec(),
                    ours: ours.as_bytes().to_vec(),
                    theirs: theirs.as_bytes().to_vec(),
                },
                path,
            }
        }

        fn chunks(&self) -> Vec<Summary> {
            let merge = LineMerge::new(&self.versions).unwrap();
            let analysis = ast::analyze(
                Path::new(self.path),
                &self.versions.base,
                &self.versions.ours,
                &self.versions.theirs,
            )
            .unwrap();
            chunks(&merge, Some(&analysis))
                .iter()
                .map(|chunk| Summary::new(&merge, chunk))
                .collect()
        }
    }

    // A chunk as the tests compare it: its items and text in every version.
    #[derive(Debug, PartialEq, Eq)]
    struct Summary {
        decidable: bool,
        conflicts: usize,
        items: [Vec<String>; 3],
        text: [String; 3],
    }

    impl Summary {
        fn new(merge: &LineMerge, chunk: &Chunk) -> Self {
            Self {
                decidable: chunk.decidable,
                conflicts: chunk.conflicts,
                items: Version::ALL.map(|version| {
                    chunk
                        .items(version)
                        .iter()
                        .map(|item| item.path.clone())
                        .collect()
                }),
                text: Version::ALL.map(|version| {
                    String::from_utf8(merge.slice(version, chunk.lines(merge, version)).to_vec())
                        .unwrap()
                }),
            }
        }
    }

    fn summary(conflicts: usize, items: [&[&str]; 3], text: [&str; 3]) -> Summary {
        Summary {
            decidable: true,
            conflicts,
            items: items.map(|paths| paths.iter().map(|path| (*path).to_owned()).collect()),
            text: text.map(str::to_owned),
        }
    }

    const THREE: &str = "\
fn alpha() -> u32 {
    1
}

fn beta() -> u32 {
    2
}

fn gamma() -> u32 {
    3
}
";

    #[test]
    fn separate_conflicts_in_separate_items_become_separate_chunks() {
        let ours = THREE
            .replace("    1\n", "    10\n")
            .replace("    2\n", "    20\n")
            .replace("    3\n", "    30\n");
        let theirs = THREE
            .replace("    1\n", "    11\n")
            .replace("    2\n", "    22\n")
            .replace("    3\n", "    33\n");

        let chunks = Case::new("lib.rs", THREE, &ours, &theirs).chunks();

        assert_eq!(
            chunks,
            [
                summary(
                    1,
                    [&["fn alpha"], &["fn alpha"], &["fn alpha"]],
                    [
                        "fn alpha() -> u32 {\n    1\n}\n",
                        "fn alpha() -> u32 {\n    10\n}\n",
                        "fn alpha() -> u32 {\n    11\n}\n",
                    ],
                ),
                summary(
                    1,
                    [&["fn beta"], &["fn beta"], &["fn beta"]],
                    [
                        "fn beta() -> u32 {\n    2\n}\n",
                        "fn beta() -> u32 {\n    20\n}\n",
                        "fn beta() -> u32 {\n    22\n}\n",
                    ],
                ),
                summary(
                    1,
                    [&["fn gamma"], &["fn gamma"], &["fn gamma"]],
                    [
                        "fn gamma() -> u32 {\n    3\n}\n",
                        "fn gamma() -> u32 {\n    30\n}\n",
                        "fn gamma() -> u32 {\n    33\n}\n",
                    ],
                ),
            ]
        );
    }

    #[test]
    fn conflicts_inside_one_item_share_its_chunk() {
        let base = "fn alpha() -> u32 {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    a + b + c\n}\n";
        let ours = base.replace("= 1", "= 10").replace("= 3", "= 30");
        let theirs = base.replace("= 1", "= 11").replace("= 3", "= 33");

        let chunks = Case::new("lib.rs", base, &ours, &theirs).chunks();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].conflicts, 2);
        assert_eq!(chunks[0].text, [base.to_owned(), ours, theirs]);
    }

    #[test]
    fn a_conflict_spanning_two_items_covers_exactly_both() {
        let base = "fn alpha() -> u32 { 1 }\nfn beta() -> u32 { 2 }\n\nfn gamma() -> u32 { 3 }\n";
        let ours = base.replace("{ 1 }", "{ 10 }").replace("{ 2 }", "{ 20 }");
        let theirs = base.replace("{ 1 }", "{ 11 }").replace("{ 2 }", "{ 22 }");

        let chunks = Case::new("lib.rs", base, &ours, &theirs).chunks();

        assert_eq!(
            chunks,
            [summary(
                1,
                [
                    &["fn alpha", "fn beta"],
                    &["fn alpha", "fn beta"],
                    &["fn alpha", "fn beta"]
                ],
                [
                    "fn alpha() -> u32 { 1 }\nfn beta() -> u32 { 2 }\n",
                    "fn alpha() -> u32 { 10 }\nfn beta() -> u32 { 20 }\n",
                    "fn alpha() -> u32 { 11 }\nfn beta() -> u32 { 22 }\n",
                ],
            )]
        );
    }

    const IMPL: &str = "\
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

    #[test]
    fn a_conflict_inside_a_method_covers_only_that_method() {
        let ours = IMPL.replace("        self.value\n", "        self.value + 1\n");
        let theirs = IMPL.replace("        self.value\n", "        self.value * 2\n");

        let chunks = Case::new("lib.rs", IMPL, &ours, &theirs).chunks();

        assert_eq!(
            chunks,
            [summary(
                1,
                [
                    &["impl Counter > fn get"],
                    &["impl Counter > fn get"],
                    &["impl Counter > fn get"]
                ],
                [
                    "    fn get(&self) -> u32 {\n        self.value\n    }\n",
                    "    fn get(&self) -> u32 {\n        self.value + 1\n    }\n",
                    "    fn get(&self) -> u32 {\n        self.value * 2\n    }\n",
                ],
            )]
        );
    }

    #[test]
    fn a_conflict_in_a_container_header_covers_the_container() {
        let ours = IMPL.replace("impl Counter {", "impl Clone for Counter {");
        let theirs = IMPL.replace("impl Counter {", "impl Default for Counter {");

        let chunks = Case::new("lib.rs", IMPL, &ours, &theirs).chunks();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].items[0], ["impl Counter"]);
        assert_eq!(chunks[0].items[1], ["impl Clone for Counter"]);
        assert!(chunks[0].text[2].starts_with("impl Default for Counter {\n"));
        assert!(chunks[0].text[2].ends_with("        self.value = 0;\n    }\n}\n"));
    }

    #[test]
    fn items_added_at_the_same_place_form_a_chunk_without_a_base() {
        let added = |name: &str| THREE.replace("fn beta", &format!("fn {name}() {{}}\n\nfn beta"));

        let chunks = Case::new("lib.rs", THREE, &added("delta"), &added("epsilon")).chunks();

        assert_eq!(
            chunks,
            [summary(
                1,
                [&[], &["fn delta"], &["fn epsilon"]],
                ["", "fn delta() {}\n\n", "fn epsilon() {}\n\n"],
            )]
        );
    }

    #[test]
    fn an_item_deleted_on_one_side_and_modified_on_the_other_is_a_chunk() {
        let deleted = THREE.replace("fn beta() -> u32 {\n    2\n}\n\n", "");
        let modified = THREE.replace("    2\n", "    20\n");

        let chunks = Case::new("lib.rs", THREE, &deleted, &modified).chunks();

        assert_eq!(chunks.len(), 1, "{chunks:?}");
        assert!(chunks[0].decidable);
        assert_eq!(chunks[0].items[0], ["fn beta"]);
        assert!(chunks[0].items[1].is_empty());
        assert_eq!(chunks[0].items[2], ["fn beta"]);
        assert!(
            chunks[0].text[1].trim().is_empty(),
            "{:?}",
            chunks[0].text[1]
        );
    }

    #[test]
    fn conflicts_outside_every_item_are_left_undecided() {
        let comment = |text: &str| format!("{THREE}\n// {text}\n");

        let chunks = Case::new(
            "lib.rs",
            &comment("end"),
            &comment("ours"),
            &comment("theirs"),
        )
        .chunks();
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].decidable);

        let blank = Case::new("lib.rs", "fn a() {}\n", "fn a() {}\n\n", "fn a() {}\n\n\n").chunks();
        assert_eq!(blank.len(), 1);
        assert!(!blank[0].decidable);
        assert_eq!(
            blank[0].items,
            [Vec::<String>::new(), Vec::new(), Vec::new()]
        );
    }

    #[test]
    fn without_an_analysis_every_conflict_is_left_undecided() {
        let versions = Versions {
            base: b"a\nb\nc\n".to_vec(),
            ours: b"A\nb\nC\n".to_vec(),
            theirs: b"1\nb\n3\n".to_vec(),
        };
        let merge = LineMerge::new(&versions).unwrap();

        let chunks = chunks(&merge, None);

        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| !chunk.decidable && chunk.conflicts == 1)
        );
    }

    const CLASS: &str = "\
import os


class Greeter:
    @property
    def name(self):
        return \"world\"

    def hello(self):
        return \"hello\"

    def bye(self):
        return \"bye\"
";

    #[test]
    fn python_methods_and_decorated_definitions_are_chunks_of_their_own() {
        let ours = CLASS
            .replace("return \"world\"", "return \"you\"")
            .replace("return \"bye\"", "return \"goodbye\"");
        let theirs = CLASS
            .replace("return \"world\"", "return \"there\"")
            .replace("return \"bye\"", "return \"see you\"");

        let chunks = Case::new("app.py", CLASS, &ours, &theirs).chunks();

        let items: Vec<_> = chunks.iter().map(|chunk| chunk.items[1].clone()).collect();
        assert_eq!(
            items,
            [
                vec!["class Greeter > def name".to_owned()],
                vec!["class Greeter > def bye".to_owned()]
            ]
        );
        assert_eq!(
            chunks[0].text[2],
            "    @property\n    def name(self):\n        return \"there\"\n"
        );
        assert!(chunks.iter().all(|chunk| chunk.decidable));
    }

    #[test]
    fn python_methods_added_at_the_end_of_a_class_stay_members() {
        let added = |body: &str| format!("{CLASS}\n    def extra(self):\n        return {body}\n");

        let chunks = Case::new("app.py", CLASS, &added("1"), &added("2")).chunks();

        assert_eq!(chunks.len(), 1, "{chunks:?}");
        assert_eq!(chunks[0].items[1], ["class Greeter > def extra"]);
        assert_eq!(chunks[0].items[0], Vec::<String>::new());
    }

    #[test]
    fn items_that_share_a_line_are_chunked_together() {
        let base = "fn a() -> u8 { 1 } fn b() -> u8 { 2 }\n\nfn c() {}\n";
        let ours = base.replace("{ 2 }", "{ 20 }");
        let theirs = base.replace("{ 2 }", "{ 22 }");

        let chunks = Case::new("lib.rs", base, &ours, &theirs).chunks();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].items[1], ["fn a", "fn b"]);
    }

    struct Random(u64);

    impl Random {
        fn below(&mut self, limit: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % limit as u64) as usize
        }
    }

    fn item(kind: usize, id: usize) -> String {
        match kind % 5 {
            0 => format!("fn f{id}() -> u32 {{\n    let x = {id};\n    x + 1\n}}\n"),
            1 => format!("/// Docs {id}.\nstruct S{id} {{\n    a: u32,\n}}\n"),
            2 => format!(
                "impl S{id} {{\n    fn get(&self) -> u32 {{\n        self.a\n    }}\n\n    \
                 fn set(&mut self, a: u32) {{\n        self.a = a;\n    }}\n}}\n"
            ),
            3 => format!("// Note {id}.\nconst C{id}: u32 = {id};\n"),
            _ => format!("mod m{id} {{\n    pub fn inner() -> u32 {{\n        {id}\n    }}\n}}\n"),
        }
    }

    fn edit(random: &mut Random, items: &[String]) -> Vec<String> {
        let mut items = items.to_vec();
        for _ in 0..1 + random.below(3) {
            let position = random.below(items.len() + 1);
            let tag = random.below(1000);
            match random.below(6) {
                0 => items.insert(position, item(random.below(5), 100 + tag)),
                1 if position < items.len() => {
                    items.remove(position);
                }
                _ if position < items.len() => {
                    let item = &mut items[position];
                    *item = match random.below(4) {
                        0 => item.replacen(
                            "    let x",
                            &format!("    let y{tag} = {tag};\n    let x"),
                            1,
                        ),
                        1 => item.replacen(
                            "        self.a\n",
                            &format!("        self.a + {tag}\n"),
                            1,
                        ),
                        2 => item.replacen("() ->", &format!("(_v{tag}: u8) ->"), 1),
                        _ => item.replacen("{\n", &format!("{{\n    // Edit {tag}.\n"), 1),
                    };
                }
                _ => {}
            }
        }
        items
    }

    // `inner` lies within the lines that `outer` touches.
    fn within_lines(text: &[u8], outer: &Range<usize>, inner: &Range<usize>) -> bool {
        let start = text[..outer.start]
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |newline| newline + 1);
        let end = text[outer.end.max(1) - 1..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(text.len(), |newline| outer.end.max(1) + newline);
        start <= inner.start && inner.end <= end
    }

    #[test]
    fn chunks_never_cut_through_items_on_random_edits() {
        let mut random = Random(0x2545_f491_4f6c_dd1d);
        let mut decided = 0;
        for _ in 0..400 {
            let base: Vec<String> = (0..2 + random.below(5))
                .map(|id| item(random.below(5), id))
                .collect();
            let (ours, theirs) = (edit(&mut random, &base), edit(&mut random, &base));
            let versions = Versions {
                base: base.join("\n").into_bytes(),
                ours: ours.join("\n").into_bytes(),
                theirs: theirs.join("\n").into_bytes(),
            };
            let case = || String::from_utf8_lossy(&versions.ours).into_owned();
            let merge = LineMerge::new(&versions).unwrap();
            let analysis = ast::analyze(
                Path::new("lib.rs"),
                &versions.base,
                &versions.ours,
                &versions.theirs,
            )
            .unwrap();

            let chunks = chunks(&merge, Some(&analysis));

            let mut covered = 0;
            for (index, chunk) in chunks.iter().enumerate() {
                assert!(chunk.regions.start >= covered, "{chunks:?}");
                covered = chunk.regions.end;
                assert!(chunk.conflicts > 0);
                assert!(index == 0 || chunks[index - 1].regions.end <= chunk.regions.start);
                if !chunk.decidable {
                    continue;
                }
                decided += 1;
                for version in Version::ALL {
                    let text = merge.text(version);
                    let span = merge.bytes(version, chunk.lines(&merge, version));
                    let outline = analysis.outline(version);
                    let mut stack = vec![Outline::ROOT];
                    while let Some(container) = stack.pop() {
                        for &child in &outline.item(container).children {
                            let item = &outline.item(child).bytes;
                            let disjoint = item.end <= span.start || span.end <= item.start;
                            let inside = span.start <= item.start && item.end <= span.end;
                            let around = within_lines(text, item, &span);
                            assert!(
                                disjoint || inside || around,
                                "{version} chunk {:?} cuts {}:\n{}",
                                chunk.regions,
                                outline.item(child).path,
                                case()
                            );
                            stack.push(child);
                        }
                    }
                }
            }
            let conflicts = merge
                .regions()
                .iter()
                .filter(|region| region.is_conflict())
                .count();
            assert_eq!(
                chunks.iter().map(|chunk| chunk.conflicts).sum::<usize>(),
                conflicts
            );

            for side in [Version::Ours, Version::Theirs] {
                let spans: Vec<_> = chunks
                    .iter()
                    .filter(|chunk| chunk.decidable)
                    .map(|chunk| crate::merge::Span {
                        regions: chunk.regions.clone(),
                        take: Some(side),
                    })
                    .collect();
                let stitched = merge.render(&spans, crate::merge::Conflicts::Fill(side));
                assert_eq!(
                    ast::check_syntax(ast::Language::Rust, &stitched.text),
                    Ok(()),
                    "taking {side}:\n{}\n--- from ---\n{}",
                    String::from_utf8_lossy(&stitched.text),
                    case()
                );
            }
        }
        assert!(decided > 50, "only {decided} chunks were decidable");
    }
}
