//! Regression tests for the data-loss defects found while migrating from the
//! official C++ sync engine.
//!
//! Each test pins one lesson the hard way: the defect bit real folders before
//! it was understood. They are intentionally written against the planner and
//! the scanner, without any network, so they run everywhere.

use native_engine::db::DbEntry;
use native_engine::engine::{
    accept_remote_entry, check_delete_guard, filter_blacklisted, BLACKLIST_AFTER,
};
use native_engine::http::RemoteEntry;
use native_engine::local::{scan_local, LocalEntry};
use native_engine::reconcile::{reconcile, Action};
use native_engine::{EngineConfig, Provider};
use std::fs;
use std::path::Path;

fn config(root: &Path) -> EngineConfig {
    EngineConfig::new(
        Provider::Nextcloud,
        "https://example.com",
        "user",
        "pass",
        root,
        "",
    )
}

fn local(rel: &str, is_dir: bool, size: u64, mtime: i64) -> LocalEntry {
    LocalEntry {
        rel: rel.to_string(),
        is_dir,
        size,
        mtime,
        inode: 1,
    }
}

fn remote(rel: &str, is_dir: bool, size: u64, etag: &str) -> RemoteEntry {
    RemoteEntry {
        path: rel.to_string(),
        is_dir,
        size,
        etag: etag.to_string(),
        fileid: None,
        mtime: Some(0),
        checksum: None,
    }
}

fn journal(rel: &str, is_dir: bool, size: u64, mtime: i64, etag: &str) -> DbEntry {
    DbEntry {
        path: rel.to_string(),
        is_dir,
        size,
        mtime,
        etag: etag.to_string(),
        fileid: None,
        inode: 1,
    }
}

fn download_of(plan: &[Action], rel: &str) -> bool {
    plan.iter()
        .any(|action| matches!(action, Action::Download { rel: r, .. } if r == rel))
}

fn deletes(plan: &[Action]) -> usize {
    plan.iter()
        .filter(|action| {
            matches!(
                action,
                Action::DeleteLocal { .. } | Action::DeleteRemote { .. }
            )
        })
        .count()
}

/// A 114-byte file where the server has a folder replaced the folder with the
/// server's HTML page and made every child fail with `ENOTDIR`.
#[test]
fn a_local_file_never_turns_a_remote_collection_into_a_download() {
    let plan = reconcile(
        &[local("ai", false, 114, 100)],
        &[
            remote("ai", true, 0, "\"d1\""),
            remote("ai/memory.md", false, 10, "\"e1\""),
        ],
        &[],
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );

    assert!(
        !download_of(&plan, "ai"),
        "a collection must never be fetched as a file: {plan:?}"
    );
    assert!(plan
        .iter()
        .any(|action| matches!(action, Action::Conflict { rel } if rel == "ai")));
}

/// Reading a collection with GET returns the WebDAV HTML page. The transfer
/// layer must reject it instead of writing 114 bytes over a folder.
#[test]
fn the_webdav_page_is_never_stored_as_file_content() {
    let page = b"<?xml version=\"1.0\"?><html><body>This is the WebDAV interface. It can only be accessed by WebDAV clients such as the Nextcloud desktop sync client.</body></html>";
    assert!(native_engine::transfer::is_webdav_banner(page));
    assert!(!native_engine::transfer::is_webdav_banner(b"hola\n"));
    assert!(!native_engine::transfer::is_webdav_banner(&vec![
        b'x';
        4096
    ]));
}

/// The journal lives inside the synced folder. An earlier run uploaded it, and
/// reconciling it deleted the local copy.
#[test]
fn the_journal_never_takes_part_in_reconciliation() {
    let plan = reconcile(
        &[],
        &[
            remote(".nextsync", true, 0, "\"a\""),
            remote(".nextsync/state.db", false, 10, "\"b\""),
        ],
        &[journal(".nextsync/state.db", false, 10, 1, "\"b\"")],
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );

    assert_eq!(deletes(&plan), 0, "the journal must be invisible: {plan:?}");
}

/// A restored backup rewrites inodes without touching a byte. Treating the
/// inode as a change signal queued thousands of pointless uploads.
#[test]
fn an_inode_change_alone_is_not_a_local_change() {
    let mut current = local("notes.md", false, 10, 500);
    current.inode = 999;
    let plan = reconcile(
        &[current],
        &[remote("notes.md", false, 10, "\"e1\"")],
        &[journal("notes.md", false, 10, 500, "\"e1\"")],
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );

    assert!(
        !plan
            .iter()
            .any(|action| matches!(action, Action::Upload { .. })),
        "an inode is not content: {plan:?}"
    );
}

/// A partial remote listing used to be read as "these files vanished", so the
/// engine deleted them locally.
#[test]
fn an_incomplete_remote_scan_never_deletes_anything() {
    let plan = reconcile(
        &[],
        &[remote("gone.txt", false, 10, "\"e1\"")],
        &[journal("gone.txt", false, 10, 100, "\"e1\"")],
        false,
        native_engine::ConflictPolicy::KeepBoth,
    );

    assert_eq!(
        deletes(&plan),
        0,
        "partial listings must not delete: {plan:?}"
    );
}

/// Directories carry no content: a changed mtime only means a child appeared
/// or vanished. Conflicting them renamed whole trees on disk.
#[test]
fn directories_never_conflict() {
    let plan = reconcile(
        &[local("docs", true, 0, 200)],
        &[remote("docs", true, 0, "\"changed\"")],
        &[journal("docs", true, 0, 100, "\"old\"")],
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );

    assert!(
        !plan
            .iter()
            .any(|action| matches!(action, Action::Conflict { .. })),
        "a directory is not a conflict: {plan:?}"
    );
}

/// An entry that vanished on both sides leaves a stale journal row, nothing
/// more. Deleting either copy would be inventing a decision.
#[test]
fn an_entry_gone_on_both_sides_produces_no_action() {
    let plan = reconcile(
        &[],
        &[],
        &[journal("old.txt", false, 10, 100, "\"e1\"")],
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );

    assert!(plan.is_empty(), "nothing to do: {plan:?}");
}

/// Hidden entries are excluded on both sides. Filtering only the local scan
/// made every remote hidden file look deleted, which wiped them off the server.
#[test]
fn hidden_entries_and_their_children_are_ignored_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".comments")).unwrap();
    fs::write(root.join(".comments/note.xml"), b"x").unwrap();
    fs::write(root.join("visible.txt"), b"x").unwrap();

    let mut cfg = config(root);
    cfg.sync_hidden_files = false;
    let scanned = scan_local(root, &cfg).unwrap();
    assert!(scanned.iter().any(|entry| entry.rel == "visible.txt"));
    assert!(
        !scanned
            .iter()
            .any(|entry| entry.rel.starts_with(".comments")),
        "hidden folders and their children stay out: {scanned:?}"
    );

    assert!(native_engine::local::ignored(
        ".comments/note.xml",
        false,
        &cfg
    ));
    assert!(!native_engine::local::ignored("visible.txt", false, &cfg));
}

/// The engine writes its own state inside the folder, so a scan must never
/// report it as a file to upload.
#[test]
fn the_engine_state_directory_is_not_scanned() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".nextsync")).unwrap();
    fs::write(root.join(".nextsync/state.db"), b"db").unwrap();
    fs::write(root.join("file.txt"), b"x").unwrap();

    let scanned = scan_local(root, &config(root)).unwrap();
    assert_eq!(scanned.len(), 1, "only the real file: {scanned:?}");
    assert_eq!(scanned[0].rel, "file.txt");
}

/// A conflict name must keep the exact marker the application parses, and a
/// second conflict on the same file must not reuse the first name.
#[test]
fn conflict_names_stay_parseable_and_unique() {
    let path = Path::new("/tmp/Informe final.docx");
    let first = native_engine::engine::conflict_name(path).unwrap();
    let second = native_engine::engine::conflict_name_n(path, 2).unwrap();

    assert!(first.contains(" (Nextcloud conflicted copy "), "{first}");
    assert!(first.ends_with(".docx"), "{first}");
    assert_ne!(first, second);
}

// ---------------------------------------------------------------------------
// Guardas portadas del motor oficial en C++.
//
// Cada caso cita el fichero y la función de la que sale el comportamiento, para
// que un cambio futuro sepa contra qué referencia se validó.
// ---------------------------------------------------------------------------

fn entry(rel: &str, is_dir: bool, etag: &str) -> DbEntry {
    DbEntry {
        path: rel.to_string(),
        is_dir,
        size: 10,
        mtime: 100,
        etag: etag.to_string(),
        fileid: None,
        inode: 1,
    }
}

/// nc-desktop `src/libsync/syncengine.cpp`: cuando todo el árbol parece borrado
/// y ningún fichero se ha visto intacto, la pasada se aborta
/// (`_hasNoneFiles`/`_hasRemoveFile`).
#[test]
fn delete_guard_aborts_when_the_whole_tree_disappears() {
    let cfg = native_engine::DeleteGuardConfig::default();
    let plan = vec![
        Action::DeleteRemote { rel: "a".into() },
        Action::DeleteRemote { rel: "b".into() },
        Action::DeleteRemote { rel: "c".into() },
    ];
    let error = check_delete_guard(&cfg, &plan, 3, 3).expect_err("must abort");
    assert!(
        matches!(error, native_engine::Error::Safety(_)),
        "{error:?}"
    );
}

#[test]
fn delete_guard_allows_an_ordinary_deletion() {
    let cfg = native_engine::DeleteGuardConfig::default();
    let plan = vec![Action::DeleteRemote { rel: "a".into() }];
    check_delete_guard(&cfg, &plan, 100, 100).expect("one file of a hundred is fine");
}

/// El segundo disparo es absoluto: un borrado masivo en el servidor aborta
/// aunque el porcentaje sobre el árbol sea pequeño.
#[test]
fn delete_guard_aborts_on_a_server_stampede() {
    let cfg = native_engine::DeleteGuardConfig::default();
    let plan: Vec<Action> = (0..1200)
        .map(|i| Action::DeleteRemote {
            rel: format!("f{i}"),
        })
        .collect();
    let error = check_delete_guard(&cfg, &plan, 100_000, 100_000).expect_err("must abort");
    assert!(
        matches!(error, native_engine::Error::Safety(_)),
        "{error:?}"
    );
}

/// nc-desktop `syncengine.cpp::checkErrorBlacklisting`: tras varios errores
/// seguidos el elemento se deja de intentar hasta que cambia.
#[test]
fn blacklisted_download_is_skipped_until_the_etag_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db = native_engine::db::StateDb::open(&dir.path().join("state.db")).unwrap();
    for _ in 0..BLACKLIST_AFTER {
        db.record_failure("a.bin", "r:etag-1", "boom").unwrap();
    }
    let remote_entry = remote("a.bin", false, 10, "etag-1");
    let local_entries: Vec<LocalEntry> = Vec::new();
    let mut rem = std::collections::BTreeMap::new();
    rem.insert("a.bin", &remote_entry);
    let loc: std::collections::BTreeMap<&str, &LocalEntry> = std::collections::BTreeMap::new();

    let plan = vec![Action::Download {
        rel: "a.bin".into(),
        etag: "etag-1".into(),
        checksum: None,
        size: 10,
        mtime: Some(0),
    }];
    let (kept, skipped) = filter_blacklisted(plan, &loc, &rem, &db);
    assert_eq!(skipped, 1);
    assert!(kept.is_empty());

    // El servidor devuelve una versión distinta: se reintenta.
    let changed = remote("a.bin", false, 10, "etag-2");
    let mut rem2 = std::collections::BTreeMap::new();
    rem2.insert("a.bin", &changed);
    let plan = vec![Action::Download {
        rel: "a.bin".into(),
        etag: "etag-2".into(),
        checksum: None,
        size: 10,
        mtime: Some(0),
    }];
    let (kept, skipped) = filter_blacklisted(plan, &loc, &rem2, &db);
    assert_eq!(skipped, 0);
    assert_eq!(kept.len(), 1);
    let _ = local_entries;
}

#[test]
fn blacklisted_upload_expires_when_the_file_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db = native_engine::db::StateDb::open(&dir.path().join("state.db")).unwrap();
    let before = local("doc.txt", false, 10, 100);
    for _ in 0..BLACKLIST_AFTER {
        db.record_failure("doc.txt", "l:10:100", "denied").unwrap();
    }
    let loc1 = std::collections::BTreeMap::from([("doc.txt", &before)]);
    let rem: std::collections::BTreeMap<&str, &RemoteEntry> = std::collections::BTreeMap::new();
    let (kept, skipped) = filter_blacklisted(
        vec![Action::Upload {
            rel: "doc.txt".into(),
        }],
        &loc1,
        &rem,
        &db,
    );
    assert_eq!((kept.len(), skipped), (0, 1));

    let after = local("doc.txt", false, 11, 200);
    let loc2 = std::collections::BTreeMap::from([("doc.txt", &after)]);
    let (kept, skipped) = filter_blacklisted(
        vec![Action::Upload {
            rel: "doc.txt".into(),
        }],
        &loc2,
        &rem,
        &db,
    );
    assert_eq!((kept.len(), skipped), (1, 0));
}

/// nc-desktop `syncengine.cpp`: un fichero de conflicto ya subido y sin acción
/// pendiente se deja como está, no se vuelve a mover ni a renombrar.
#[test]
fn a_conflict_copy_present_on_both_sides_is_left_alone() {
    let name = "nota (Nextcloud conflicted copy 2026-11-05 10-00-00).txt";
    let local_entries = vec![local(name, false, 10, 100)];
    let remote_entries = vec![remote(name, false, 10, "e1")];
    let db_entries = vec![entry(name, false, "e1")];
    let plan = reconcile(
        &local_entries,
        &remote_entries,
        &db_entries,
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );
    assert!(
        plan.iter().all(|a| !a.is_write()),
        "a settled conflict copy must not be touched: {plan:?}"
    );
}

/// nc-desktop `discovery.cpp`: un cambio de tipo (directorio remoto, fichero
/// local) no puede borrar el árbol local ni los hijos remotos.
#[test]
fn a_local_file_never_deletes_a_remote_collection() {
    let local_entries = vec![local("docs", false, 114, 100)];
    let remote_entries = vec![
        remote("docs", true, 0, "d1"),
        remote("docs/a.txt", false, 10, "e1"),
        remote("docs/b.txt", false, 10, "e2"),
    ];
    let db_entries = vec![entry("docs", true, "d1")];
    let plan = reconcile(
        &local_entries,
        &remote_entries,
        &db_entries,
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );
    assert!(
        !plan
            .iter()
            .any(|a| matches!(a, Action::DeleteRemote { .. })),
        "remote children must survive: {plan:?}"
    );
    // Y el fichero local tampoco se reemplaza por la colección.
    assert!(
        !plan.iter().any(|a| matches!(a, Action::Download { .. })),
        "the banner page must never be downloaded: {plan:?}"
    );
}

// The C++ clients spell a path as a `QString` and never have to defend against
// a hostile href because the server is trusted. A Rust client that builds a
// `PathBuf` from a server string must validate it: a `..` segment escapes the
// synced folder (audit finding H1, 6-Nov-2026).

#[test]
fn unsafe_relative_paths_are_rejected() {
    for bad in [
        "..",
        "../outside.txt",
        "a/../../b",
        "/etc/passwd",
        "a//b",
        "a\\b",
        "a/\0b",
    ] {
        assert!(
            !native_engine::is_safe_rel(bad),
            "{bad:?} must not be accepted as a relative path"
        );
    }
    for good in [
        "",
        "a.txt",
        "docs/a.txt",
        "a..b/c.txt",
        "carpeta con espacios/x",
    ] {
        assert!(native_engine::is_safe_rel(good), "{good:?} is a valid path");
    }
}

// A `{:?}` of the engine configuration lands in logs and panic messages. The
// password must not be part of it (audit finding H2, same rule as issue #140).

#[test]
fn debug_output_redacts_the_password() {
    let mut cfg = config(Path::new("/tmp/redact"));
    cfg.password = "hunter2-super-secret".to_string();
    let text = format!("{cfg:?}");
    assert!(!text.contains("hunter2"), "password leaked: {text}");
    assert!(
        text.contains("[REDACTED]"),
        "redaction marker missing: {text}"
    );
}

/// A rename on the server must move the local file, not download a second copy
/// and delete the old one (nc-desktop `src/libsync/discovery.cpp` matches the
/// rename by `fileid`, which is why the engine stores `oc:fileid`).
#[test]
fn remote_rename_moves_the_local_file_instead_of_copying_it() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("old.txt"), b"hello").unwrap();

    let journal = vec![DbEntry {
        path: "old.txt".into(),
        is_dir: false,
        size: 5,
        mtime: 100,
        etag: "e-old".into(),
        fileid: Some("42".into()),
        inode: 1,
    }];
    let scanned = scan_local(root.path(), &config(root.path())).unwrap();

    let mut renamed = remote("new.txt", false, 5, "e-new");
    renamed.fileid = Some("42".into());

    let plan = reconcile(
        &scanned,
        &[renamed],
        &journal,
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );

    assert!(
        plan.iter().any(
            |a| matches!(a, Action::MoveLocal { from, to } if from == "old.txt" && to == "new.txt")
        ),
        "expected a local move, got {plan:?}"
    );
    assert!(
        !plan.iter().any(|a| matches!(a, Action::Download { .. })),
        "a rename must not download the file again: {plan:?}"
    );
    assert!(
        !plan.iter().any(|a| matches!(a, Action::DeleteLocal { .. })),
        "a rename must not delete the file it moves: {plan:?}"
    );
}

/// OpenCloud builds the TUS metadata as comma-separated `key base64value`
/// pairs (`oc-desktop src/libsync/propagateuploadtus.cpp:88`). Nextcloud has no
/// TUS at all (`nc-desktop src/libsync` ships only the chunked v2 upload), so
/// this is the only place where the two providers diverge on the wire.
#[test]
fn tus_metadata_matches_the_opencloud_layout() {
    let meta = native_engine::transfer::tus_metadata("docs/nota final.txt", 1_700_000_000);
    let mut parts = meta.split(", ");
    let first = parts.next().unwrap();
    let second = parts.next().unwrap();
    assert_eq!(second, "mtime 1700000000");
    let encoded = first.strip_prefix("filename ").unwrap();
    // base64 of the relative path, not of the absolute one: the space must be
    // encoded, and the value must not contain the separator.
    assert!(!encoded.contains(' '));
    assert!(!encoded.contains(','));
    assert_eq!(encoded, "ZG9jcy9ub3RhIGZpbmFsLnR4dA==");
}

/// A directory name with a comma would break the metadata if it were sent
/// unencoded; base64 keeps the header parseable.
#[test]
fn tus_metadata_survives_awkward_names() {
    let meta = native_engine::transfer::tus_metadata("a,b/c d.txt", 42);
    assert_eq!(meta.matches(", ").count(), 1);
    assert!(meta.ends_with("mtime 42"));
}

/// A conflict must never move a directory tree aside (Fotos, 6-Nov-2026).
#[test]
fn conflicts_never_rename_a_directory_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("Colegas");
    std::fs::create_dir_all(dir.join("2007 03 Windows Vista")).unwrap();
    let file = dir.join("foto.jpg");
    std::fs::write(&file, b"123").unwrap();
    assert!(!native_engine::engine::conflict_may_rename(
        &dir.join("2007 03 Windows Vista")
    ));
    assert!(native_engine::engine::conflict_may_rename(&file));
    // A path that does not exist is treated as a file: nothing to move.
    assert!(native_engine::engine::conflict_may_rename(
        &tmp.path().join("ausente.txt")
    ));
}

/// Issue #271: discovery must drop remote entries the local scan never
/// produces. Otherwise every hidden or excluded file that only exists on the
/// server is planned as a local deletion and destroyed there.
#[test]
fn remote_entries_the_local_scan_never_yields_are_rejected() {
    let root = std::env::temp_dir();
    let mut cfg = config(&root);
    cfg.sync_hidden_files = false;
    cfg.exclude_patterns = vec!["*.tmp".into(), "Cache/".into()];

    assert!(!accept_remote_entry(".comments", true, &cfg));
    assert!(!accept_remote_entry(".comments/note.xml", false, &cfg));
    assert!(!accept_remote_entry("a/.hidden/b.txt", false, &cfg));
    assert!(!accept_remote_entry("Fotos/scratch.tmp", false, &cfg));
    assert!(!accept_remote_entry("Cache", true, &cfg));

    assert!(accept_remote_entry("Fotos", true, &cfg));
    assert!(accept_remote_entry("Fotos/IMG_1.jpg", false, &cfg));
    assert!(accept_remote_entry("Fotos/.hidden", false, &cfg) == cfg.sync_hidden_files);
}

/// The same predicate must accept hidden paths when the user asked for them.
#[test]
fn hidden_remote_entries_are_kept_when_hidden_files_are_enabled() {
    let root = std::env::temp_dir();
    let mut cfg = config(&root);
    cfg.sync_hidden_files = true;

    assert!(accept_remote_entry(".comments", true, &cfg));
    assert!(accept_remote_entry("a/.hidden/b.txt", false, &cfg));
}

/// Issue #271: a hidden entry that the journal remembers but the local scan
/// no longer yields looks like a local deletion, so reconciliation asks the
/// server to delete it. Filtering the remote walk with the same predicate the
/// local scan uses keeps it out of the plan.
#[test]
fn a_journaled_hidden_entry_is_never_deleted_from_the_server() {
    let cfg = config(&std::env::temp_dir());
    let local_entries = [local("visible.txt", false, 10, 100)];
    let remote_entries = [
        remote(".comments", true, 0, "\"d1\""),
        remote("visible.txt", false, 10, "\"e1\""),
    ];
    let journal_entries = [
        journal(".comments", true, 0, 100, "\"d1\""),
        journal("visible.txt", false, 10, 100, "\"e1\""),
    ];

    // Without the filter the hidden directory is planned as a server deletion:
    // this is the behaviour that wiped 210 real files in production.
    let unfiltered = reconcile(
        &local_entries,
        &remote_entries,
        &journal_entries,
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );
    assert_eq!(deletes(&unfiltered), 1);

    let filtered: Vec<RemoteEntry> = [(".comments", true), ("visible.txt", false)]
        .into_iter()
        .filter(|(rel, is_dir)| accept_remote_entry(rel, *is_dir, &cfg))
        .map(|(rel, is_dir)| {
            let etag = if is_dir { "\"d1\"" } else { "\"e1\"" };
            remote(rel, is_dir, 0, etag)
        })
        .collect();
    let plan = reconcile(
        &local_entries,
        &filtered,
        &journal_entries,
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );
    assert_eq!(deletes(&plan), 0);
}

/// Issue #274: a path that is a file locally and a collection on the server
/// used to cascade into thousands of server deletions. Neither side may be
/// touched, and the collision has to reach the run report.
#[test]
fn a_file_local_against_a_collection_remote_touches_neither_side() {
    let plan = reconcile(
        &[local("Media", false, 114, 100)],
        &[
            remote("Media", true, 0, "\"d1\""),
            remote("Media/foto.jpg", false, 10, "\"e1\""),
        ],
        &[],
        true,
        native_engine::ConflictPolicy::KeepBoth,
    );

    let rendered = format!("{plan:?}");
    assert_eq!(deletes(&plan), 0, "the server keeps its tree: {rendered}");
    assert!(
        !rendered.contains("Upload"),
        "nothing is uploaded through a colliding path: {rendered}"
    );
    assert!(
        rendered.contains("Conflict"),
        "the collision is reported: {rendered}"
    );
}
