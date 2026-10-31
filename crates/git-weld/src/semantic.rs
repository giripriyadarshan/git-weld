//! Semantic three-way merge over [`LineMerge`]: AST item outlines, widening
//! line conflicts into chunks of whole items, non-overlapping diverged item
//! detection, lazy [`SemanticState`] assembly, and stitched syntax
//! verification.

mod items;
mod state;

pub use crate::merge::Version;
pub use state::{ChunkState, SemanticState, SignatureState, VersionState};

use crate::merge::{Conflicts, LineMerge, Rendered, Span};
use items::{Item, Reference, Segment, dedent};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    ops::{Range, RangeInclusive},
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
};
use tree_sitter::{Node, Parser, Point, Tree};

pub(crate) const MAX_CLEAN_ANALYZE_BYTES: usize = 512 * 1024;

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
    UnexpectedFailure,
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
            Self::UnexpectedFailure => formatter.write_str("it failed unexpectedly"),
        }
    }
}

impl Error for AnalysisError {}

/// Why a merged result does not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyntaxError {
    InvalidUtf8,
    At { line: usize, column: usize },
    Parser(String),
    UnexpectedFailure,
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
            Self::UnexpectedFailure => formatter.write_str("the parser failed unexpectedly"),
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
    index: usize,
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

/// A run of merge regions widened to cover whole syntactic items in every
/// version so it can be decided and replaced as a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// The merge regions it covers; at least one is a conflict.
    pub regions: Range<usize>,
    /// How many conflicts it covers.
    pub conflicts: usize,
    /// The paths of the chunk's items: those of base, then those that only
    /// ours or theirs has.
    pub items: Vec<String>,
    /// Whether a provider may decide it. A chunk that covers no item, or that
    /// touches text outside every item (such as a comment at the end of the
    /// file), keeps its conflict markers.
    pub decidable: bool,
    covered: [Vec<usize>; 3],
}

impl Chunk {
    /// The chunk's 0-based, half-open line range in `version`.
    pub fn lines(&self, merge: &LineMerge, version: Version) -> Range<usize> {
        merge.span_lines(version, self.regions.clone())
    }

    /// The paths of the chunk's items: those of base, then those that only
    /// ours or theirs has.
    pub fn item_paths(&self) -> &[String] {
        &self.items
    }

    fn covered(&self, version: Version) -> &[usize] {
        &self.covered[version as usize]
    }

    fn overlaps_diverged(&self, outlines: &[Outline; 3], diverged: &DivergedItem) -> bool {
        [
            (Version::Base, &diverged.base),
            (Version::Ours, &diverged.ours),
            (Version::Theirs, &diverged.theirs),
        ]
        .into_iter()
        .any(|(version, location)| {
            location.as_ref().is_some_and(|location| {
                let outline = &outlines[version as usize];
                self.covered(version)
                    .iter()
                    .any(|&root| outline.shares_lineage(root, location.index))
            })
        })
    }
}

/// Syntax-aware three-way merge layered over [`LineMerge`].
pub struct SemanticMerge<'a> {
    path: PathBuf,
    merge: &'a LineMerge<'a>,
    analysis: Option<Analysis>,
    analysis_error: Option<AnalysisError>,
    chunks: Vec<Chunk>,
    combined_items: Vec<DivergedItem>,
}

impl<'a> SemanticMerge<'a> {
    /// Analyzes the three versions in `merge` for the file at `path`, widens
    /// line conflicts into syntactic chunks (falling back to one undecidable
    /// chunk per conflict on error or panic), and finds diverged items whose
    /// byte spans do not overlap any chunk.
    pub fn analyze(path: &Path, merge: &'a LineMerge<'a>) -> Self {
        let path_buf = path.to_path_buf();
        if merge.is_clean()
            && Version::ALL
                .into_iter()
                .any(|version| merge.text(version).len() > MAX_CLEAN_ANALYZE_BYTES)
        {
            return Self {
                path: path_buf,
                merge,
                analysis: None,
                analysis_error: None,
                chunks: Vec::new(),
                combined_items: Vec::new(),
            };
        }

        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            let analysis = analyze_versions(
                path,
                merge.text(Version::Base),
                merge.text(Version::Ours),
                merge.text(Version::Theirs),
            )?;
            let chunks = widen_chunks(merge, &analysis);
            let combined_items = analysis
                .diverged_items
                .iter()
                .filter(|diverged| {
                    !chunks
                        .iter()
                        .any(|chunk| chunk.overlaps_diverged(&analysis.outlines, diverged))
                })
                .cloned()
                .collect();
            Ok((analysis, chunks, combined_items))
        }))
        .unwrap_or(Err(AnalysisError::UnexpectedFailure));

        match outcome {
            Ok((analysis, chunks, combined_items)) => Self {
                path: path_buf,
                merge,
                analysis: Some(analysis),
                analysis_error: None,
                chunks,
                combined_items,
            },
            Err(error) => Self {
                path: path_buf,
                merge,
                analysis: None,
                analysis_error: Some(error),
                chunks: fallback_chunks(merge),
                combined_items: Vec::new(),
            },
        }
    }

    /// Builds the [`SemanticState`] for the decidable chunks, paired with their
    /// indices in [`Self::chunks`]. Returns `Ok(None)` when no chunks are
    /// decidable or when semantic analysis was skipped.
    pub fn semantic_state(&self) -> Result<Option<(Vec<usize>, SemanticState)>, AnalysisError> {
        let Some(analysis) = &self.analysis else {
            return Ok(None);
        };
        let decidable: Vec<usize> = self
            .chunks
            .iter()
            .enumerate()
            .filter_map(|(index, chunk)| chunk.decidable.then_some(index))
            .collect();
        if decidable.is_empty() {
            return Ok(None);
        }
        let selected: Vec<&Chunk> = decidable.iter().map(|&index| &self.chunks[index]).collect();
        let state = panic::catch_unwind(AssertUnwindSafe(|| {
            SemanticState::new(&self.path, analysis, self.merge, &selected)
        }))
        .map_err(|_| AnalysisError::UnexpectedFailure)?;
        Ok(Some((decidable, state)))
    }

    /// Verifies stitched syntax when any chunk in `takes` is resolved, resets
    /// `takes` to `None` in place if the stitched file fails to parse or
    /// panics, and renders the merged file.
    pub fn resolve(
        &self,
        takes: &mut [Option<Version>],
        marker_size: u16,
    ) -> (Rendered, Result<(), SyntaxError>) {
        assert_eq!(
            takes.len(),
            self.chunks.len(),
            "takes length must match chunk count"
        );
        let syntax = if let Some(language) = self.language()
            && takes.iter().any(Option::is_some)
        {
            let probe = self
                .merge
                .render(&spans(&self.chunks, takes), Conflicts::Fill(Version::Ours));
            let check =
                panic::catch_unwind(AssertUnwindSafe(|| check_syntax(language, &probe.text)))
                    .unwrap_or(Err(SyntaxError::UnexpectedFailure));
            if check.is_err() {
                takes.fill(None);
            }
            check
        } else {
            Ok(())
        };
        let rendered = self
            .merge
            .render(&spans(&self.chunks, takes), Conflicts::Markers(marker_size));
        (rendered, syntax)
    }

    pub fn language(&self) -> Option<Language> {
        self.analysis.as_ref().map(|analysis| analysis.language)
    }

    pub fn analysis_error(&self) -> Option<&AnalysisError> {
        self.analysis_error.as_ref()
    }

    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    pub fn combined_items(&self) -> &[DivergedItem] {
        &self.combined_items
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Analysis {
    language: Language,
    diverged_items: Vec<DivergedItem>,
    outlines: [Outline; 3],
}

impl Analysis {
    fn outline(&self, version: Version) -> &Outline {
        &self.outlines[version as usize]
    }
}

/// The items of one version as a tree. The root (index 0) is the whole file;
/// the children of a container are the members of its body.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Outline {
    items: Vec<OutlineItem>,
    references: Vec<Reference>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OutlineItem {
    key: Vec<Segment>,
    /// Human-readable path, such as `impl Counter > fn get`.
    path: String,
    name: Option<String>,
    signature: Option<String>,
    node_kind: String,
    /// Includes the item's leading comments, attributes, and decorators.
    bytes: Range<usize>,
    parent: Option<usize>,
    /// Indices of the members, in file order. Only containers have members.
    children: Vec<usize>,
    /// Whether the members are compared one by one: impl, trait, mod, and
    /// extern blocks, classes, and the root.
    container: bool,
}

impl Outline {
    const ROOT: usize = 0;

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

    fn item(&self, index: usize) -> &OutlineItem {
        &self.items[index]
    }

    fn items(&self) -> &[OutlineItem] {
        &self.items
    }

    /// Names referenced inside `bytes`, in source order.
    fn referenced_names(&self, bytes: Range<usize>) -> impl Iterator<Item = &str> {
        self.references
            .iter()
            .filter(move |reference| {
                bytes.start <= reference.bytes.start && reference.bytes.end <= bytes.end
            })
            .map(|reference| reference.name.as_str())
    }

    // The members of `container` that share bytes with `bytes`. Members are
    // sorted and do not overlap.
    fn intersecting(&self, container: usize, bytes: &Range<usize>) -> &[usize] {
        let children = &self.item(container).children;
        let first = children.partition_point(|&child| self.item(child).bytes.end <= bytes.start);
        let count = children[first..]
            .iter()
            .take_while(|&&child| self.item(child).bytes.start < bytes.end)
            .count();
        &children[first..first + count]
    }

    // The items inside `bytes` whose container is not inside them, in file order.
    fn outermost_items(&self, bytes: &Range<usize>) -> Vec<usize> {
        let mut found = Vec::new();
        let mut containers = vec![Self::ROOT];
        while let Some(container) = containers.pop() {
            for &child in self.intersecting(container, bytes) {
                let item = &self.item(child).bytes;
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

    fn shares_lineage(&self, first: usize, second: usize) -> bool {
        first == second || self.is_ancestor(first, second) || self.is_ancestor(second, first)
    }

    fn is_ancestor(&self, ancestor: usize, mut item: usize) -> bool {
        while let Some(parent) = self.item(item).parent {
            if parent == Self::ROOT {
                break;
            }
            if parent == ancestor {
                return true;
            }
            item = parent;
        }
        false
    }

    fn collect_covered_keys<'a>(
        &'a self,
        span: &Range<usize>,
        covered_roots: &[usize],
        chunk_items: &[String],
        covered: &mut HashSet<&'a [Segment]>,
    ) {
        let mut stack: Vec<usize> = covered_roots.to_vec();
        while let Some(index) = stack.pop() {
            let item = self.item(index);
            covered.insert(item.key.as_slice());
            stack.extend_from_slice(&item.children);
        }
        for &root in covered_roots {
            let mut parent = self.item(root).parent;
            while let Some(index) = parent {
                if index == Self::ROOT {
                    break;
                }
                let item = self.item(index);
                covered.insert(item.key.as_slice());
                parent = item.parent;
            }
        }
        let mut containers = vec![Self::ROOT];
        while let Some(container) = containers.pop() {
            for &child in self.intersecting(container, span) {
                let item = self.item(child);
                if item.container && range_contains(&item.bytes, span) {
                    covered.insert(item.key.as_slice());
                    containers.push(child);
                }
            }
        }
        for item in self.items.iter().skip(1) {
            if chunk_items.contains(&item.path) {
                covered.insert(item.key.as_slice());
            }
        }
    }
}

fn range_contains(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    !outer.is_empty() && !inner.is_empty() && outer.start <= inner.start && inner.end <= outer.end
}

fn spans(chunks: &[Chunk], takes: &[Option<Version>]) -> Vec<Span> {
    chunks
        .iter()
        .zip(takes)
        .map(|(chunk, &take)| Span {
            regions: chunk.regions.clone(),
            take,
        })
        .collect()
}

fn fallback_chunks(merge: &LineMerge) -> Vec<Chunk> {
    (0..merge.regions().len())
        .filter(|&index| merge.regions()[index].is_conflict())
        .map(|index| Chunk {
            regions: index..index + 1,
            conflicts: 1,
            items: Vec::new(),
            decidable: false,
            covered: Default::default(),
        })
        .collect()
}

fn widen_chunks(merge: &LineMerge, analysis: &Analysis) -> Vec<Chunk> {
    let conflicts =
        (0..merge.regions().len()).filter(|&index| merge.regions()[index].is_conflict());
    let chunker = Chunker { merge, analysis };
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
}

impl<'a> Chunker<'a> {
    // The smallest span around `span` whose lines in `version` cover `bytes`.
    fn widen(&self, version: Version, span: &Range<usize>, bytes: &Range<usize>) -> Range<usize> {
        let first = self.merge.line_at(version, bytes.start);
        let end = self
            .merge
            .line_at(version, bytes.end.saturating_sub(1).max(bytes.start))
            + 1;
        let regions = self.merge.regions();
        let start = regions.partition_point(|region| region.lines(version).end <= first);
        let stop = regions.partition_point(|region| region.lines(version).start < end);
        span.start.min(start)..span.end.max(stop)
    }

    fn cover(&self, version: Version, span: &Range<usize>) -> Cover {
        let bytes = self.merge.span_bytes(version, span.clone());
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
            let hits = outline.intersecting(container, &bytes);
            match hits {
                [] => return Cover::Nothing,
                [only]
                    if outline.item(*only).container && {
                        let lines = self
                            .merge
                            .line_span_bytes(version, outline.item(*only).bytes.clone());
                        lines.start <= bytes.start && bytes.end <= lines.end
                    } =>
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

    fn chunk(&self, span: Range<usize>) -> Chunk {
        let mut decidable = true;
        let mut covers_items = false;
        let mut items: Vec<String> = Vec::new();
        let covered = Version::ALL.map(|version| {
            match self.cover(version, &span) {
                Cover::Nothing => {}
                Cover::Bytes(_) => covers_items = true,
                Cover::Outside => decidable = false,
            }
            let outline = self.analysis.outline(version);
            let roots = outline.outermost_items(&self.merge.span_bytes(version, span.clone()));
            for &index in &roots {
                let path = &outline.item(index).path;
                if !items.contains(path) {
                    items.push(path.clone());
                }
            }
            roots
        });
        Chunk {
            regions: span.clone(),
            conflicts: self.merge.regions()[span]
                .iter()
                .filter(|region| region.is_conflict())
                .count(),
            items,
            decidable: decidable && covers_items,
            covered,
        }
    }
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
    for &child in outline.intersecting(container, bytes) {
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

/// Parses all three versions of the file at the logical path `path` and finds
/// the items that both sides changed. Edits that only change formatting do not
/// count as changes.
fn analyze_versions(
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
            SyntaxError::UnexpectedFailure => AnalysisError::UnexpectedFailure,
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
fn check_syntax(language: Language, source: &[u8]) -> Result<(), SyntaxError> {
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
            (Some((_, old)), Some((_, new_ours)), Some((_, new_theirs)))
                if old.fingerprint != new_ours.fingerprint
                    && old.fingerprint != new_theirs.fingerprint
                    && new_ours.fingerprint != new_theirs.fingerprint =>
            {
                Divergence::BothModified
            }
            (None, Some((_, new_ours)), Some((_, new_theirs)))
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
            .map(|(_, item)| item.path.clone());
        let locate = |(index, item): (usize, &Item)| Location {
            index,
            ..item.location.clone()
        };
        diverged.push(DivergedItem {
            item: path.unwrap_or_default(),
            kind,
            base: old.map(locate),
            ours: new_ours.map(locate),
            theirs: new_theirs.map(locate),
        });
    }
    diverged
}

fn index(items: &[Item]) -> HashMap<&[Segment], (usize, &Item)> {
    items
        .iter()
        .enumerate()
        .map(|(position, item)| (item.key.as_slice(), (position, item)))
        .collect()
}

// Whether `side` added or modified the item at `key` or anything inside it.
// Removing children of an item that the other side deleted is compatible.
fn adds_or_modifies(
    key: &[Segment],
    base: &HashMap<&[Segment], (usize, &Item)>,
    side: &[Item],
) -> bool {
    side.iter()
        .filter(|item| item.key.starts_with(key))
        .any(|item| {
            base.get(item.key.as_slice())
                .is_none_or(|(_, original)| original.fingerprint != item.fingerprint)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::Versions;

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
        analyze_versions(
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
        index: usize,
    ) -> Location {
        let start = source.find(snippet).expect("snippet is in the source");
        Location {
            node_kind: node_kind.to_owned(),
            bytes: start..start + snippet.len(),
            lines,
            index,
        }
    }

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
            let semantic = SemanticMerge::analyze(Path::new(self.path), &merge);
            let analysis = semantic.analysis.as_ref().unwrap();
            semantic
                .chunks()
                .iter()
                .map(|chunk| Summary::new(&merge, analysis, chunk))
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
        fn new(merge: &LineMerge, analysis: &Analysis, chunk: &Chunk) -> Self {
            Self {
                decidable: chunk.decidable,
                conflicts: chunk.conflicts,
                items: Version::ALL.map(|version| {
                    let outline = analysis.outline(version);
                    chunk
                        .covered(version)
                        .iter()
                        .map(|&index| outline.item(index).path.clone())
                        .collect()
                }),
                text: Version::ALL.map(|version| {
                    String::from_utf8(merge.span_slice(version, chunk.regions.clone()).to_vec())
                        .unwrap()
                }),
            }
        }
    }

    fn chunk_summary(conflicts: usize, items: [&[&str]; 3], text: [&str; 3]) -> Summary {
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
        let versions = Versions {
            base: b"a\n".to_vec(),
            ours: b"b\n".to_vec(),
            theirs: b"c\n".to_vec(),
        };
        let merge = LineMerge::new(&versions).unwrap();
        let semantic = SemanticMerge::analyze(Path::new("notes.txt"), &merge);

        assert_eq!(semantic.language(), None);
        assert_eq!(
            semantic.analysis_error(),
            Some(&AnalysisError::UnsupportedFileType)
        );
        assert_eq!(
            semantic.analysis_error().unwrap().to_string(),
            "unsupported file type (supported: .rs, .py)"
        );
        assert_eq!(semantic.chunks().len(), 1);
        assert!(!semantic.chunks()[0].decidable);
        assert_eq!(semantic.semantic_state(), Ok(None));
    }

    #[test]
    fn rejects_input_that_is_not_utf8() {
        let versions = Versions {
            base: b"fn a() {}\n".to_vec(),
            ours: b"fn a() { 1 }\n".to_vec(),
            theirs: b"fn a() { \"\xff\"; }\n".to_vec(),
        };
        let merge = LineMerge::new(&versions).unwrap();
        let semantic = SemanticMerge::analyze(Path::new("lib.rs"), &merge);

        assert_eq!(semantic.language(), None);
        assert_eq!(
            semantic.analysis_error(),
            Some(&AnalysisError::InvalidUtf8 {
                version: Version::Theirs
            })
        );
        assert_eq!(
            semantic.analysis_error().unwrap().to_string(),
            "theirs is not valid UTF-8"
        );
        assert_eq!(semantic.chunks().len(), 1);
        assert!(!semantic.chunks()[0].decidable);
    }

    #[test]
    fn reports_where_a_version_has_a_syntax_error() {
        let versions = Versions {
            base: b"fn a() {}\n".to_vec(),
            ours: b"fn a() {\n    let x = ;\n}\n".to_vec(),
            theirs: b"fn a() { 1 }\n".to_vec(),
        };
        let merge = LineMerge::new(&versions).unwrap();
        let semantic = SemanticMerge::analyze(Path::new("lib.rs"), &merge);
        let error = semantic.analysis_error().unwrap();
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
        assert_eq!(semantic.language(), None);
        assert_eq!(semantic.chunks().len(), 1);
        assert!(!semantic.chunks()[0].decidable);

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
                    1..=3,
                    1,
                )),
                ours: Some(location(
                    &ours,
                    "fn alpha() -> u32 {\n    10\n}",
                    "function_item",
                    1..=3,
                    1,
                )),
                theirs: Some(location(
                    &theirs,
                    "fn alpha() -> u32 {\n    100\n}",
                    "function_item",
                    3..=5,
                    2,
                )),
            }]
        );
        assert_eq!(
            analysis.diverged_items[0].to_string(),
            "fn alpha: modified on both sides (base lines 1-3, ours lines 1-3, theirs lines 3-5)"
        );
    }

    #[test]
    fn combined_items_reports_diverged_items_that_do_not_overlap_any_chunk() {
        let base = "fn alpha() -> u32 {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    a + b + c\n}\n\nfn beta() -> u32 {\n    2\n}\n";
        let ours = base.replace("= 1", "= 10").replace("    2\n", "    20\n");
        let theirs = base.replace("= 3", "= 30").replace("    2\n", "    200\n");
        let versions = Versions {
            base: base.as_bytes().to_vec(),
            ours: ours.as_bytes().to_vec(),
            theirs: theirs.as_bytes().to_vec(),
        };
        let merge = LineMerge::new(&versions).unwrap();
        let semantic = SemanticMerge::analyze(Path::new("src/lib.rs"), &merge);

        assert_eq!(semantic.chunks().len(), 1);
        assert_eq!(semantic.chunks()[0].item_paths(), ["fn beta"]);
        assert_eq!(semantic.combined_items().len(), 1);
        assert_eq!(semantic.combined_items()[0].item, "fn alpha");
        assert_eq!(semantic.combined_items()[0].kind, Divergence::BothModified);

        let container_base = "// Counter impl.\nimpl Counter {\n    fn get(&self) -> u32 {\n        let a = 1;\n        let b = 2;\n        let c = 3;\n        a + b + c\n    }\n\n    fn reset(&mut self) {\n        self.value = 0;\n    }\n}\n";
        let sibling_ours = container_base
            .replace("= 1", "= 10")
            .replace("self.value = 0;", "self.value = 1;");
        let sibling_theirs = container_base
            .replace("= 3", "= 30")
            .replace("self.value = 0;", "self.value = 2;");
        let sibling_versions = Versions {
            base: container_base.as_bytes().to_vec(),
            ours: sibling_ours.as_bytes().to_vec(),
            theirs: sibling_theirs.as_bytes().to_vec(),
        };
        let sibling_merge = LineMerge::new(&sibling_versions).unwrap();
        let sibling_semantic = SemanticMerge::analyze(Path::new("src/lib.rs"), &sibling_merge);
        assert_eq!(
            sibling_semantic.chunks()[0].item_paths(),
            ["impl Counter > fn reset"]
        );
        assert_eq!(sibling_semantic.combined_items().len(), 1);
        assert_eq!(
            sibling_semantic.combined_items()[0].item,
            "impl Counter > fn get"
        );

        let header_ours = container_base
            .replace("// Counter impl.", "// Ours impl.")
            .replace("= 1", "= 10");
        let header_theirs = container_base
            .replace("// Counter impl.", "// Theirs impl.")
            .replace("= 3", "= 30");
        let header_versions = Versions {
            base: container_base.as_bytes().to_vec(),
            ours: header_ours.as_bytes().to_vec(),
            theirs: header_theirs.as_bytes().to_vec(),
        };
        let header_merge = LineMerge::new(&header_versions).unwrap();
        let header_semantic = SemanticMerge::analyze(Path::new("src/lib.rs"), &header_merge);
        assert_eq!(header_semantic.chunks()[0].item_paths(), ["impl Counter"]);
        assert!(header_semantic.combined_items().is_empty());
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
        let get = |value: &str| IMPL.replace("        self.value\n", &format!("        {value}\n"));
        let reset = IMPL.replace("self.value = 0;", "self.value = 1;");

        assert_eq!(summary("lib.rs", IMPL, &get("self.value + 1"), &reset), []);
        assert_eq!(
            summary(
                "lib.rs",
                IMPL,
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
                    4..=5,
                    2,
                )),
                ours: Some(location(
                    &ours,
                    "def alpha():\n    return 10",
                    "function_definition",
                    4..=5,
                    2,
                )),
                theirs: Some(location(
                    &theirs,
                    "def alpha():\n    return 100",
                    "function_definition",
                    5..=6,
                    3,
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
                6..=8,
                3,
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
                chunk_summary(
                    1,
                    [&["fn alpha"], &["fn alpha"], &["fn alpha"]],
                    [
                        "fn alpha() -> u32 {\n    1\n}\n",
                        "fn alpha() -> u32 {\n    10\n}\n",
                        "fn alpha() -> u32 {\n    11\n}\n",
                    ],
                ),
                chunk_summary(
                    1,
                    [&["fn beta"], &["fn beta"], &["fn beta"]],
                    [
                        "fn beta() -> u32 {\n    2\n}\n",
                        "fn beta() -> u32 {\n    20\n}\n",
                        "fn beta() -> u32 {\n    22\n}\n",
                    ],
                ),
                chunk_summary(
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
            [chunk_summary(
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

    #[test]
    fn a_conflict_inside_a_method_covers_only_that_method() {
        let ours = IMPL.replace("        self.value\n", "        self.value + 1\n");
        let theirs = IMPL.replace("        self.value\n", "        self.value * 2\n");

        let chunks = Case::new("lib.rs", IMPL, &ours, &theirs).chunks();

        assert_eq!(
            chunks,
            [chunk_summary(
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
            [chunk_summary(
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

        let semantic = SemanticMerge::analyze(Path::new("notes.txt"), &merge);
        let chunks = semantic.chunks();

        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| !chunk.decidable && chunk.conflicts == 1)
        );
    }

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

    #[test]
    fn resolve_resets_takes_to_none_when_stitched_syntax_fails() {
        let base =
            "class A:\n    def f(self):\n        return 1\n\n    def g(self):\n        return 2\n";
        let ours = "class A:\n    def f(self):\n        return 10\n\n";
        let theirs = "class A:\n\n    def g(self):\n        return 20\n";
        let versions = Versions {
            base: base.as_bytes().to_vec(),
            ours: ours.as_bytes().to_vec(),
            theirs: theirs.as_bytes().to_vec(),
        };
        let merge = LineMerge::new(&versions).unwrap();
        let semantic = SemanticMerge::analyze(Path::new("app.py"), &merge);

        let mut takes = vec![Some(Version::Theirs), Some(Version::Ours)];
        let (rendered, syntax) = semantic.resolve(&mut takes, 7);

        assert!(matches!(syntax, Err(SyntaxError::At { .. })), "{syntax:?}");
        assert_eq!(takes, [None, None]);
        assert_eq!(rendered.markers, 2);
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
            let semantic = SemanticMerge::analyze(Path::new("lib.rs"), &merge);
            let analysis = semantic.analysis.as_ref().unwrap();
            let chunks = semantic.chunks();

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
                    let span = merge.span_bytes(version, chunk.regions.clone());
                    let outline = analysis.outline(version);
                    let mut stack = vec![Outline::ROOT];
                    while let Some(container) = stack.pop() {
                        for &child in &outline.item(container).children {
                            let item = &outline.item(child).bytes;
                            let lines = merge.line_span_bytes(version, item.clone());
                            let disjoint = item.end <= span.start || span.end <= item.start;
                            let inside = span.start <= item.start && item.end <= span.end;
                            let around = lines.start <= span.start && span.end <= lines.end;
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
                    .map(|chunk| Span {
                        regions: chunk.regions.clone(),
                        take: Some(side),
                    })
                    .collect();
                let stitched = merge.render(&spans, Conflicts::Fill(side));
                assert_eq!(
                    check_syntax(Language::Rust, &stitched.text),
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
