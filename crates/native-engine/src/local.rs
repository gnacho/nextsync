//! Local filesystem scan.

use crate::{EngineConfig, Error, Result};
use std::path::Path;

const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub inode: u64,
}

pub fn scan_local(root: &Path, cfg: &EngineConfig) -> Result<Vec<LocalEntry>> {
    let db_name = cfg.db_path();
    let mut out = Vec::new();
    walk(root, root, cfg, &mut out, 0, &db_name)?;
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

fn walk(
    root: &Path,
    dir: &Path,
    cfg: &EngineConfig,
    out: &mut Vec<LocalEntry>,
    depth: usize,
    db_path: &Path,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in rd {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| Error::Config("path outside root".into()))?
            .to_string_lossy()
            .replace('\\', "/");
        if rel.is_empty() {
            continue;
        }
        if path == db_path || rel == ".nextsync" || rel.starts_with(".nextsync/") {
            continue;
        }
        let is_dir = meta.is_dir();
        if excluded(&rel, &name, is_dir, cfg) {
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        out.push(LocalEntry {
            rel: rel.clone(),
            is_dir,
            size: if is_dir { 0 } else { meta.len() },
            mtime,
            inode: inode_of(&meta),
        });
        if is_dir {
            walk(root, &path, cfg, out, depth + 1, db_path)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn inode_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn inode_of(_meta: &std::fs::Metadata) -> u64 {
    0
}

/// Whether a relative path is outside the sync scope. Shared by the local scan
/// and the remote listing so both sides agree on what the engine ignores: an
/// entry missing from one side because it is ignored must never look like a
/// deletion (#271).
pub(crate) fn ignored(rel: &str, is_dir: bool, cfg: &EngineConfig) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    excluded(rel, name, is_dir, cfg)
}

fn excluded(rel: &str, name: &str, is_dir: bool, cfg: &EngineConfig) -> bool {
    // Check every segment, not just the leaf: when a hidden directory is out
    // of scope its children are too, and the remote listing still returns them.
    if !cfg.sync_hidden_files
        && rel
            .split('/')
            .any(|segment| segment.starts_with('.') && segment != ".")
    {
        return true;
    }
    cfg.exclude_patterns.iter().any(|p| {
        matches_pattern(p, rel)
            || matches_pattern(p, name)
            || (is_dir && matches_pattern(p, &format!("{rel}/")))
    })
}

/// Minimal glob: `*` (any run), `?` (one char). Everything else literal.
pub fn matches_pattern(pattern: &str, value: &str) -> bool {
    fn go(p: &[u8], v: &[u8]) -> bool {
        match p.first() {
            None => v.is_empty(),
            Some(b'*') => {
                let rest = &p[1..];
                if go(rest, v) {
                    return true;
                }
                let mut i = 0;
                while i < v.len() {
                    if go(rest, &v[i + 1..]) {
                        return true;
                    }
                    i += 1;
                }
                false
            }
            Some(b'?') => !v.is_empty() && go(&p[1..], &v[1..]),
            Some(c) => !v.is_empty() && v[0] == *c && go(&p[1..], &v[1..]),
        }
    }
    go(pattern.as_bytes(), value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;

    fn cfg(root: &Path) -> EngineConfig {
        EngineConfig::new(Provider::Nextcloud, "http://x", "u", "p", root, "")
    }

    fn touch(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn scans_and_skips_excluded_and_hidden() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("keep.txt"), "x");
        touch(&dir.path().join(".hidden"), "x");
        touch(&dir.path().join("node_modules/a.js"), "x");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let mut c = cfg(dir.path());
        c.exclude_patterns = vec!["node_modules".into()];
        let found = scan_local(dir.path(), &c).unwrap();
        let names: Vec<_> = found.iter().map(|e| e.rel.clone()).collect();
        assert!(names.contains(&"keep.txt".to_string()));
        assert!(names.contains(&"sub".to_string()));
        assert!(!names.iter().any(|n| n.contains("hidden")));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
    }

    #[test]
    fn sync_hidden_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join(".hidden"), "x");
        let mut c = cfg(dir.path());
        c.sync_hidden_files = true;
        let found = scan_local(dir.path(), &c).unwrap();
        assert!(found.iter().any(|e| e.rel == ".hidden"));
    }

    #[test]
    fn does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.txt"), "x");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
        let found = scan_local(dir.path(), &cfg(dir.path())).unwrap();
        assert!(!found.iter().any(|e| e.rel == "loop"));
    }

    #[test]
    fn glob() {
        assert!(matches_pattern("*.tmp", "a.tmp"));
        assert!(matches_pattern("build*", "build-x"));
        assert!(!matches_pattern("*.tmp", "a.txt"));
        assert!(matches_pattern("a?c", "abc"));
    }

    #[test]
    fn ignored_hidden_entries_and_their_children() {
        let mut config = cfg(Path::new("/tmp/nothing"));
        config.sync_hidden_files = false;
        assert!(ignored(".comments/x.xml", false, &config));
        assert!(ignored(".nextsync/state.db", false, &config));
        assert!(ignored(".mediaartlocal", true, &config));
        assert!(!ignored("Documents/notes.md", false, &config));
        assert!(!ignored(
            "a/.hidden/b.txt",
            false,
            &EngineConfig {
                sync_hidden_files: true,
                ..config.clone()
            }
        ));
    }

    #[test]
    fn ignored_entries_follow_exclude_patterns() {
        let mut config = cfg(Path::new("/tmp/nothing"));
        config.exclude_patterns = vec!["*.tmp".to_string(), "build".to_string()];
        assert!(ignored("a/b.tmp", false, &config));
        assert!(ignored("build", true, &config));
        assert!(!ignored("a/b.txt", false, &config));
    }
}
