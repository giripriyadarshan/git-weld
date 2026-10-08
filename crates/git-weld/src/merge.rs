use anyhow::{Context, Result, ensure};
use std::{fs, io::Write, path::Path};
use tempfile::NamedTempFile;

pub fn write_manual_conflict(
    base: &Path,
    ours: &Path,
    theirs: &Path,
    marker_size: u16,
) -> Result<()> {
    ensure!(
        marker_size > 0,
        "conflict marker size must be greater than zero"
    );

    let base_content = read_version(base, "base")?;
    let ours_content = read_version(ours, "ours")?;
    let theirs_content = read_version(theirs, "theirs")?;
    let conflict = render_conflict(&base_content, &ours_content, &theirs_content, marker_size);

    let permissions = fs::metadata(ours)
        .with_context(|| format!("failed to inspect output file {}", ours.display()))?
        .permissions();
    let parent = ours
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
        .write_all(&conflict)
        .context("failed to write conflict output")?;
    replacement
        .as_file()
        .sync_all()
        .context("failed to sync conflict output")?;
    replacement
        .persist(ours)
        .with_context(|| format!("failed to replace output file {}", ours.display()))?;

    Ok(())
}

fn read_version(path: &Path, version: &str) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("failed to read {version} file {}", path.display()))
}

fn render_conflict(base: &[u8], ours: &[u8], theirs: &[u8], marker_size: u16) -> Vec<u8> {
    let mut output = Vec::new();
    append_marker(&mut output, b'<', marker_size, b"HEAD");
    append_version(&mut output, ours);
    append_marker(&mut output, b'|', marker_size, b"base");
    append_version(&mut output, base);
    append_marker(&mut output, b'=', marker_size, b"");
    append_version(&mut output, theirs);
    append_marker(&mut output, b'>', marker_size, b"theirs");
    output
}

fn append_marker(output: &mut Vec<u8>, marker: u8, size: u16, label: &[u8]) {
    output.extend(std::iter::repeat_n(marker, usize::from(size)));
    if !label.is_empty() {
        output.push(b' ');
        output.extend_from_slice(label);
    }
    output.push(b'\n');
}

fn append_version(output: &mut Vec<u8>, content: &[u8]) {
    output.extend_from_slice(content);
    if !content.is_empty() && !content.ends_with(b"\n") {
        output.push(b'\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

        write_manual_conflict(&base, &ours, &theirs, 7).unwrap();

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

        let error = write_manual_conflict(&base, &ours, &theirs, 7).unwrap_err();

        assert!(error.to_string().contains("failed to read theirs file"));
        assert_eq!(fs::read(&ours).unwrap(), b"ours\n");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn rejects_zero_marker_size_before_reading_files() {
        let missing = Path::new("missing");
        let error = write_manual_conflict(missing, missing, missing, 0).unwrap_err();
        assert_eq!(
            error.to_string(),
            "conflict marker size must be greater than zero"
        );
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

        write_manual_conflict(&base, &ours, &theirs, 7).unwrap();

        assert_eq!(
            fs::metadata(&ours).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
