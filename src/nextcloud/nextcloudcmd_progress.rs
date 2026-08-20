//! `nextcloudcmd` progress parser.
//!
//! Fase 2 (Task 2.3): parses the per-file operation lines that
//! `nextcloudcmd` prints on stdout and turns them into [`SyncProgress`]
//! events. Mirrors `nextcloud/nextcloudcmd_progress.py`: anything that does
//! not look like a file operation is returned as `None` so the caller can
//! forward the raw line verbatim without guessing.

/// One parsed progress event from `nextcloudcmd` output.
///
/// Mirrors `nextcloudcmd_progress.SyncProgress`: `processed` counts operations
/// reported so far in the current sync when a total is unavailable; it starts
/// at 1 for the first parsed line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncProgress {
    /// Normalized action: `download`, `upload`, `delete`, `conflict`, ...
    pub action: String,
    /// Path of the file being operated on.
    pub path: String,
    /// Number of operations reported so far in the current sync.
    pub processed: u32,
}

impl SyncProgress {
    /// Create a progress event with a zero operation counter.
    pub fn new(action: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            path: path.into(),
            processed: 0,
        }
    }

    /// Whether this event describes activity worth showing as live progress.
    pub fn is_operation(&self) -> bool {
        matches!(
            self.action.as_str(),
            "download" | "upload" | "delete" | "conflict" | "checking"
        )
    }

    /// Short human label, mirroring `nextcloudcmd_progress.describe_progress`.
    pub fn describe(&self) -> String {
        if self.is_operation() && self.processed > 0 {
            format!("{}: {} ({})", self.action, self.path, self.processed)
        } else {
            format!("{}: {}", self.action, self.path)
        }
    }
}

/// Normalized action for the raw action words printed by `nextcloudcmd`.
///
/// Mirror of the `_ACTION_ALIASES` mapping in `nextcloudcmd_progress.py`.
const ACTION_ALIASES: [(&str, &str); 13] = [
    ("downloading", "download"),
    ("download", "download"),
    ("download started", "download"),
    ("uploading", "upload"),
    ("upload", "upload"),
    ("upload started", "upload"),
    ("deleting", "delete"),
    ("delete", "delete"),
    ("removing", "delete"),
    ("synced", "synced"),
    ("skipped", "skipped"),
    ("conflict", "conflict"),
    ("conflicted copy", "conflict"),
];

/// Parse one `nextcloudcmd` stdout line into a [`SyncProgress`], if possible.
///
/// Two families of lines carry progress, both verified against real runs:
///
/// - the `action: path` shape of the original Python fixtures, and
/// - the Qt `qCDebug(lcPropagation)`-style lines the binary actually emits
///   (`Completed propagation of "x" by OCC::PropagateDownloadFile(...) with
///   status OCC::SyncFileItem::Success`, `discovered "x"
///   CSyncEnums::CSYNC_INSTRUCTION_NEW`, `STARTING "dir"`), which map to
///   transfer actions and to a generic `checking` phase.
///
/// Returns `None` for lines that do not look like progress so the caller can
/// forward them verbatim without guessing.
pub fn parse_progress_line(line: &str) -> Option<SyncProgress> {
    let stripped = strip_log_prefix(line.trim());
    if stripped.is_empty() {
        return None;
    }
    if let Some(progress) = parse_propagation_line(stripped) {
        return Some(progress);
    }
    if let Some(progress) = parse_discovery_line(stripped) {
        return Some(progress);
    }
    let colon = stripped.find(':')?;
    let action_raw = stripped[..colon].trim();
    let path = stripped[colon + 1..].trim().trim_matches('"');
    if action_raw.is_empty() || path.is_empty() {
        return None;
    }
    if !is_alphabetic_words(action_raw) {
        return None;
    }
    let raw_action = action_raw.to_ascii_lowercase();
    let action = ACTION_ALIASES
        .iter()
        .find_map(|(key, value)| (*key == raw_action).then_some(value.to_string()))?;
    Some(SyncProgress::new(action, path.to_string()))
}

/// Strip the Qt text-logging prefix the binary emits under
/// `QT_FORCE_STDERR_LOGGING`: a timestamp, `[ level category ]` and, for
/// debug lines, a second `[ function ]` bracket. Both shapes end the prefix
/// with `]:\t` right before the message, so everything up to the LAST
/// `]:\t` is prefix on lines that carry one.
fn strip_log_prefix(line: &str) -> &str {
    if !line.contains(" [ ") {
        return line;
    }
    match line.rfind("]:\t") {
        Some(position) => &line[position + 3..],
        None => line,
    }
}

/// `Completed propagation of "x" by OCC::PropagateDownloadFile(...) with
/// status OCC::SyncFileItem::Success`.
fn parse_propagation_line(line: &str) -> Option<SyncProgress> {
    let rest = line.strip_prefix("Completed propagation of \"")?;
    let (path, tail) = rest.split_once('"')?;
    if path.is_empty() {
        return None;
    }
    let action = if tail.contains("PropagateDownloadFile") {
        "download"
    } else if tail.contains("PropagateUploadFile") {
        "upload"
    } else if tail.contains("PropagateJob") || tail.contains("PropagateDirectory") {
        "checking"
    } else {
        return None;
    };
    Some(SyncProgress::new(action, path.to_string()))
}

/// `discovered "x" CSyncEnums::CSYNC_INSTRUCTION_NEW` and `STARTING "dir"`
/// (the discovery phase walking the tree).
fn parse_discovery_line(line: &str) -> Option<SyncProgress> {
    for prefix in ["discovered \"", "STARTING \""] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let (path, _) = rest.split_once('"')?;
            if !path.is_empty() {
                return Some(SyncProgress::new("checking", path.to_string()));
            }
        }
    }
    None
}

/// Short human label for a [`SyncProgress`] value (`""` for `None`).
pub fn describe_progress(progress: Option<&SyncProgress>) -> String {
    match progress {
        Some(progress) => progress.describe(),
        None => String::new(),
    }
}

/// Whether the text is `[A-Za-z]+(?:[ ][A-Za-z]+)*` (the Python action regex).
fn is_alphabetic_words(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut i = 0;
    let first = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == first {
        return false;
    }
    loop {
        if i == bytes.len() {
            return true;
        }
        if bytes[i] == b' ' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
        } else {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_download_line() {
        let progress = parse_progress_line("Downloading: /home/user/NextCloud/a.pdf");
        let progress = progress.expect("a download line should parse");
        assert_eq!(progress.action, "download");
        assert_eq!(progress.path, "/home/user/NextCloud/a.pdf");
    }

    #[test]
    fn parses_upload_line() {
        let progress = parse_progress_line("Uploading: docs/report.txt");
        let progress = progress.expect("an upload line should parse");
        assert_eq!(progress.action, "upload");
        assert_eq!(progress.path, "docs/report.txt");
    }

    #[test]
    fn parses_delete_line() {
        let progress = parse_progress_line("Deleting: /tmp/NextCloud/old.odt");
        let progress = progress.expect("a delete line should parse");
        assert_eq!(progress.action, "delete");
    }

    #[test]
    fn parses_synced_line_with_extra_spaces() {
        let progress = parse_progress_line("Synced  : /home/user/NextCloud/file.txt");
        let progress = progress.expect("a synced line should parse");
        assert_eq!(progress.action, "synced");
        assert_eq!(progress.path, "/home/user/NextCloud/file.txt");
    }

    #[test]
    fn ignores_non_operation_lines() {
        for line in [
            "",
            "  ",
            "Synchronizing folders",
            "Nextcloud synchronization completed",
            "Created journal: /tmp/foo/.sync.db",
            "exit 0",
            "14:22:03 [INFO] Database is ready",
        ] {
            assert!(
                parse_progress_line(line).is_none(),
                "should ignore: {line:?}"
            );
        }
    }

    #[test]
    fn ignores_unknown_actions() {
        assert!(parse_progress_line("Flurping: /tmp/file").is_none());
    }

    #[test]
    fn ignores_actions_with_run_on_spaces() {
        // Two spaces between action words do not match the Python regex.
        assert!(parse_progress_line("download  started: /tmp/a").is_none());
    }

    #[test]
    fn parses_real_propagation_lines() {
        // Verbatim lines captured from a nextcloudcmd run: transfers surface
        // as "Completed propagation" with the propagator naming the action.
        let download = parse_progress_line(
            "Completed propagation of \"Akon - Right Now Na Na Na.mp3\" by OCC::PropagateDownloadFile(0x55acf9c79ec0) with status OCC::SyncFileItem::Success",
        )
        .expect("a real download line should parse");
        assert_eq!(download.action, "download");
        assert_eq!(download.path, "Akon - Right Now Na Na Na.mp3");

        let upload = parse_progress_line(
            "Completed propagation of \"docs/report.txt\" by OCC::PropagateUploadFileV1(0x7f) with status OCC::SyncFileItem::Success",
        )
        .expect("a real upload line should parse");
        assert_eq!(upload.action, "upload");

        // Failures still name the propagator; the action is the transfer
        // attempt, which is what the live line reports.
        let failed = parse_progress_line(
            "Completed propagation of \"a.txt\" by OCC::PropagateDownloadFile(0x1) with status OCC::SyncFileItem::NormalError",
        )
        .expect("the propagator names the action even on failure");
        assert_eq!(failed.action, "download");
    }

    #[test]
    fn parses_real_discovery_lines() {
        // The discovery phase: directories walked and items found.
        let discovered = parse_progress_line(
            "discovered \"Proyectos/domatix/dashboards/package.json\" CSyncEnums::CSYNC_INSTRUCTION_CONFLICT OCC::SyncFileItem::None CSyncEnums::ItemTypeFile",
        )
        .expect("a real discovery line should parse");
        assert_eq!(discovered.action, "checking");
        assert_eq!(discovered.path, "Proyectos/domatix/dashboards/package.json");

        let starting = parse_progress_line(
            "STARTING \"Proyectos/domatix/dashboards/node_modules/framer-motion/dist/cjs\" OCC::ProcessDirectoryJob::NormalQuery \"Proyectos\" OCC::ProcessDirectoryJob::NormalQuery",
        )
        .expect("a real STARTING line should parse");
        assert_eq!(starting.action, "checking");
    }

    #[test]
    fn parses_qt_text_logging_prefixed_lines() {
        // Under QT_FORCE_STDERR_LOGGING every message carries a timestamp,
        // a `[ level category ]` bracket and (debug lines) a second
        // `[ function ]` bracket before the message itself.
        let starting = parse_progress_line(
            "08-20 17:29:00:572 [ info nextcloud.sync.discovery ]:\tSTARTING \"Gestion\" OCC::ProcessDirectoryJob::NormalQuery \"Gestion\" OCC::ProcessDirectoryJob::ParentDontExist",
        )
        .expect("a prefixed info line should parse");
        assert_eq!(starting.action, "checking");
        assert_eq!(starting.path, "Gestion");

        let completed = parse_progress_line(
            "08-20 17:30:12:750 [ debug nextcloud.sync.propagator ]\t[ OCC::PropagateDownloadFile::start ]:\tCompleted propagation of \"Akon - Right Now Na Na Na.mp3\" by OCC::PropagateDownloadFile(0x55acf9c79ec0) with status OCC::SyncFileItem::Success",
        )
        .expect("a prefixed debug propagation line should parse");
        assert_eq!(completed.action, "download");
        assert_eq!(completed.path, "Akon - Right Now Na Na Na.mp3");

        // The prefix must not break the plain colon format either.
        let plain =
            parse_progress_line("Downloading: /tmp/a.pdf").expect("plain line still parses");
        assert_eq!(plain.action, "download");
    }

    #[test]
    fn ignores_actions_with_punctuation() {
        assert!(parse_progress_line("14:22:03 [INFO] Database is ready").is_none());
    }

    #[test]
    fn processed_count_is_not_set_by_parser() {
        let progress = parse_progress_line("Downloading: /tmp/a");
        let progress = progress.expect("a download line should parse");
        assert_eq!(progress.processed, 0);
    }

    #[test]
    fn strips_quotes_around_path() {
        let progress = parse_progress_line("Uploading: \"/tmp/a file\"");
        let progress = progress.expect("an upload line should parse");
        assert_eq!(progress.path, "/tmp/a file");
    }

    #[test]
    fn describe_includes_count_when_known() {
        let mut progress = parse_progress_line("Downloading: /a.pdf").expect("parses");
        progress.processed = 7;
        assert_eq!(progress.describe(), "download: /a.pdf (7)");
        assert!(progress.describe().contains("/a.pdf"));
    }

    #[test]
    fn describe_without_count_omits_parenthesized_number() {
        let progress = parse_progress_line("Uploading: docs/report.txt").expect("parses");
        assert_eq!(progress.describe(), "upload: docs/report.txt");
    }

    #[test]
    fn describe_none_is_empty() {
        assert_eq!(describe_progress(None), "");
        let progress = parse_progress_line("Downloading: /a.pdf").expect("parses");
        assert_eq!(describe_progress(Some(&progress)), "download: /a.pdf");
    }

    #[test]
    fn synced_is_not_an_operation_but_still_describes() {
        let progress = parse_progress_line("Synced  : /tmp/NextCloud/file.txt").expect("parses");
        assert!(!progress.is_operation());
        assert_eq!(progress.describe(), "synced: /tmp/NextCloud/file.txt");
    }
}
