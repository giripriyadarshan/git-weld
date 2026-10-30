//! `.git-weld.log`: one JSON object per line for every decision a provider
//! made, so that editor plugins can show the reasoning next to the code.
//!
//! The driver appends to the file in its working directory, which is the top
//! of the work tree when Git runs it. Records are never rewritten, so the
//! newest record for a file and chunk is the one that applies.

use crate::provider::MergeAction;
use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    ops::RangeInclusive,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const FILE_NAME: &str = ".git-weld.log";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Record {
    /// When the merge ran, in UTC (RFC 3339), such as `2026-10-12T14:42:27Z`.
    pub(crate) time: String,
    /// The logical path of the merged file (`%P`).
    pub(crate) file: String,
    /// The chunk's number in the file, from 1.
    pub(crate) chunk: usize,
    /// The paths of the chunk's items, such as `impl Counter > fn get`.
    pub(crate) items: Vec<String>,
    /// Where the chunk is in the merged file, 1-based and inclusive: the
    /// chosen side's code when resolved, the conflict markers otherwise.
    pub(crate) lines: Option<Lines>,
    pub(crate) provider: String,
    pub(crate) decision: MergeAction,
    pub(crate) confidence: f64,
    pub(crate) reason: String,
    /// Whether git-weld replaced the chunk with the chosen side's code.
    pub(crate) resolved: bool,
    /// Whether the decision came from the local decision cache.
    pub(crate) cached: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Lines {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl From<RangeInclusive<usize>> for Lines {
    fn from(lines: RangeInclusive<usize>) -> Self {
        Self {
            start: *lines.start(),
            end: *lines.end(),
        }
    }
}

/// Appends `records` to the log at `path` in a single write, so records of
/// drivers that run at the same time do not interleave.
pub(crate) fn append(path: &Path, records: &[Record]) -> io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to write through a symlink",
        ));
    }
    let mut buffer = Vec::new();
    for record in records {
        serde_json::to_writer(&mut buffer, record)?;
        buffer.push(b'\n');
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&buffer)
}

/// The current time in UTC, formatted as RFC 3339.
pub(crate) fn now() -> String {
    timestamp(SystemTime::now())
}

fn timestamp(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let (days, second) = (seconds / 86_400, seconds % 86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second / 3600,
        second % 3600 / 60,
        second % 60
    )
}

// Howard Hinnant's `civil_from_days`: the Gregorian date `days` after
// 1970-01-01.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let days = days + 719_468;
    let (era, day_of_era) = (days / 146_097, days % 146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};
    use tempfile::tempdir;

    fn at(seconds: u64) -> String {
        timestamp(UNIX_EPOCH + Duration::from_secs(seconds))
    }

    fn record(chunk: usize, resolved: bool) -> Record {
        Record {
            time: "2026-10-12T14:42:27Z".to_owned(),
            file: "src/lib.rs".to_owned(),
            chunk,
            items: vec!["fn alpha".to_owned()],
            lines: Some(Lines::from(4..=6)),
            provider: "mock".to_owned(),
            decision: MergeAction::TakeOurs,
            confidence: 0.99,
            reason: "ours supersedes theirs".to_owned(),
            resolved,
            cached: false,
        }
    }

    #[test]
    fn formats_times_as_rfc_3339_in_utc() {
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(1_791_816_147), "2026-10-12T14:42:27Z");
        assert_eq!(at(1_709_251_199), "2024-02-29T23:59:59Z");
        assert_eq!(at(951_868_800), "2000-03-01T00:00:00Z");
        assert_eq!(now().len(), "2026-10-12T14:42:27Z".len());
    }

    #[test]
    fn appends_one_json_object_per_line() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);

        append(&path, &[record(1, true)]).unwrap();
        append(&path, &[]).unwrap();
        append(&path, &[record(2, false), record(3, false)]).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            serde_json::json!({
                "time": "2026-10-12T14:42:27Z",
                "file": "src/lib.rs",
                "chunk": 1,
                "items": ["fn alpha"],
                "lines": {"start": 4, "end": 6},
                "provider": "mock",
                "decision": "TakeOurs",
                "confidence": 0.99,
                "reason": "ours supersedes theirs",
                "resolved": true,
                "cached": false,
            })
        );
        assert_eq!(lines[2]["chunk"], 3);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn does_not_create_the_log_without_records() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);

        append(&path, &[]).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn reports_files_it_cannot_write() {
        let directory = tempdir().unwrap();

        let error = append(
            &directory.path().join("missing").join(FILE_NAME),
            &[record(1, true)],
        );

        assert_eq!(error.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_write_through_a_symlink() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("outside.txt");
        fs::write(&target, "unchanged\n").unwrap();
        let link = directory.path().join(FILE_NAME);
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = append(&link, &[record(1, true)]).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read_to_string(&target).unwrap(), "unchanged\n");
    }
}
