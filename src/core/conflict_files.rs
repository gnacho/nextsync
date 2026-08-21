//! Conflicted-copy detection and resolution (Task 5.4).
//!
//! Mirrors `core/conflict_files.py` (v0.4.0): the sync engine preserves files
//! that changed on both sides as `<name> (Nextcloud conflicted copy <date>)
//! <ext>` and this module finds them, describes them and performs the local
//! resolution actions (`keep_local` / `keep_remote`). It never runs
//! `nextcloudcmd`; it only scans the synchronized folder.
//!
//! # Deviations from `conflict_files.py` (motivated)
//!
//! - **Exclusion matcher parameter**: `find_conflicts` takes an
//!   [`ExclusionMatcher`] and skips excluded names during the walk, keeping
//!   the conflict scan consistent with the watcher and the deletion guard
//!   (both agree with `nextcloudcmd`'s `--exclude`). The Python walker had no
//!   exclusions.
//! - **No progress callback**: the Python `find_conflicts(root, progress=...)`
//!   reports `(index, total)` while walking; the Rust signature drops it
//!   (nothing in the ported UI consumes it).
//! - **Manual parser instead of `re`**: the pattern is matched with a small
//!   hand-rolled scanner (the repo avoids the `regex` crate). Semantics
//!   replicate `CONFLICT_RE` including the non-greedy stem backtracking over
//!   repeated ` (Nextcloud conflicted copy ` markers and the
//!   `(?:\d{4}-\d{2}-\d{2}(?:[- ][\d: -]+)?)` date grammar.
//! - **`describe_modified` uses `glib::DateTime`** (already a dependency)
//!   instead of Python's `datetime.strftime("%x %H:%M")`; both render a
//!   locale-dependent short date + time.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::core::exclusions::ExclusionMatcher;
use crate::storage::config::expanduser;

/// The literal marker the engine inserts before the conflict timestamp,
/// matched case-insensitively (like the Python `re.IGNORECASE`).
const CONFLICT_MARKER: &str = " (Nextcloud conflicted copy ";

/// The parse result of one conflicted-copy name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictNameParts {
    /// Everything before the ` (Nextcloud conflicted copy ` marker.
    pub stem: String,
    /// The captured conflict timestamp (e.g. `2026-08-11 23-45-12`).
    pub date: String,
    /// The trailing extension (`.pdf`), when present.
    pub extension: Option<String>,
}

/// One `* (Nextcloud conflicted copy <date>).*` file found in a sync root.
#[derive(Debug, Clone, PartialEq)]
pub struct ConflictFile {
    /// Absolute path of the conflicted copy itself.
    pub path: PathBuf,
    /// The name the file would have had without the conflict suffix.
    pub original_name: String,
    /// The derived path of the "original" working file (may not exist).
    pub original_path: PathBuf,
    /// The raw conflict timestamp from the file name.
    pub conflict_date: String,
    /// Size in bytes.
    pub size: u64,
    /// Last modification time as seconds since the Unix epoch.
    pub modified: f64,
}

impl ConflictFile {
    /// The file name of the conflicted copy.
    pub fn name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    }

    /// One-line description: `original · conflicted copy name`.
    pub fn description(&self) -> String {
        format!("{} · {}", self.original_name, self.name())
    }
}

/// Try to parse a file name as a conflicted copy.
///
/// Replicates `CONFLICT_RE.match(name)`:
/// `^(?P<stem>.*?) (Nextcloud conflicted copy (?P<date>\d{4}-\d{2}-\d{2}(?:[- ][\d: -]+)?)\)(?P<extension>\.[^.]*)?$`
/// with `re.IGNORECASE`. Because the stem is non-greedy the first marker that
/// yields a full match wins (later markers are only tried if the first one
/// fails to reach end-of-string).
pub fn parse_conflict_name(name: &str) -> Option<ConflictNameParts> {
    let upper = name.to_ascii_uppercase();
    let marker = CONFLICT_MARKER.to_ascii_uppercase();
    let mut from = 0;
    while let Some(relative) = upper[from..].find(&marker) {
        let marker_start = from + relative;
        let stem = &name[..marker_start];
        let after = &name[marker_start + CONFLICT_MARKER.len()..];
        if let Some((date, rest)) = parse_date(after) {
            if let Some(rest) = rest.strip_prefix(')') {
                let (extension, tail) = parse_extension(rest);
                if tail.is_empty() {
                    return Some(ConflictNameParts {
                        stem: stem.to_string(),
                        date: date.to_string(),
                        extension: extension.map(str::to_string),
                    });
                }
            }
        }
        from = marker_start + 1;
    }
    None
}

/// Parse `\d{4}-\d{2}-\d{2}(?:[- ][\d: -]+)?` at the start of `rest`, returning
/// the matched date and the remainder.
fn parse_date(rest: &str) -> Option<(&str, &str)> {
    let bytes = rest.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let all_digits = |slice: &[u8]| slice.iter().all(u8::is_ascii_digit);
    if !all_digits(&bytes[..4]) || bytes[4] != b'-' {
        return None;
    }
    if !all_digits(&bytes[5..7]) || bytes[7] != b'-' {
        return None;
    }
    if !all_digits(&bytes[8..10]) {
        return None;
    }
    // Optional greedy tail `[- ][\d: -]+` (must contribute at least one char
    // past the separator, exactly like the regex backtracking).
    let mut end = 10;
    if bytes
        .get(10)
        .is_some_and(|byte| *byte == b'-' || *byte == b' ')
    {
        let mut run = 11;
        while run < bytes.len() && is_date_tail_char(bytes[run]) {
            run += 1;
        }
        if run > 11 {
            end = run;
        }
    }
    Some((&rest[..end], &rest[end..]))
}

/// Whether a byte belongs to `[\d: -]`, the greedy date-tail class.
fn is_date_tail_char(byte: u8) -> bool {
    byte.is_ascii_digit() || byte == b':' || byte == b' ' || byte == b'-'
}

/// Parse the optional extension `\.[^.]*` at the start of `rest`.
fn parse_extension(rest: &str) -> (Option<&str>, &str) {
    let Some(after_dot) = rest.strip_prefix('.') else {
        return (None, rest);
    };
    let end = after_dot.find('.').unwrap_or(after_dot.len());
    (Some(&rest[..end + 1]), &rest[end + 1..])
}

/// Walk a sync folder and return every conflicted copy inside it.
///
/// Only regular files matching the engine's naming pattern are returned; the
/// walk is iterative, never follows symlinks and skips excluded names. The
/// original path is derived from the file name (stripping the conflict
/// suffix), so the "original" may not exist locally.
pub fn find_conflicts(root: &Path, matcher: &ExclusionMatcher) -> Vec<ConflictFile> {
    let root = absolute_root(root);
    let mut matches: Vec<ConflictFile> = Vec::new();
    if !root.is_dir() {
        return matches;
    }

    let mut stack: Vec<PathBuf> = vec![root];
    while let Some(directory) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => continue,
            };
            if kind.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if matcher.matches_name(&name) {
                continue;
            }
            let Some(parts) = parse_conflict_name(&name) else {
                continue;
            };
            let path = entry.path();
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            matches.push(ConflictFile {
                path,
                original_name: original_name(&parts),
                original_path: derive_original_path(&entry.path(), &parts),
                conflict_date: parts.date,
                size: metadata.len(),
                modified: modified_seconds(&metadata),
            });
        }
    }
    matches.sort_by(|a, b| a.path.file_name().cmp(&b.path.file_name()));
    matches
}

/// Derive the original (conflict-free) file name from the parsed parts.
fn original_name(parts: &ConflictNameParts) -> String {
    let extension = parts.extension.clone().unwrap_or_default();
    if parts
        .stem
        .to_ascii_lowercase()
        .ends_with(&extension.to_ascii_lowercase())
    {
        parts.stem.clone()
    } else {
        format!("{}{}", parts.stem, extension)
    }
}

/// The sibling path the "original" would live at (the conflict suffix is
/// stripped from the name only; the folder stays the same).
fn derive_original_path(path: &Path, parts: &ConflictNameParts) -> PathBuf {
    path.with_file_name(original_name(parts))
}

/// Last modification time as `f64` seconds since the Unix epoch (0 when the
/// filesystem cannot report it).
fn modified_seconds(metadata: &fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

/// Return an ISO-ish timestamp for display from the conflicted-copy date.
pub fn conflict_iso_date(value: &str) -> String {
    value.trim().replace(' ', "T")
}

/// Delete the conflicted copy, keeping the working file in place.
pub fn keep_local(conflict: &ConflictFile) -> bool {
    match fs::remove_file(&conflict.path) {
        Ok(()) => true,
        // The copy may already be gone; keeping the working file still
        // "succeeded" from the user's point of view.
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// Replace the local working file with the conflicted copy.
///
/// The conflicted copy is the engine's version of the remote content; keeping
/// it promotes that content over whatever the local working file holds. The
/// modification time is preserved like `shutil.copy2`.
pub fn keep_remote(conflict: &ConflictFile) -> bool {
    (|| -> io::Result<()> {
        let source = fs::metadata(&conflict.path)?;
        let modified = source.modified().unwrap_or(UNIX_EPOCH);
        fs::copy(&conflict.path, &conflict.original_path)?;
        fs::File::options()
            .write(true)
            .open(&conflict.original_path)?
            .set_times(fs::FileTimes::new().set_modified(modified))?;
        // The conflicted copy was promoted; remove it so the next discovery
        // does not report the same conflict again (and the engine does not
        // reprocess it on the following run).
        match fs::remove_file(&conflict.path) {
            Ok(()) => {}
            // The copy may already be gone; promoting the content is what
            // matters.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
        Ok(())
    })()
    .is_ok()
}

/// Render a Unix timestamp as a locale short date + time (`%x %H:%M`).
///
/// Returns an empty string when the timestamp is out of range for the locale
/// (mirroring the Python `except (OSError, ValueError, OverflowError)`).
pub fn describe_modified(timestamp: f64) -> String {
    glib::DateTime::from_unix_local(timestamp as i64)
        .ok()
        .and_then(|date_time| date_time.format("%x %H:%M").ok())
        .map(|formatted| formatted.to_string())
        .unwrap_or_default()
}

/// Absolute, `~`-expanded form of a root path.
fn absolute_root(path: &Path) -> PathBuf {
    let expanded = expanduser(&path.to_string_lossy());
    std::path::absolute(&expanded).unwrap_or(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::{tempdir, TempDir};

    fn root() -> TempDir {
        tempdir().expect("tempdir works")
    }

    fn empty_matcher() -> ExclusionMatcher {
        ExclusionMatcher::new(Vec::<String>::new(), true)
    }

    fn write(base: &Path, name: &str) {
        let path = base.join(name);
        fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
        fs::write(&path, "content").expect("write");
    }

    fn name_of(conflicts: &[ConflictFile]) -> Vec<String> {
        conflicts
            .iter()
            .map(|conflict| conflict.name().to_string())
            .collect()
    }

    // ---- pattern ------------------------------------------------------------

    #[test]
    fn matches_engine_pattern() {
        let parts =
            parse_conflict_name("report.pdf (Nextcloud conflicted copy 2026-08-11 23-45-12).pdf")
                .expect("matches");
        assert_eq!(parts.stem, "report.pdf");
        assert_eq!(parts.extension.as_deref(), Some(".pdf"));
        assert_eq!(parts.date, "2026-08-11 23-45-12");
    }

    #[test]
    fn matches_date_only() {
        let parts = parse_conflict_name("notes (Nextcloud conflicted copy 2026-08-11).txt")
            .expect("matches");
        assert_eq!(parts.stem, "notes");
        assert_eq!(parts.date, "2026-08-11");
        assert_eq!(parts.extension.as_deref(), Some(".txt"));
    }

    #[test]
    fn matches_marker_case_insensitively() {
        let parts =
            parse_conflict_name("REPORT.PDF (NEXTCLOUD CONFLICTED COPY 2026-08-11 09-30-00).PDF")
                .expect("matches");
        assert_eq!(parts.stem, "REPORT.PDF");
        assert_eq!(parts.extension.as_deref(), Some(".PDF"));
        assert_eq!(parts.date, "2026-08-11 09-30-00");
    }

    #[test]
    fn does_not_match_plain_files() {
        for name in ["report.pdf", "conflicted copy.txt", "Nextcloud conflicted"] {
            assert!(
                parse_conflict_name(name).is_none(),
                "expected {name:?} to be rejected"
            );
        }
    }

    #[test]
    fn no_extension_ends_at_the_closing_paren() {
        let parts =
            parse_conflict_name("readme (Nextcloud conflicted copy 2026-08-11)").expect("matches");
        assert_eq!(parts.extension, None);
        assert_eq!(parts.stem, "readme");
    }

    #[test]
    fn repeated_marker_backtracks_to_the_last_complete_one() {
        let parts = parse_conflict_name(
            "a (Nextcloud conflicted copy 2026-08-11) b (Nextcloud conflicted copy 2026-08-12).txt",
        )
        .expect("matches");
        assert_eq!(parts.date, "2026-08-12");
        assert_eq!(parts.stem, "a (Nextcloud conflicted copy 2026-08-11) b");
        assert_eq!(parts.extension.as_deref(), Some(".txt"));
    }

    // ---- find_conflicts -----------------------------------------------------

    #[test]
    fn finds_only_conflicted_copies() {
        let dir = root();
        write(dir.path(), "normal.txt");
        write(
            dir.path(),
            "report.pdf (Nextcloud conflicted copy 2026-08-11 09-30-00).pdf",
        );
        write(
            dir.path(),
            "docs/note (Nextcloud conflicted copy 2026-08-10).md",
        );
        let names = name_of(&find_conflicts(dir.path(), &empty_matcher()));
        assert_eq!(names.len(), 2);
        assert!(names.contains(
            &"report.pdf (Nextcloud conflicted copy 2026-08-11 09-30-00).pdf".to_string()
        ));
        assert!(names.contains(&"note (Nextcloud conflicted copy 2026-08-10).md".to_string()));
    }

    #[test]
    fn original_path_is_derived() {
        let dir = root();
        let name = "report.pdf (Nextcloud conflicted copy 2026-08-11 09-30-00).pdf";
        write(dir.path(), name);
        let conflicts = find_conflicts(dir.path(), &empty_matcher());
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].original_name, "report.pdf");
        assert_eq!(conflicts[0].original_path, dir.path().join("report.pdf"));
        assert_eq!(conflicts[0].conflict_date, "2026-08-11 09-30-00");
    }

    #[test]
    fn empty_root_yields_no_conflicts() {
        assert!(find_conflicts(root().path(), &empty_matcher()).is_empty());
    }

    #[test]
    fn missing_root_yields_no_conflicts() {
        assert!(
            find_conflicts(Path::new("/nonexistent/conflict-root"), &empty_matcher()).is_empty()
        );
    }

    #[test]
    fn walk_skips_symlinks() {
        let dir = root();
        write(
            dir.path(),
            "real (Nextcloud conflicted copy 2026-08-11).txt",
        );
        let link = dir
            .path()
            .join("link (Nextcloud conflicted copy 2026-08-11).txt");
        let _ = std::os::unix::fs::symlink(
            dir.path()
                .join("real (Nextcloud conflicted copy 2026-08-11).txt"),
            &link,
        );
        let conflicts = find_conflicts(dir.path(), &empty_matcher());
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0].name(),
            "real (Nextcloud conflicted copy 2026-08-11).txt"
        );
    }

    #[test]
    fn excluded_names_are_skipped() {
        let dir = root();
        write(
            dir.path(),
            "draft.swp (Nextcloud conflicted copy 2026-08-11 09-30-00).swp",
        );
        write(
            dir.path(),
            "report.pdf (Nextcloud conflicted copy 2026-08-11 09-30-00).pdf",
        );
        let matcher = ExclusionMatcher::new(["*.swp"], true);
        let conflicts = find_conflicts(dir.path(), &matcher);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0].name(),
            "report.pdf (Nextcloud conflicted copy 2026-08-11 09-30-00).pdf"
        );
    }

    #[test]
    fn results_are_sorted_by_name() {
        let dir = root();
        write(
            dir.path(),
            "zeta (Nextcloud conflicted copy 2026-08-11).txt",
        );
        write(
            dir.path(),
            "alpha (Nextcloud conflicted copy 2026-08-11).txt",
        );
        let names = name_of(&find_conflicts(dir.path(), &empty_matcher()));
        assert_eq!(names.len(), 2);
        assert!(names[0] < names[1]);
    }

    // ---- keep_local / keep_remote ------------------------------------------

    #[test]
    fn keep_local_deletes_the_copy() {
        let dir = root();
        let name = "report.pdf (Nextcloud conflicted copy 2026-08-11 09-30-00).pdf";
        let path = dir.path().join(name);
        fs::write(&path, "b").expect("write");
        let original = dir.path().join("report.pdf");
        fs::write(&original, "keep-me").expect("write");
        let conflicts = find_conflicts(dir.path(), &empty_matcher());
        assert!(keep_local(&conflicts[0]));
        assert!(!path.exists());
        assert!(original.exists());
        assert_eq!(fs::read_to_string(&original).unwrap(), "keep-me");
    }

    #[test]
    fn keep_local_accepts_a_missing_copy() {
        let dir = root();
        let name = "report.pdf (Nextcloud conflicted copy 2026-08-11 09-30-00).pdf";
        let conflicts = find_conflicts(dir.path(), &empty_matcher());
        assert!(conflicts.is_empty());
        let conflict = ConflictFile {
            path: dir.path().join(name),
            original_name: "report.pdf".to_string(),
            original_path: dir.path().join("report.pdf"),
            conflict_date: "2026-08-11 09-30-00".to_string(),
            size: 0,
            modified: 0.0,
        };
        assert!(keep_local(&conflict));
    }

    #[test]
    fn keep_remote_replaces_the_working_file() {
        let dir = root();
        let name = "report.pdf (Nextcloud conflicted copy 2026-08-11 09-30-00).pdf";
        fs::write(dir.path().join(name), "remote-content").expect("write");
        let original = dir.path().join("report.pdf");
        fs::write(&original, "local-content").expect("write");
        let conflicts = find_conflicts(dir.path(), &empty_matcher());
        assert!(keep_remote(&conflicts[0]));
        assert!(original.exists());
        assert_eq!(fs::read_to_string(&original).unwrap(), "remote-content");
        assert!(!dir.path().join(name).exists());
    }

    // ---- helpers ------------------------------------------------------------

    #[test]
    fn conflict_iso_date_normalizes_separators() {
        assert_eq!(
            conflict_iso_date("2026-08-11 23-45-12"),
            "2026-08-11T23-45-12"
        );
        assert_eq!(conflict_iso_date(" 2026-08-11 "), "2026-08-11");
    }

    #[test]
    fn describe_modified_formats_a_valid_timestamp() {
        let rendered = describe_modified(0.0);
        assert!(!rendered.is_empty());
        assert!(rendered.contains(':'));
    }

    #[test]
    fn describe_modified_is_empty_outside_range() {
        // `f64::INFINITY as i64` saturates to `i64::MAX`, which GDateTime
        // cannot represent (the Python raises OverflowError for the same
        // input).
        assert!(describe_modified(f64::INFINITY).is_empty());
    }

    #[test]
    fn description_mentions_both_names() {
        let dir = root();
        let name = "report.pdf (Nextcloud conflicted copy 2026-08-11 09-30-00).pdf";
        write(dir.path(), name);
        let conflicts = find_conflicts(dir.path(), &empty_matcher());
        let description = conflicts[0].description();
        assert!(description.contains("report.pdf"));
        assert!(description.contains(name));
    }
}
