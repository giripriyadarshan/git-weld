//! The line-based three-way merge that runs before anything else, like
//! `git merge-file`, and writing the merged file atomically.

use anyhow::{Context, Result, ensure};
use similar::{Algorithm, MergeResolution, TextMerge};
use std::{
    error::Error,
    fmt, fs,
    io::Write,
    ops::{Range, RangeInclusive},
    path::Path,
};
use tempfile::NamedTempFile;

/// The labels of the diff3 conflict markers: `<<<<<<< HEAD`,
/// `||||||| base`, and `>>>>>>> theirs`.
const LABELS: [&str; 3] = ["base", "HEAD", "theirs"];

/// One of the three versions of a merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Version {
    Base,
    Ours,
    Theirs,
}

impl Version {
    pub const ALL: [Self; 3] = [Self::Base, Self::Ours, Self::Theirs];

    const fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Base => "base",
            Self::Ours => "ours",
            Self::Theirs => "theirs",
        })
    }
}

#[derive(Debug)]
pub struct Versions {
    pub base: Vec<u8>,
    pub ours: Vec<u8>,
    pub theirs: Vec<u8>,
}

impl Versions {
    pub fn read(base: &Path, ours: &Path, theirs: &Path) -> Result<Self> {
        Ok(Self {
            base: read_version(base, "base")?,
            ours: read_version(ours, "ours")?,
            theirs: read_version(theirs, "theirs")?,
        })
    }

    pub fn get(&self, version: Version) -> &[u8] {
        match version {
            Version::Base => &self.base,
            Version::Ours => &self.ours,
            Version::Theirs => &self.theirs,
        }
    }
}

/// How the line merge settled one region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// No side changed these lines.
    Unchanged,
    /// Only ours changed them.
    Ours,
    /// Only theirs changed them.
    Theirs,
    /// Both sides made the same change.
    Both,
    /// The sides changed them in different ways.
    Conflict,
}

/// A run of lines that the merge settled in one way, as 0-based, half-open
/// line ranges in each version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub resolution: Resolution,
    pub base: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
}

impl Region {
    pub fn lines(&self, version: Version) -> Range<usize> {
        match version {
            Version::Base => self.base.clone(),
            Version::Ours => self.ours.clone(),
            Version::Theirs => self.theirs.clone(),
        }
    }

    pub fn is_conflict(&self) -> bool {
        self.resolution == Resolution::Conflict
    }
}

/// The merge produced regions that do not fit the three versions, which would
/// be a bug in the merge algorithm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidMerge(String);

impl fmt::Display for InvalidMerge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "the line merge is inconsistent: {}", self.0)
    }
}

impl Error for InvalidMerge {}

/// A three-way merge by lines, like `git merge-file` with the histogram diff
/// that Git's own merge uses.
///
/// Edits that do not overlap combine, and identical edits count once; the
/// rest become conflicts with their line ranges in every version. A line ends
/// after `\n`, `\r\n`, or a lone `\r`. Unchanged regions are split into
/// single lines, so every unchanged line boundary is also a region boundary.
pub struct LineMerge<'a> {
    versions: &'a Versions,
    // The byte offset where each line starts, then the text length.
    starts: [Vec<usize>; 3],
    regions: Vec<Region>,
}

impl<'a> LineMerge<'a> {
    pub fn new(versions: &'a Versions) -> Result<Self, InvalidMerge> {
        let merge = TextMerge::configure()
            .algorithm(Algorithm::Histogram)
            .merge_lines(
                versions.base.as_slice(),
                versions.ours.as_slice(),
                versions.theirs.as_slice(),
            );
        let starts = [
            line_starts(merge.base_len(), |line| merge.base_line(line)),
            line_starts(merge.ours_len(), |line| merge.ours_line(line)),
            line_starts(merge.theirs_len(), |line| merge.theirs_line(line)),
        ];

        let mut regions = Vec::new();
        for region in merge.regions() {
            let resolution = match region.resolution() {
                MergeResolution::Unchanged => Resolution::Unchanged,
                MergeResolution::Ours => Resolution::Ours,
                MergeResolution::Theirs => Resolution::Theirs,
                MergeResolution::Both => Resolution::Both,
                _ => Resolution::Conflict,
            };
            let (base, ours, theirs) = (
                region.base_range(),
                region.ours_range(),
                region.theirs_range(),
            );
            if resolution == Resolution::Unchanged
                && base.len() == ours.len()
                && base.len() == theirs.len()
            {
                regions.extend((0..base.len()).map(|offset| Region {
                    resolution,
                    base: base.start + offset..base.start + offset + 1,
                    ours: ours.start + offset..ours.start + offset + 1,
                    theirs: theirs.start + offset..theirs.start + offset + 1,
                }));
            } else {
                regions.push(Region {
                    resolution,
                    base,
                    ours,
                    theirs,
                });
            }
        }

        let merge = Self {
            versions,
            starts,
            regions,
        };
        merge.validate()?;
        Ok(merge)
    }

    // Every version must be covered by its regions in order, and a region
    // must only differ between the versions its resolution says changed.
    fn validate(&self) -> Result<(), InvalidMerge> {
        for version in Version::ALL {
            if self.starts[version.index()].last() != Some(&self.versions.get(version).len()) {
                return Err(InvalidMerge(format!(
                    "the {version} lines do not cover the file"
                )));
            }
            let mut next = 0;
            for region in &self.regions {
                let lines = region.lines(version);
                if lines.start != next || lines.end < lines.start {
                    return Err(InvalidMerge(format!(
                        "the {version} lines are out of order at line {next}"
                    )));
                }
                next = lines.end;
            }
            if next != self.line_count(version) {
                return Err(InvalidMerge(format!(
                    "the {version} lines are not all merged"
                )));
            }
        }
        for region in &self.regions {
            let same = |left: Version, right: Version| {
                self.slice(left, region.lines(left)) == self.slice(right, region.lines(right))
            };
            let consistent = match region.resolution {
                Resolution::Unchanged => {
                    same(Version::Base, Version::Ours) && same(Version::Base, Version::Theirs)
                }
                Resolution::Ours => same(Version::Base, Version::Theirs),
                Resolution::Theirs => same(Version::Base, Version::Ours),
                Resolution::Both => same(Version::Ours, Version::Theirs),
                Resolution::Conflict => true,
            };
            if !consistent {
                return Err(InvalidMerge(format!(
                    "a {:?} region at base lines {:?} changes the wrong versions",
                    region.resolution, region.base
                )));
            }
        }
        Ok(())
    }

    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    pub fn conflicts(&self) -> usize {
        self.regions
            .iter()
            .filter(|region| region.is_conflict())
            .count()
    }

    pub fn is_clean(&self) -> bool {
        !self.regions.iter().any(Region::is_conflict)
    }

    pub fn text(&self, version: Version) -> &'a [u8] {
        self.versions.get(version)
    }

    pub fn line_count(&self, version: Version) -> usize {
        self.starts[version.index()].len() - 1
    }

    /// The bytes of `lines` in `version`.
    pub fn bytes(&self, version: Version, lines: Range<usize>) -> Range<usize> {
        let starts = &self.starts[version.index()];
        starts[lines.start]..starts[lines.end]
    }

    pub fn slice(&self, version: Version, lines: Range<usize>) -> &'a [u8] {
        &self.text(version)[self.bytes(version, lines)]
    }

    /// The line that contains byte `offset` of `version`; the line count for the
    /// end of the text.
    pub fn line_at(&self, version: Version, offset: usize) -> usize {
        self.starts[version.index()].partition_point(|&start| start <= offset) - 1
    }

    /// The line in `version` where region `index` starts; the line count when
    /// `index` is the number of regions.
    pub fn boundary(&self, version: Version, index: usize) -> usize {
        self.regions.get(index).map_or_else(
            || self.line_count(version),
            |region| region.lines(version).start,
        )
    }

    /// The 0-based, half-open line range in `version` covered by `regions`.
    pub fn span_lines(&self, version: Version, regions: Range<usize>) -> Range<usize> {
        self.boundary(version, regions.start)..self.boundary(version, regions.end)
    }

    /// The byte range in `version` covered by `regions`.
    pub fn span_bytes(&self, version: Version, regions: Range<usize>) -> Range<usize> {
        self.bytes(version, self.span_lines(version, regions))
    }

    /// The bytes of `regions` in `version`.
    pub fn span_slice(&self, version: Version, regions: Range<usize>) -> &'a [u8] {
        &self.text(version)[self.span_bytes(version, regions)]
    }

    /// The byte range of the full lines in `version` that enclose `bytes`, or
    /// `bytes.start..bytes.start` when `bytes` is empty.
    pub fn line_span_bytes(&self, version: Version, bytes: Range<usize>) -> Range<usize> {
        if bytes.is_empty() {
            return bytes.start..bytes.start;
        }
        let first = self.line_at(version, bytes.start);
        let last = self.line_at(version, bytes.end - 1);
        self.bytes(version, first..last + 1)
    }

    /// Writes the merged file. Each span that takes a side is written as that
    /// side's lines; every other conflict is written as `conflicts` says.
    /// `spans` must be sorted and must not overlap.
    pub fn render(&self, spans: &[Span], conflicts: Conflicts) -> Rendered {
        let mut writer = Writer::default();
        let mut lines = Vec::with_capacity(spans.len());
        let mut markers = 0;
        let mut spans = spans.iter().peekable();
        let mut index = 0;

        while index < self.regions.len() {
            let Some(span) = spans.next_if(|span| span.regions.start == index) else {
                markers += self.write_region(&mut writer, &self.regions[index], conflicts);
                index += 1;
                continue;
            };
            let start = writer.position();
            match span.take {
                Some(version) => writer.write(self.span_slice(version, span.regions.clone())),
                None => {
                    for region in &self.regions[span.regions.clone()] {
                        markers += self.write_region(&mut writer, region, conflicts);
                    }
                }
            }
            lines.push(writer.lines_since(start));
            index = index.max(span.regions.end);
        }

        Rendered {
            text: writer.text,
            markers,
            lines,
        }
    }

    // Returns 1 when it wrote conflict markers.
    fn write_region(&self, writer: &mut Writer, region: &Region, conflicts: Conflicts) -> usize {
        match (region.resolution, conflicts) {
            (Resolution::Theirs, _) => {
                writer.write(self.slice(Version::Theirs, region.theirs.clone()));
            }
            (Resolution::Conflict, Conflicts::Fill(version)) => {
                writer.write(self.slice(version, region.lines(version)));
            }
            (Resolution::Conflict, Conflicts::Markers(size)) => {
                self.write_markers(writer, region, size);
                return 1;
            }
            _ => writer.write(self.slice(Version::Ours, region.ours.clone())),
        }
        0
    }

    fn write_markers(&self, writer: &mut Writer, region: &Region, size: u16) {
        let eol: &[u8] = if self.needs_cr(region) {
            b"\r\n"
        } else {
            b"\n"
        };
        writer.conflict(
            [
                self.slice(Version::Base, region.base.clone()),
                self.slice(Version::Ours, region.ours.clone()),
                self.slice(Version::Theirs, region.theirs.clone()),
            ],
            size,
            eol,
        );
    }

    // Git's rule (`is_cr_needed` in xdiff/xmerge.c): markers end in CRLF when
    // the lines before the conflict in ours and theirs (or their first lines)
    // and the first line of base do.
    fn needs_cr(&self, region: &Region) -> bool {
        let mut needs = self.ends_in_crlf(Version::Ours, region.ours.start.saturating_sub(1));
        if needs != Some(false) {
            needs = self.ends_in_crlf(Version::Theirs, region.theirs.start.saturating_sub(1));
        }
        if needs != Some(false) {
            needs = self.ends_in_crlf(Version::Base, 0);
        }
        needs == Some(true)
    }

    // `None` when the version cannot tell: it is empty, or its only line has
    // no line ending.
    fn ends_in_crlf(&self, version: Version, line: usize) -> Option<bool> {
        let count = self.line_count(version);
        let crlf = |line: usize| self.slice(version, line..line + 1).ends_with(b"\r\n");
        if line + 1 < count {
            return Some(crlf(line));
        }
        let last = count.checked_sub(1)?;
        if self.slice(version, last..count).ends_with(b"\n") {
            Some(crlf(last))
        } else {
            last.checked_sub(1).map(crlf)
        }
    }
}

fn line_starts<'line>(count: usize, line: impl Fn(usize) -> Option<&'line [u8]>) -> Vec<usize> {
    let mut starts = Vec::with_capacity(count + 1);
    let mut offset = 0;
    starts.push(offset);
    for index in 0..count {
        offset += line(index).map_or(0, <[u8]>::len);
        starts.push(offset);
    }
    starts
}

/// A run of regions in [`LineMerge::render`]: written from one side when
/// `take` is set, merged otherwise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub regions: Range<usize>,
    pub take: Option<Version>,
}

/// How [`LineMerge::render`] writes the conflicts outside spans that take a
/// side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflicts {
    /// Standard diff3 markers of the given size (Git's `%L`).
    Markers(u16),
    /// One version's lines, for checking that the rest of the file parses.
    Fill(Version),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub text: Vec<u8>,
    /// How many conflicts were written with markers.
    pub markers: usize,
    /// Where each span ended up in `text`, as 1-based, inclusive line numbers;
    /// `None` when it wrote nothing.
    pub lines: Vec<Option<RangeInclusive<usize>>>,
}

#[derive(Default)]
struct Writer {
    text: Vec<u8>,
    newlines: usize,
}

impl Writer {
    fn write(&mut self, bytes: &[u8]) {
        self.newlines += bytes.iter().filter(|&&byte| byte == b'\n').count();
        self.text.extend_from_slice(bytes);
    }

    fn end_line(&mut self, eol: &[u8]) {
        if self.text.last().is_some_and(|&byte| byte != b'\n') {
            self.write(eol);
        }
    }

    // Like Git, a missing final line ending is added inside a conflict.
    fn lines(&mut self, lines: &[u8], eol: &[u8]) {
        self.write(lines);
        if !lines.is_empty() {
            self.end_line(eol);
        }
    }

    fn marker(&mut self, marker: u8, size: u16, label: &str, eol: &[u8]) {
        self.text
            .extend(std::iter::repeat_n(marker, usize::from(size)));
        if !label.is_empty() {
            self.text.push(b' ');
            self.text.extend_from_slice(label.as_bytes());
        }
        self.write(eol);
    }

    fn conflict(&mut self, [base, ours, theirs]: [&[u8]; 3], size: u16, eol: &[u8]) {
        let [base_label, ours_label, theirs_label] = LABELS;
        self.end_line(eol);
        self.marker(b'<', size, ours_label, eol);
        self.lines(ours, eol);
        self.marker(b'|', size, base_label, eol);
        self.lines(base, eol);
        self.marker(b'=', size, "", eol);
        self.lines(theirs, eol);
        self.marker(b'>', size, theirs_label, eol);
    }

    fn position(&self) -> (usize, usize) {
        (self.text.len(), self.newlines)
    }

    fn lines_since(&self, (bytes, newlines): (usize, usize)) -> Option<RangeInclusive<usize>> {
        if self.text.len() == bytes {
            return None;
        }
        let last = if self.text.ends_with(b"\n") {
            self.newlines
        } else {
            self.newlines + 1
        };
        Some(newlines + 1..=last.max(newlines + 1))
    }
}

/// Replaces `output` with `contents` atomically, keeping its permissions.
pub(crate) fn write_output(output: &Path, contents: &[u8]) -> Result<()> {
    let permissions = fs::metadata(output)
        .with_context(|| format!("failed to inspect output file {}", output.display()))?
        .permissions();
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut replacement = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary output in {}", parent.display()))?;
    replacement
        .as_file()
        .set_permissions(permissions)
        .context("failed to preserve output file permissions")?;
    replacement
        .write_all(contents)
        .context("failed to write the merged output")?;
    replacement
        .as_file()
        .sync_all()
        .context("failed to sync the merged output")?;
    replacement
        .persist(output)
        .with_context(|| format!("failed to replace output file {}", output.display()))?;
    Ok(())
}

/// Writes one conflict over the whole file, all three versions in diff3
/// order. This is the fallback when the line merge cannot be trusted.
pub(crate) fn write_manual_conflict(
    output: &Path,
    versions: &Versions,
    marker_size: u16,
) -> Result<()> {
    ensure!(
        marker_size > 0,
        "conflict marker size must be greater than zero"
    );
    let conflict = render_conflict(
        &versions.base,
        &versions.ours,
        &versions.theirs,
        marker_size,
    );
    write_output(output, &conflict)
}

fn read_version(path: &Path, version: &str) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("failed to read {version} file {}", path.display()))
}

fn render_conflict(base: &[u8], ours: &[u8], theirs: &[u8], marker_size: u16) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.conflict([base, ours, theirs], marker_size, b"\n");
    writer.text
}

#[cfg(test)]
mod tests {
    use super::*;
    use similar::ConflictStyle;
    use tempfile::tempdir;

    fn versions(base: &str, ours: &str, theirs: &str) -> Versions {
        Versions {
            base: base.as_bytes().to_vec(),
            ours: ours.as_bytes().to_vec(),
            theirs: theirs.as_bytes().to_vec(),
        }
    }

    fn merged(versions: &Versions, marker_size: u16) -> (String, usize) {
        let rendered = LineMerge::new(versions)
            .unwrap()
            .render(&[], Conflicts::Markers(marker_size));
        (String::from_utf8(rendered.text).unwrap(), rendered.markers)
    }

    fn conflict_lines(merge: &LineMerge) -> Vec<[Range<usize>; 3]> {
        merge
            .regions()
            .iter()
            .filter(|region| region.is_conflict())
            .map(|region| {
                [
                    region.base.clone(),
                    region.ours.clone(),
                    region.theirs.clone(),
                ]
            })
            .collect()
    }

    #[test]
    fn combines_edits_that_do_not_overlap() {
        let versions = versions(
            "one\ntwo\nthree\nfour\n",
            "ONE\ntwo\nthree\nfour\n",
            "one\ntwo\nthree\nFOUR\n",
        );
        let merge = LineMerge::new(&versions).unwrap();

        assert!(merge.is_clean());
        assert_eq!(merge.conflicts(), 0);
        assert_eq!(
            merged(&versions, 7),
            ("ONE\ntwo\nthree\nFOUR\n".to_owned(), 0)
        );
    }

    #[test]
    fn counts_identical_edits_once() {
        let versions = versions("one\ntwo\n", "one\nTWO\nadded\n", "one\nTWO\nadded\n");

        assert_eq!(merged(&versions, 7), ("one\nTWO\nadded\n".to_owned(), 0));
    }

    #[test]
    fn reports_each_conflict_with_its_lines_in_every_version() {
        let versions = versions(
            "a\nb\nc\nd\ne\nf\n",
            "a\nB1\nc\nd\nE1\nf\n",
            "a\nB2\nc\nd\ne\nf\ng\n",
        );
        let merge = LineMerge::new(&versions).unwrap();

        assert_eq!(merge.conflicts(), 1);
        assert_eq!(conflict_lines(&merge), [[1..2, 1..2, 1..2]]);
        assert_eq!(
            merged(&versions, 7).0,
            "a\n<<<<<<< HEAD\nB1\n||||||| base\nb\n=======\nB2\n>>>>>>> theirs\nc\nd\nE1\nf\ng\n"
        );

        // Edits that touch each other conflict, as in Git.
        let touching = self::versions("a\nb\n", "a\nB\n", "a\nb\nc\n");
        assert_eq!(LineMerge::new(&touching).unwrap().conflicts(), 1);

        let versions = versions_with_two_conflicts();
        let merge = LineMerge::new(&versions).unwrap();
        assert_eq!(
            conflict_lines(&merge),
            [[1..2, 1..3, 1..2], [3..4, 4..5, 3..3]]
        );
    }

    fn versions_with_two_conflicts() -> Versions {
        versions("a\nb\nc\nd\ne\n", "a\nB1\nB1b\nc\nD1\ne\n", "a\nB2\nc\ne\n")
    }

    #[test]
    fn writes_diff3_markers_of_the_requested_size_around_each_conflict() {
        assert_eq!(
            merged(&versions_with_two_conflicts(), 3),
            (
                "a\n<<< HEAD\nB1\nB1b\n||| base\nb\n===\nB2\n>>> theirs\nc\n\
                 <<< HEAD\nD1\n||| base\nd\n===\n>>> theirs\ne\n"
                    .to_owned(),
                2
            )
        );
    }

    #[test]
    fn ends_markers_in_crlf_when_the_versions_do_like_git() {
        let crlf = versions("a\r\nb\r\nc\r\n", "a\r\nB1\r\nc\r\n", "a\r\nB2\r\nc\r\n");
        assert_eq!(
            merged(&crlf, 7).0,
            "a\r\n<<<<<<< HEAD\r\nB1\r\n||||||| base\r\nb\r\n=======\r\nB2\r\n>>>>>>> theirs\r\nc\r\n"
        );

        let mixed = versions("a\r\nb\r\nc\r\n", "a\r\nB1\r\nc\r\n", "a\nB2\nc\n");
        assert_eq!(
            merged(&mixed, 7).0,
            "<<<<<<< HEAD\na\r\nB1\r\nc\r\n||||||| base\na\r\nb\r\nc\r\n=======\na\nB2\nc\n>>>>>>> theirs\n"
        );
    }

    #[test]
    fn adds_a_missing_final_line_ending_inside_conflicts() {
        assert_eq!(
            merged(&versions("a\nb", "a\nours", "a\ntheirs"), 7).0,
            "a\n<<<<<<< HEAD\nours\n||||||| base\nb\n=======\ntheirs\n>>>>>>> theirs\n"
        );
        assert_eq!(
            merged(&versions("a\r\nb", "a\r\nours", "a\r\ntheirs"), 7).0,
            "a\r\n<<<<<<< HEAD\r\nours\r\n||||||| base\r\nb\r\n=======\r\ntheirs\r\n\
             >>>>>>> theirs\r\n"
        );
        assert_eq!(
            merged(&versions("", "ours", "theirs"), 7).0,
            "<<<<<<< HEAD\nours\n||||||| base\n=======\ntheirs\n>>>>>>> theirs\n"
        );
    }

    #[test]
    fn merges_input_that_is_not_utf8() {
        let versions = Versions {
            base: b"\xff\na\nb\n".to_vec(),
            ours: b"\xff\nA\nb\n".to_vec(),
            theirs: b"\xff\na\nb\n\xfe\n".to_vec(),
        };

        let rendered = LineMerge::new(&versions)
            .unwrap()
            .render(&[], Conflicts::Markers(7));

        assert_eq!(rendered.text, b"\xff\nA\nb\n\xfe\n");
        assert_eq!(rendered.markers, 0);
    }

    #[test]
    fn splits_unchanged_regions_into_lines() {
        let versions = versions("a\nb\nc\n", "a\nb\nC\n", "a\nb\nc\n");
        let merge = LineMerge::new(&versions).unwrap();

        let resolutions: Vec<_> = merge
            .regions()
            .iter()
            .map(|region| (region.resolution, region.base.clone()))
            .collect();
        assert_eq!(
            resolutions,
            [
                (Resolution::Unchanged, 0..1),
                (Resolution::Unchanged, 1..2),
                (Resolution::Ours, 2..3),
            ]
        );
        assert_eq!(merge.boundary(Version::Ours, 2), 2);
        assert_eq!(merge.boundary(Version::Ours, 3), 3);
        assert_eq!(merge.bytes(Version::Ours, 1..3), 2..6);
        assert_eq!(merge.line_at(Version::Ours, 3), 1);
        assert_eq!(merge.line_at(Version::Ours, 6), 3);
    }

    #[test]
    fn translates_region_spans_and_enclosing_lines_across_versions() {
        let empty = versions("", "", "");
        let empty_merge = LineMerge::new(&empty).unwrap();
        for version in Version::ALL {
            assert_eq!(empty_merge.span_lines(version, 0..0), 0..0);
            assert_eq!(empty_merge.span_bytes(version, 0..0), 0..0);
            assert_eq!(empty_merge.span_slice(version, 0..0), b"");
            assert_eq!(empty_merge.line_span_bytes(version, 0..0), 0..0);
        }

        let single = versions("solo", "solo", "solo");
        let single_merge = LineMerge::new(&single).unwrap();
        for version in Version::ALL {
            assert_eq!(single_merge.span_lines(version, 0..1), 0..1);
            assert_eq!(single_merge.span_bytes(version, 0..1), 0..4);
            assert_eq!(single_merge.span_slice(version, 0..1), b"solo");
            assert_eq!(single_merge.span_lines(version, 1..1), 1..1);
            assert_eq!(single_merge.span_bytes(version, 1..1), 4..4);
            assert_eq!(single_merge.span_slice(version, 1..1), b"");
            assert_eq!(single_merge.line_span_bytes(version, 1..3), 0..4);
            assert_eq!(single_merge.line_span_bytes(version, 4..4), 4..4);
        }

        let multi = versions("α\nβ\r\nδ\nγ", "α\nβ_ours\r\nδ\nγ", "α\nβ\r\nδ\nγ_theirs");
        let merge = LineMerge::new(&multi).unwrap();
        let region_count = merge.regions().len();
        assert_eq!(region_count, 4);

        // Region 0 is `"α\n"` (3 bytes: 2-byte UTF-8 `'α'` + `'\n'`).
        // Region 1 is `"β_ours\r\n"` in ours (9 bytes) and `"β\r\n"` in base/theirs (4 bytes).
        // Region 2 is `"δ\n"` (3 bytes).
        // Region 3 is `"γ"` in base/ours (2 bytes, no trailing newline) and `"γ_theirs"` in theirs (9 bytes).
        assert_eq!(merge.span_lines(Version::Ours, 0..2), 0..2);
        assert_eq!(merge.span_bytes(Version::Ours, 0..2), 0..12);
        assert_eq!(
            merge.span_slice(Version::Ours, 0..2),
            "α\nβ_ours\r\n".as_bytes()
        );
        assert_eq!(merge.span_lines(Version::Theirs, 1..4), 1..4);
        assert_eq!(merge.span_bytes(Version::Theirs, 1..4), 3..19);
        assert_eq!(
            merge.span_slice(Version::Theirs, 1..4),
            "β\r\nδ\nγ_theirs".as_bytes()
        );
        assert_eq!(
            merge.span_lines(Version::Base, region_count..region_count),
            4..4
        );
        assert_eq!(
            merge.span_bytes(Version::Base, region_count..region_count),
            12..12
        );
        assert_eq!(
            merge.span_slice(Version::Base, region_count..region_count),
            b""
        );

        // `line_span_bytes` widens interior byte offsets (including inside multi-byte
        // UTF-8 characters and on lines without trailing newlines) to full lines.
        assert_eq!(merge.line_span_bytes(Version::Ours, 1..2), 0..3);
        assert_eq!(merge.line_span_bytes(Version::Ours, 1..5), 0..12);
        assert_eq!(merge.line_span_bytes(Version::Ours, 15..16), 15..17);
        assert_eq!(merge.line_span_bytes(Version::Theirs, 13..19), 10..19);
        assert_eq!(merge.line_span_bytes(Version::Theirs, 10..10), 10..10);
        let reversed = std::hint::black_box(5)..std::hint::black_box(2);
        assert_eq!(merge.line_span_bytes(Version::Theirs, reversed), 5..5);
    }

    #[test]
    fn writes_spans_from_the_side_they_take_and_reports_where_they_landed() {
        let versions = versions_with_two_conflicts();
        let merge = LineMerge::new(&versions).unwrap();
        let conflicts: Vec<usize> = (0..merge.regions().len())
            .filter(|&index| merge.regions()[index].is_conflict())
            .collect();
        assert_eq!(conflicts.len(), 2);

        let rendered = merge.render(
            &[
                Span {
                    regions: conflicts[0]..conflicts[0] + 1,
                    take: Some(Version::Theirs),
                },
                Span {
                    regions: conflicts[1]..conflicts[1] + 1,
                    take: None,
                },
            ],
            Conflicts::Markers(7),
        );

        assert_eq!(
            String::from_utf8(rendered.text).unwrap(),
            "a\nB2\nc\n<<<<<<< HEAD\nD1\n||||||| base\nd\n=======\n>>>>>>> theirs\ne\n"
        );
        assert_eq!(rendered.markers, 1);
        assert_eq!(rendered.lines, [Some(2..=2), Some(4..=9)]);

        let deleted = merge.render(
            &[Span {
                regions: conflicts[1]..conflicts[1] + 1,
                take: Some(Version::Theirs),
            }],
            Conflicts::Fill(Version::Ours),
        );
        assert_eq!(
            String::from_utf8(deleted.text).unwrap(),
            "a\nB1\nB1b\nc\ne\n"
        );
        assert_eq!((deleted.markers, deleted.lines), (0, vec![None]));
    }

    struct Random(u64);

    impl Random {
        fn below(&mut self, limit: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % limit as u64) as usize
        }

        fn line(&mut self) -> &'static str {
            ["a\n", "b\n", "c\n", "{\n", "}\n", "\n", "    x();\n"][self.below(7)]
        }

        fn edit(&mut self, base: &[&'static str]) -> Vec<&'static str> {
            let mut lines = base.to_vec();
            for _ in 0..self.below(4) {
                let position = self.below(lines.len() + 1);
                match self.below(3) {
                    0 => lines.insert(position, self.line()),
                    1 if position < lines.len() => {
                        lines.remove(position);
                    }
                    _ if position < lines.len() => lines[position] = self.line(),
                    _ => {}
                }
            }
            lines
        }
    }

    #[test]
    fn stays_consistent_and_renders_like_similar_on_random_inputs() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        for _ in 0..3000 {
            let base: Vec<_> = (0..random.below(8)).map(|_| random.line()).collect();
            let (ours, theirs) = (random.edit(&base), random.edit(&base));
            let mut texts = [base.concat(), ours.concat(), theirs.concat()];
            if random.below(4) == 0 {
                let version = random.below(3);
                texts[version] = texts[version].trim_end_matches('\n').to_owned();
            }
            let [base, ours, theirs] = &texts;
            let versions = versions(base, ours, theirs);

            let merge = LineMerge::new(&versions).unwrap();
            let rendered = merge.render(&[], Conflicts::Markers(7));

            let mut expected = TextMerge::configure()
                .algorithm(Algorithm::Histogram)
                .merge_lines(base.as_bytes(), ours.as_bytes(), theirs.as_bytes());
            expected
                .conflict_style(ConflictStyle::Diff3)
                .labels("base", "HEAD", "theirs");
            let mut text = Vec::new();
            expected.to_writer(&mut text).unwrap();
            assert_eq!(rendered.text, text, "{texts:?}");
            assert_eq!(rendered.markers, expected.conflict_count(), "{texts:?}");
            if ours == base {
                assert_eq!(rendered.text, theirs.as_bytes(), "{texts:?}");
            }
            if theirs == base || ours == theirs {
                assert_eq!(rendered.text, ours.as_bytes(), "{texts:?}");
            }

            let take_ours: Vec<Span> = (0..merge.regions().len())
                .filter(|&index| merge.regions()[index].is_conflict())
                .map(|index| Span {
                    regions: index..index + 1,
                    take: Some(Version::Ours),
                })
                .collect();
            assert_eq!(
                merge.render(&take_ours, Conflicts::Markers(7)).text,
                merge.render(&[], Conflicts::Fill(Version::Ours)).text,
                "{texts:?}"
            );

            let region_count = merge.regions().len();
            for version in Version::ALL {
                let text = merge.text(version);
                assert_eq!(
                    merge.span_lines(version, 0..region_count),
                    0..merge.line_count(version)
                );
                assert_eq!(merge.span_bytes(version, 0..region_count), 0..text.len());
                assert_eq!(merge.span_slice(version, 0..region_count), text);

                let stitched: Vec<u8> = (0..region_count)
                    .flat_map(|index| merge.span_slice(version, index..index + 1))
                    .copied()
                    .collect();
                assert_eq!(stitched, text, "{texts:?}");

                for start in 0..=region_count {
                    for end in start..=region_count {
                        let expected_lines =
                            merge.boundary(version, start)..merge.boundary(version, end);
                        let expected_bytes = merge.bytes(version, expected_lines.clone());
                        assert_eq!(merge.span_lines(version, start..end), expected_lines);
                        assert_eq!(merge.span_bytes(version, start..end), expected_bytes);
                        assert_eq!(merge.span_slice(version, start..end), &text[expected_bytes]);
                    }
                }

                assert_eq!(
                    merge.line_span_bytes(version, text.len()..text.len()),
                    text.len()..text.len()
                );
                if !text.is_empty() {
                    let a = random.below(text.len());
                    let b = random.below(text.len());
                    let (lo, hi) = (a.min(b), a.max(b) + 1);
                    let enclosed = merge.line_span_bytes(version, lo..hi);
                    let expected_start = text[..lo]
                        .iter()
                        .rposition(|&byte| byte == b'\n')
                        .map_or(0, |newline| newline + 1);
                    let expected_end = text[hi - 1..]
                        .iter()
                        .position(|&byte| byte == b'\n')
                        .map_or(text.len(), |newline| hi + newline);
                    assert_eq!(enclosed, expected_start..expected_end, "{texts:?}");
                }
            }
        }
    }

    #[test]
    fn renders_all_three_versions_in_diff3_order() {
        assert_eq!(
            render_conflict(b"base\n", b"ours\n", b"theirs\n", 7),
            b"<<<<<<< HEAD\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> theirs\n"
        );
    }

    #[test]
    fn puts_markers_on_new_lines_for_unterminated_inputs() {
        assert_eq!(
            render_conflict(b"base", b"ours", b"theirs", 7),
            b"<<<<<<< HEAD\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> theirs\n"
        );
    }

    #[test]
    fn handles_empty_inputs() {
        assert_eq!(
            render_conflict(b"", b"", b"", 7),
            b"<<<<<<< HEAD\n||||||| base\n=======\n>>>>>>> theirs\n"
        );
    }

    #[test]
    fn preserves_non_utf8_bytes_and_existing_line_endings() {
        assert_eq!(
            render_conflict(b"base\r\n", b"\xff\r\n", b"theirs\r\n", 7),
            b"<<<<<<< HEAD\n\xff\r\n||||||| base\nbase\r\n=======\ntheirs\r\n>>>>>>> theirs\n"
        );
    }

    #[test]
    fn honors_custom_marker_size() {
        assert_eq!(
            render_conflict(b"base\n", b"ours\n", b"theirs\n", 3),
            b"<<< HEAD\nours\n||| base\nbase\n===\ntheirs\n>>> theirs\n"
        );
    }

    #[test]
    fn replaces_only_ours_and_leaves_no_temporary_files() {
        let directory = tempdir().unwrap();
        let base = directory.path().join("base");
        let ours = directory.path().join("ours");
        let theirs = directory.path().join("theirs");
        fs::write(&base, b"base\n").unwrap();
        fs::write(&ours, b"ours\n").unwrap();
        fs::write(&theirs, b"theirs\n").unwrap();

        let versions = Versions::read(&base, &ours, &theirs).unwrap();
        write_manual_conflict(&ours, &versions, 7).unwrap();

        assert_eq!(fs::read(&base).unwrap(), b"base\n");
        assert_eq!(fs::read(&theirs).unwrap(), b"theirs\n");
        assert_eq!(
            fs::read(&ours).unwrap(),
            render_conflict(b"base\n", b"ours\n", b"theirs\n", 7)
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 3);
    }

    #[test]
    fn missing_input_does_not_modify_ours() {
        let directory = tempdir().unwrap();
        let base = directory.path().join("base");
        let ours = directory.path().join("ours");
        let theirs = directory.path().join("theirs");
        fs::write(&base, b"base\n").unwrap();
        fs::write(&ours, b"ours\n").unwrap();

        let error = Versions::read(&base, &ours, &theirs).unwrap_err();

        assert!(error.to_string().contains("failed to read theirs file"));
        assert_eq!(fs::read(&ours).unwrap(), b"ours\n");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn rejects_zero_marker_size_before_touching_the_output() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("missing");
        let versions = Versions {
            base: b"base\n".to_vec(),
            ours: b"ours\n".to_vec(),
            theirs: b"theirs\n".to_vec(),
        };

        let error = write_manual_conflict(&output, &versions, 0).unwrap_err();

        assert_eq!(
            error.to_string(),
            "conflict marker size must be greater than zero"
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn preserves_ours_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let base = directory.path().join("base");
        let ours = directory.path().join("ours");
        let theirs = directory.path().join("theirs");
        fs::write(&base, b"base\n").unwrap();
        fs::write(&ours, b"ours\n").unwrap();
        fs::write(&theirs, b"theirs\n").unwrap();
        fs::set_permissions(&ours, fs::Permissions::from_mode(0o640)).unwrap();

        let versions = Versions::read(&base, &ours, &theirs).unwrap();
        write_manual_conflict(&ours, &versions, 7).unwrap();

        assert_eq!(
            fs::metadata(&ours).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
