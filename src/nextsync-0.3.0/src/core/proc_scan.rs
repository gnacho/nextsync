//! Detection of an external sync engine already running on a folder.
//!
//! Issue #35: `nextcloudcmd`/`opencloudcmd` keep their own journals, and two
//! engines reconciling the same tree at once corrupt them. Before a run
//! starts the scheduler peeks at `/proc/*/cmdline` (a cheap scan of small
//! procfs files) and aborts with a clear error when another engine process
//! references the folder (or a parent/child of it) as one of its arguments.
//!
//! Only the binary name is ever surfaced; command lines never enter logs.

use std::path::Path;

use crate::core::sync_permit::{canonical_sync_root, paths_overlap};

/// Engine binaries whose command lines mark an external sync.
pub const ENGINE_BINARIES: [&str; 2] = ["nextcloudcmd", "opencloudcmd"];

/// Split a NUL-separated `/proc/<pid>/cmdline` payload into arguments.
pub fn split_cmdline(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect()
}

/// The final component of a path-like string (`argv[0]` binary name).
fn basename(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

/// Whether a command line belongs to a sync engine working on `root`.
///
/// `argv[0]` must be one of [`ENGINE_BINARIES`] and some other argument must
/// be an absolute path overlapping the canonical root — the engines take the
/// local folder as a positional argument (`nextcloudcmd ... <local> <url>`
/// and `opencloudcmd <url> <space> <local>`). Flags and URLs never look like
/// local paths, so the overlap comparison filters them out naturally.
pub fn engine_cmdline_matches_root(argv: &[String], root: &Path) -> bool {
    let Some(binary) = argv.first() else {
        return false;
    };
    if !ENGINE_BINARIES.contains(&basename(binary)) {
        return false;
    }
    let canonical = canonical_sync_root(root);
    argv.iter()
        .skip(1)
        .filter(|argument| argument.starts_with('/'))
        .any(|argument| paths_overlap(Path::new(argument), &canonical))
}

/// Scan a procfs-like directory for an engine process on `root`.
///
/// `self_pid` is skipped (our own children of the engine never appear here,
/// but the guard keeps the scan honest). Returns the binary name of the
/// first matching process. Entries that cannot be read are ignored.
pub fn find_external_engine_on_root_in(
    proc_root: &Path,
    self_pid: u32,
    root: &Path,
) -> Option<String> {
    let entries = std::fs::read_dir(proc_root).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let argv = split_cmdline(&bytes);
        if argv.is_empty() {
            continue;
        }
        if engine_cmdline_matches_root(&argv, root) {
            return Some(basename(&argv[0]).to_string());
        }
    }
    None
}

/// Production entry point: scan the real procfs for an engine on `root`.
pub fn find_external_engine_on_root(root: &Path) -> Option<String> {
    find_external_engine_on_root_in(Path::new("/proc"), std::process::id(), root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cmdline_file(dir: &Path, pid: &str, argv: &[&str]) {
        std::fs::create_dir_all(dir.join(pid)).expect("mkdir");
        let mut bytes = Vec::new();
        for argument in argv {
            bytes.extend_from_slice(argument.as_bytes());
            bytes.push(0);
        }
        std::fs::write(dir.join(pid).join("cmdline"), bytes).expect("write");
    }

    #[test]
    fn cmdline_splits_on_nuls_and_drops_the_trailing_empty() {
        let argv = split_cmdline(b"/usr/bin/nextcloudcmd\0--non-interactive\0/tmp/nc\0");
        assert_eq!(
            argv,
            ["/usr/bin/nextcloudcmd", "--non-interactive", "/tmp/nc"]
        );
        assert!(split_cmdline(b"").is_empty());
    }

    #[test]
    fn engine_on_the_same_folder_matches() {
        let root = Path::new("/tmp/nc");
        let argv: Vec<String> = ["/usr/bin/nextcloudcmd", "-h", "/tmp/nc", "https://cloud"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(engine_cmdline_matches_root(&argv, root));
    }

    #[test]
    fn engine_on_a_parent_or_child_matches() {
        let argv: Vec<String> = ["opencloudcmd", "https://cloud", "space-id", "/tmp/nc/docs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(engine_cmdline_matches_root(&argv, Path::new("/tmp/nc")));
        assert!(engine_cmdline_matches_root(
            &argv,
            Path::new("/tmp/nc/docs/file.txt")
        ));
        assert!(!engine_cmdline_matches_root(&argv, Path::new("/tmp/other")));
    }

    #[test]
    fn other_binaries_and_flag_values_never_match() {
        let root = Path::new("/tmp/nc");
        let unrelated: Vec<String> = ["/usr/bin/rsync", "-a", "/tmp/nc", "/backup"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(!engine_cmdline_matches_root(&unrelated, root));
        // The URL argument is not an absolute path, and the exclude file of a
        // different folder does not overlap.
        let flags: Vec<String> = [
            "/usr/bin/nextcloudcmd",
            "--exclude",
            "/tmp/other/rules",
            "/tmp/other",
            "https://cloud",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(!engine_cmdline_matches_root(&flags, root));
        assert!(engine_cmdline_matches_root(&flags, Path::new("/tmp/other")));
    }

    #[test]
    fn proc_scan_finds_the_engine_and_skips_self() {
        let dir = tempdir().expect("tempdir");
        let root = Path::new("/tmp/nc");
        cmdline_file(
            dir.path(),
            "42",
            &["/usr/bin/nextcloudcmd", "/tmp/nc", "https://c"],
        );
        cmdline_file(dir.path(), "43", &["/usr/bin/sleep", "60"]);
        let self_pid: u32 = "42".parse().unwrap();
        assert_eq!(
            find_external_engine_on_root_in(dir.path(), self_pid, root),
            None
        );
        assert_eq!(
            find_external_engine_on_root_in(dir.path(), 1, root),
            Some("nextcloudcmd".to_string())
        );
        assert_eq!(
            find_external_engine_on_root_in(dir.path(), 1, Path::new("/tmp/other")),
            None
        );
    }
}
