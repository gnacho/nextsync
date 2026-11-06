//! Sync orchestration.

use crate::db::{DbEntry, StateDb};
use crate::http::{percent_encode_path, HttpClient, HttpOptions, RemoteEntry};
use crate::local::{scan_local, LocalEntry};
use crate::reconcile::{reconcile, Action};
use crate::{
    CancelToken, EngineConfig, Error, ExitState, Progress, ProgressSink, Provider, Result,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Consecutive failed runs after which a path is skipped until its signature
/// changes. The official engine uses three (nc-desktop
/// `src/libsync/syncengine.cpp`, `checkErrorBlacklisting`).
pub const BLACKLIST_AFTER: u32 = 3;

/// Only files may be moved aside for a conflict. A directory carries a whole
/// tree: renaming it makes the next pass delete the remote collection.
pub fn conflict_may_rename(path: &std::path::Path) -> bool {
    !path.is_dir()
}

/// Identifies one version of a file so a blacklist entry expires when the file
/// changes. Avoids re-trying a broken transfer forever, and avoids skipping a
/// file that was fixed.
pub fn signature_for(
    action: &Action,
    local: &BTreeMap<&str, &LocalEntry>,
    remote: &BTreeMap<&str, &RemoteEntry>,
) -> String {
    match action {
        Action::Download { rel, etag, .. } => match remote.get(rel.as_str()) {
            Some(entry) if entry.etag == *etag => format!("r:{etag}"),
            // The etag moved since planning: the file changed, so the old
            // failure says nothing about this version.
            _ => String::new(),
        },
        Action::Upload { rel } => match local.get(rel.as_str()) {
            Some(entry) => format!("l:{}:{}", entry.size, entry.mtime),
            None => String::new(),
        },
        _ => String::new(),
    }
}

/// Refuses a plan that would wipe a large share of the tracked tree, or a
/// stampede of remote deletions. Modelled on the official engine, which aborts
/// when every file looks deleted and no file looked untouched
/// (nc-desktop `src/libsync/syncengine.cpp`, `_hasNoneFiles`/`_hasRemoveFile`).
pub fn check_delete_guard(
    cfg: &crate::DeleteGuardConfig,
    plan: &[Action],
    local_tracked: usize,
    known: usize,
) -> Result<()> {
    if !cfg.enabled {
        return Ok(());
    }
    let deletes = plan.iter().filter(|a| a.is_delete()).count();
    if deletes == 0 {
        return Ok(());
    }
    let remote_deletes = plan
        .iter()
        .filter(|a| matches!(a, Action::DeleteRemote { .. }))
        .count();
    if known > 0 && local_tracked == 0 {
        return Err(Error::Safety(
            "local folder is empty but the journal is not; refusing to delete".into(),
        ));
    }
    // Ratio over the tracked tree, not over the plan: a large folder with a
    // handful of deletions must still be measurable (#271).
    let total = plan.len().max(local_tracked).max(known).max(1);
    let ratio = deletes as f64 / total as f64;
    // Three independent trips:
    // - the whole tracked tree disappears at once (the signature of a broken
    //   or truncated remote listing, nc-desktop `_hasNoneFiles`),
    // - a large share of the tree, but only once it is a real group: a single
    //   deletion in a small folder is above any ratio and still legitimate,
    // - an absolute stampede on the server.
    const MASS_REMOTE_DELETE: usize = 1000;
    let whole_tree = deletes >= total && total > 0;
    let group_ratio = deletes >= cfg.min_group.max(1) && ratio > cfg.max_delete_ratio;
    if whole_tree || group_ratio || remote_deletes >= MASS_REMOTE_DELETE {
        return Err(Error::Safety(format!(
            "{deletes} of {total} tracked entries would be deleted ({remote_deletes} on the server, limit {:.0}% or {MASS_REMOTE_DELETE} server deletions)",
            cfg.max_delete_ratio * 100.0
        )));
    }
    Ok(())
}

/// Drops transfers that already failed [`BLACKLIST_AFTER`] runs in a row with
/// the same signature. Returns the filtered plan and how many entries went.
pub fn filter_blacklisted(
    plan: Vec<Action>,
    local: &BTreeMap<&str, &LocalEntry>,
    remote: &BTreeMap<&str, &RemoteEntry>,
    db: &StateDb,
) -> (Vec<Action>, usize) {
    let mut skipped = 0usize;
    let kept = plan
        .into_iter()
        .filter(|action| {
            let signature = signature_for(action, local, remote);
            if signature.is_empty() {
                return true;
            }
            match db.failure_attempts(action.rel(), &signature) {
                Ok(attempts) if attempts >= BLACKLIST_AFTER => {
                    log::debug!("{}: blacklisted after {attempts} failures", action.rel());
                    skipped += 1;
                    false
                }
                _ => true,
            }
        })
        .collect();
    (kept, skipped)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub exit: ExitState,
    pub downloaded: u32,
    pub uploaded: u32,
    pub deleted_local: u32,
    pub deleted_remote: u32,
    pub conflicts: u32,
    pub errors: u32,
    pub actions: usize,
}

#[allow(clippy::derivable_impls)]
impl Default for ExitState {
    fn default() -> Self {
        ExitState::Success
    }
}

pub struct Engine {
    cfg: EngineConfig,
}

struct Endpoints {
    /// WebDAV root of the synced folder, no trailing slash.
    remote_root: String,
    /// Upload staging root (Nextcloud chunked v2) or space root (OpenCloud TUS).
    upload_root: String,
    space_root: String,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self { cfg }
    }

    pub fn run(&self, sink: &dyn ProgressSink, cancel: &CancelToken) -> Result<SyncSummary> {
        let http = HttpClient::new(&HttpOptions {
            base_url: self.cfg.base_url.clone(),
            username: self.cfg.username.clone(),
            password: self.cfg.password.clone(),
            trust_invalid_certificates: self.cfg.trust_invalid_certificates,
            proxy: self.cfg.proxy.clone(),
            timeout: self.cfg.timeout,
        })?;
        let endpoints = self.endpoints(&http)?;
        std::fs::create_dir_all(&self.cfg.local_root)?;
        if !self.cfg.dry_run {
            self.ensure_remote_root(&http)?;
        }

        let db = StateDb::open(&self.cfg.db_path())?;
        let db_entries = db.load_all()?;

        sink.event(Progress::new("checking", "/"));
        // Fast path: when the sync root ETag did not move, the server tree is
        // the same one the journal already describes, so the full PROPFIND walk
        // can be skipped. Both Nextcloud and OpenCloud bump the root ETag when
        // any descendant changes, so this still sees nested edits.
        let root_etag = self.root_etag(&http, &endpoints)?;
        let reusable =
            !db_entries.is_empty() && root_etag.is_some() && db.get_meta("root_etag")? == root_etag;
        let (remote, complete) = if reusable {
            log::info!("root etag unchanged, reusing the journal instead of walking the tree");
            let entries = db_entries
                .iter()
                .map(|e| RemoteEntry {
                    path: e.path.clone(),
                    is_dir: e.is_dir,
                    size: e.size,
                    etag: e.etag.clone(),
                    fileid: e.fileid.clone(),
                    mtime: Some(e.mtime),
                    checksum: None,
                })
                .collect();
            (entries, true)
        } else {
            match self.discover_remote(&http, &endpoints, sink, cancel) {
                Ok(v) => v,
                Err(e) => {
                    // A failed or partial scan must never drive deletions.
                    log::warn!("remote discovery failed: {e}");
                    (Vec::new(), false)
                }
            }
        };
        // Drop everything the engine is configured to ignore before planning.
        // The local scan already skips these, so keeping them in the remote set
        // would turn every ignored entry into a server-side deletion (#271).
        let remote: Vec<RemoteEntry> = remote
            .into_iter()
            .filter(|entry| !crate::local::ignored(&entry.path, entry.is_dir, &self.cfg))
            .collect();
        let local = scan_local(&self.cfg.local_root, &self.cfg)?;
        let plan = reconcile(&local, &remote, &db_entries, complete);
        let loc: BTreeMap<&str, &LocalEntry> = local.iter().map(|e| (e.rel.as_str(), e)).collect();
        let rem: BTreeMap<&str, &RemoteEntry> =
            remote.iter().map(|e| (e.path.as_str(), e)).collect();
        // A transfer that already failed three runs in a row with the same
        // signature is left alone until the file changes. Without this, one
        // unreadable file burns a slot and a retry budget on every single run
        // (nc-desktop src/libsync/syncengine.cpp, checkErrorBlacklisting).
        let (plan, blacklisted) = filter_blacklisted(plan, &loc, &rem, &db);
        if blacklisted > 0 {
            log::warn!("{blacklisted} entries skipped: blacklisted after repeated failures");
        }

        let total_actions = plan.iter().filter(|a| a.is_write()).count();
        self.guard_deletes(&plan, &local, &db)?;

        let mut summary = SyncSummary {
            actions: plan.len(),
            ..Default::default()
        };

        if self.cfg.dry_run {
            for action in &plan {
                if action.is_write() {
                    summary.processed_dry(action, sink);
                }
            }
            summary.exit = if plan.iter().any(|a| matches!(a, Action::Conflict { .. })) {
                ExitState::Conflict
            } else {
                ExitState::Success
            };
            return Ok(summary);
        }

        let mut processed = 0u32;
        let mut refresh: BTreeSet<String> = BTreeSet::new();
        let mut download_jobs: Vec<&Action> = Vec::new();
        let mut upload_jobs: Vec<&Action> = Vec::new();

        for action in &plan {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if !action.is_write() {
                self.record(&db, action, &loc, &rem)?;
                continue;
            }
            if matches!(action, Action::Download { .. }) {
                // Downloads are independent of each other and dominate the
                // runtime of a cold pass, so they run later in a pool (issue
                // #268).
                download_jobs.push(action);
                continue;
            }
            if matches!(action, Action::Upload { .. }) {
                // Uploads are independent too, and the first pass of a new
                // server is dominated by them (issue #268).
                upload_jobs.push(action);
                continue;
            }
            processed += 1;
            match self.apply(
                &http,
                &endpoints,
                action,
                &loc,
                &rem,
                &db,
                sink,
                processed,
                cancel,
                &mut refresh,
            ) {
                Ok(()) => match action {
                    Action::Download { .. } => summary.downloaded += 1,
                    Action::Upload { .. } => summary.uploaded += 1,
                    Action::DeleteLocal { .. } => summary.deleted_local += 1,
                    Action::DeleteRemote { .. } => summary.deleted_remote += 1,
                    Action::Conflict { .. } => summary.conflicts += 1,
                    _ => {}
                },
                Err(Error::Auth(m)) => return Err(Error::Auth(m)),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    eprintln!("{} failed: {e}", action.rel());
                    summary.errors += 1;
                    continue;
                }
            }
        }

        if !download_jobs.is_empty() {
            let workers = std::env::var("NATIVE_DOWNLOAD_WORKERS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(4usize)
                .clamp(1, 16)
                .min(download_jobs.len());
            let next = std::sync::atomic::AtomicUsize::new(0);
            let done = std::sync::atomic::AtomicU32::new(0);
            let failures = std::sync::Mutex::new(Vec::<String>::new());
            let dirs = std::sync::Mutex::new(BTreeSet::<String>::new());
            let ok_paths = std::sync::Mutex::new(Vec::<String>::new());
            let failed_paths = std::sync::Mutex::new(Vec::<(String, String, String)>::new());
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    scope.spawn(|| loop {
                        if cancel.is_cancelled() {
                            break;
                        }
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let Some(action) = download_jobs.get(index) else {
                            break;
                        };
                        match self.run_download(&http, &endpoints, action, &rem, sink, cancel) {
                            Ok(dir) => {
                                done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                if let Ok(mut paths) = ok_paths.lock() {
                                    paths.push(action.rel().to_string());
                                }
                                if let Ok(mut dirs) = dirs.lock() {
                                    dirs.insert(dir);
                                }
                            }
                            Err(Error::Auth(m)) => {
                                if let Ok(mut failures) = failures.lock() {
                                    failures.push(format!("auth:{m}"));
                                }
                                break;
                            }
                            Err(Error::Cancelled) => break,
                            Err(e) => {
                                eprintln!("{} failed: {e}", action.rel());
                                if let Ok(mut paths) = failed_paths.lock() {
                                    paths.push((
                                        action.rel().to_string(),
                                        signature_for(action, &loc, &rem),
                                        e.to_string(),
                                    ));
                                }
                                if let Ok(mut failures) = failures.lock() {
                                    failures.push(e.to_string());
                                }
                            }
                        }
                    });
                }
            });
            summary.downloaded += done.load(std::sync::atomic::Ordering::SeqCst);
            if let Ok(paths) = ok_paths.into_inner() {
                for path in paths {
                    let _ = db.clear_failure(&path);
                }
            }
            if let Ok(entries) = failed_paths.into_inner() {
                for (path, signature, error) in entries {
                    if !signature.is_empty() {
                        let _ = db.record_failure(&path, &signature, &error);
                    }
                }
            }
            if let Ok(dirs) = dirs.into_inner() {
                refresh.extend(dirs);
            }
            if let Ok(failures) = failures.into_inner() {
                summary.errors += failures.len() as u32;
                if let Some(message) = failures
                    .iter()
                    .find_map(|f| f.strip_prefix("auth:").map(str::to_string))
                {
                    return Err(Error::Auth(message));
                }
            }
        }

        if !upload_jobs.is_empty() {
            let workers = std::env::var("NATIVE_UPLOAD_WORKERS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(4usize)
                .clamp(1, 8)
                .min(upload_jobs.len());
            let next = std::sync::atomic::AtomicUsize::new(0);
            let done = std::sync::atomic::AtomicU32::new(0);
            let failures = std::sync::Mutex::new(Vec::<String>::new());
            let dirs = std::sync::Mutex::new(BTreeSet::<String>::new());
            let ok_paths = std::sync::Mutex::new(Vec::<String>::new());
            let failed_paths = std::sync::Mutex::new(Vec::<(String, String, String)>::new());
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    scope.spawn(|| loop {
                        if cancel.is_cancelled() {
                            break;
                        }
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let Some(action) = upload_jobs.get(index) else {
                            break;
                        };
                        match self.run_upload(&http, &endpoints, action, &loc, sink, cancel) {
                            Ok(dir) => {
                                done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                if let Ok(mut paths) = ok_paths.lock() {
                                    paths.push(action.rel().to_string());
                                }
                                if let Ok(mut dirs) = dirs.lock() {
                                    dirs.insert(dir);
                                }
                            }
                            Err(Error::Auth(m)) => {
                                if let Ok(mut failures) = failures.lock() {
                                    failures.push(format!("auth:{m}"));
                                }
                                break;
                            }
                            Err(Error::Cancelled) => break,
                            Err(e) => {
                                eprintln!("{} failed: {e}", action.rel());
                                if let Ok(mut paths) = failed_paths.lock() {
                                    paths.push((
                                        action.rel().to_string(),
                                        signature_for(action, &loc, &rem),
                                        e.to_string(),
                                    ));
                                }
                                if let Ok(mut failures) = failures.lock() {
                                    failures.push(e.to_string());
                                }
                            }
                        }
                    });
                }
            });
            summary.uploaded += done.load(std::sync::atomic::Ordering::SeqCst);
            if let Ok(paths) = ok_paths.into_inner() {
                for path in paths {
                    let _ = db.clear_failure(&path);
                }
            }
            if let Ok(entries) = failed_paths.into_inner() {
                for (path, signature, error) in entries {
                    if !signature.is_empty() {
                        let _ = db.record_failure(&path, &signature, &error);
                    }
                }
            }
            if let Ok(dirs) = dirs.into_inner() {
                refresh.extend(dirs);
            }
            if let Ok(failures) = failures.into_inner() {
                summary.errors += failures.len() as u32;
                if let Some(message) = failures
                    .iter()
                    .find_map(|f| f.strip_prefix("auth:").map(str::to_string))
                {
                    return Err(Error::Auth(message));
                }
            }
        }

        // One Depth:1 listing per touched directory instead of one Depth:0
        // PROPFIND per transferred file (issue #268).
        for dir in &refresh {
            if let Err(e) = self.refresh_dir(&http, &endpoints, &db, dir) {
                log::warn!("refresh {dir}: {e}");
            }
        }

        summary.exit = if summary.errors > 0 {
            ExitState::Failed
        } else if summary.conflicts > 0 {
            ExitState::Conflict
        } else {
            ExitState::Success
        };
        if let Some(etag) = self.root_etag(&http, &endpoints)? {
            db.set_meta("root_etag", &etag)?;
        }
        db.set_meta("last_sync", &crate::http::now_epoch().to_string())?;
        let _ = total_actions;
        Ok(summary)
    }

    fn guard_deletes(&self, plan: &[Action], local: &[LocalEntry], db: &StateDb) -> Result<()> {
        check_delete_guard(&self.cfg.delete_guard, plan, local.len(), db.count()?)
    }

    fn run_download(
        &self,
        http: &HttpClient,
        ep: &Endpoints,
        action: &Action,
        rem: &BTreeMap<&str, &RemoteEntry>,
        sink: &dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<String> {
        let Action::Download {
            rel,
            etag,
            mtime,
            checksum,
            ..
        } = action
        else {
            return Err(Error::Config("not a download action".into()));
        };
        sink.event(Progress {
            action: "download".to_string(),
            path: rel.clone(),
            processed: 0,
            bytes_done: 0,
            bytes_total: 0,
        });
        let url = self.url(ep, rel);
        let dest = self.local_path(rel);
        let total = rem.get(rel.as_str()).map(|r| r.size).unwrap_or(0);
        crate::transfer::download(
            http,
            &url,
            &dest,
            Some(etag),
            checksum.as_deref(),
            *mtime,
            total,
            self.cfg.max_retries,
            cancel,
            sink,
            rel,
        )?;
        Ok(parent_dir(rel))
    }

    #[allow(clippy::too_many_arguments)]
    fn run_upload(
        &self,
        http: &HttpClient,
        ep: &Endpoints,
        action: &Action,
        loc: &BTreeMap<&str, &LocalEntry>,
        sink: &dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<String> {
        let Action::Upload { rel } = action else {
            return Err(Error::Config("not an upload action".into()));
        };
        sink.event(Progress {
            action: "upload".to_string(),
            path: rel.clone(),
            processed: 0,
            bytes_done: 0,
            bytes_total: 0,
        });
        let Some(entry) = loc.get(rel.as_str()) else {
            return Err(Error::Config(format!("missing local {rel}")));
        };
        let url = self.url(ep, rel);
        let path = self.local_path(rel);
        crate::transfer::upload(
            http,
            &path,
            &url,
            self.cfg.provider,
            &ep.upload_root,
            &ep.space_root,
            entry.mtime,
            self.cfg.chunk_threshold,
            self.cfg.max_retries,
            cancel,
            sink,
            rel,
        )?;
        Ok(parent_dir(rel))
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        http: &HttpClient,
        ep: &Endpoints,
        action: &Action,
        loc: &BTreeMap<&str, &LocalEntry>,
        rem: &BTreeMap<&str, &RemoteEntry>,
        db: &StateDb,
        sink: &dyn ProgressSink,
        processed: u32,
        cancel: &CancelToken,
        refresh: &mut BTreeSet<String>,
    ) -> Result<()> {
        let progress = |action: &str, path: &str| {
            sink.event(Progress {
                action: action.to_string(),
                path: path.to_string(),
                processed,
                bytes_done: 0,
                bytes_total: 0,
            });
        };
        match action {
            Action::RecordOnly { .. } => self.record(db, action, loc, rem),
            Action::MkdirLocal { rel } => {
                progress("mkdir", rel);
                std::fs::create_dir_all(self.local_path(rel))?;
                self.record(db, action, loc, rem)
            }
            Action::MkdirRemote { rel } => {
                progress("mkdir", rel);
                let _ = http.mkcol(&self.url(ep, rel));
                self.record(db, action, loc, rem)
            }
            Action::Download {
                rel,
                etag,
                mtime,
                checksum,
                ..
            } => {
                progress("download", rel);
                let url = self.url(ep, rel);
                let dest = self.local_path(rel);
                let total = rem.get(rel.as_str()).map(|r| r.size).unwrap_or(0);
                crate::transfer::download(
                    http,
                    &url,
                    &dest,
                    Some(etag),
                    checksum.as_deref(),
                    *mtime,
                    total,
                    self.cfg.max_retries,
                    cancel,
                    sink,
                    rel,
                )?;
                {
                    refresh.insert(parent_dir(rel));
                    Ok(())
                }
            }
            Action::Upload { rel } => {
                progress("upload", rel);
                let Some(entry) = loc.get(rel.as_str()) else {
                    return Err(Error::Config(format!("missing local {rel}")));
                };
                let url = self.url(ep, rel);
                let path = self.local_path(rel);
                crate::transfer::upload(
                    http,
                    &path,
                    &url,
                    self.cfg.provider,
                    &ep.upload_root,
                    &ep.space_root,
                    entry.mtime,
                    self.cfg.chunk_threshold,
                    self.cfg.max_retries,
                    cancel,
                    sink,
                    rel,
                )?;
                {
                    refresh.insert(parent_dir(rel));
                    Ok(())
                }
            }
            Action::DeleteRemote { rel } => {
                progress("delete", rel);
                http.delete(&self.url(ep, rel))?;
                db.remove(rel)?;
                Ok(())
            }
            Action::DeleteLocal { rel } => {
                progress("delete", rel);
                let path = self.local_path(rel);
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
                db.remove(rel)?;
                Ok(())
            }
            Action::MoveLocal { from, to } => {
                let src = self.local_path(from);
                let dst = self.local_path(to);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if src.exists() {
                    std::fs::rename(&src, &dst)?;
                }
                db.rename(from, to)?;
                Ok(())
            }
            Action::MoveRemote { from, to } => {
                let src = self.url(ep, from);
                let dst = self.url(ep, to);
                http.move_to(&src, &dst, true)?;
                db.rename(from, to)?;
                Ok(())
            }
            Action::Conflict { rel } => {
                progress("conflict", rel);
                let path = self.local_path(rel);
                let remote_is_dir = rem.get(rel.as_str()).map(|r| r.is_dir).unwrap_or(false);
                // A conflict on a directory is a planner artefact (a type
                // change or a stale journal), never user content. Renaming it
                // moves the whole tree aside and the next run then deletes the
                // remote collection. Files can be renamed, trees cannot.
                if !conflict_may_rename(&path) {
                    log::warn!("conflict on a directory left untouched: {rel}");
                    return Ok(());
                }
                if path.exists() {
                    let mut target = path.with_file_name(conflict_name(&path)?);
                    let mut n = 2;
                    while target.exists() {
                        target = path.with_file_name(conflict_name_n(&path, n)?);
                        n += 1;
                    }
                    std::fs::rename(&path, &target)?;
                }
                let (etag, mtime, checksum, total) = rem
                    .get(rel.as_str())
                    .map(|r| (r.etag.clone(), r.mtime, r.checksum.clone(), r.size))
                    .unwrap_or_default();
                let url = self.url(ep, rel);
                let dest = self.local_path(rel);
                if remote_is_dir {
                    // The remote side is a collection: fetching it with GET
                    // would return the server's HTML page, not a file.
                    std::fs::create_dir_all(&dest)?;
                    return Ok(());
                }
                crate::transfer::download(
                    http,
                    &url,
                    &dest,
                    Some(&etag),
                    checksum.as_deref(),
                    mtime,
                    total,
                    self.cfg.max_retries,
                    cancel,
                    sink,
                    rel,
                )?;
                {
                    refresh.insert(parent_dir(rel));
                    Ok(())
                }
            }
        }
    }

    fn record(
        &self,
        db: &StateDb,
        action: &Action,
        loc: &BTreeMap<&str, &LocalEntry>,
        rem: &BTreeMap<&str, &RemoteEntry>,
    ) -> Result<()> {
        let rel = action.rel();
        let l = loc.get(rel);
        let r = rem.get(rel);
        let entry = DbEntry {
            path: rel.to_string(),
            is_dir: l.map(|e| e.is_dir).or(r.map(|e| e.is_dir)).unwrap_or(false),
            size: l.map(|e| e.size).or(r.map(|e| e.size)).unwrap_or(0),
            mtime: l.map(|e| e.mtime).or(r.and_then(|e| e.mtime)).unwrap_or(0),
            etag: r.map(|e| e.etag.clone()).unwrap_or_default(),
            fileid: r.and_then(|e| e.fileid.clone()),
            inode: l.map(|e| e.inode).unwrap_or(0),
        };
        db.upsert(&entry)
    }

    /// Read back the authoritative remote state after a transfer.
    /// Refresh every entry of one directory with a single Depth:1 listing.
    fn refresh_dir(
        &self,
        http: &HttpClient,
        ep: &Endpoints,
        db: &StateDb,
        dir: &str,
    ) -> Result<()> {
        let url = if dir.is_empty() {
            ep.remote_root.clone()
        } else {
            format!("{}/", self.url(ep, dir))
        };
        let entries = http.propfind(&url, "1")?;
        for entry in entries {
            let rel = if dir.is_empty() {
                entry.path.clone()
            } else {
                format!("{dir}/{}", entry.path)
            };
            if rel.is_empty() {
                continue;
            }
            let path = self.local_path(&rel);
            let meta = std::fs::symlink_metadata(&path).ok();
            let is_dir = entry.is_dir;
            let size = meta
                .as_ref()
                .map(|m| if is_dir { 0 } else { m.len() })
                .unwrap_or(entry.size);
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(entry.mtime.unwrap_or(0));
            db.upsert(&DbEntry {
                path: rel,
                is_dir,
                size,
                mtime,
                etag: entry.etag.clone(),
                fileid: entry.fileid.clone(),
                inode: inode_of_meta(meta.as_ref()),
            })?;
        }
        Ok(())
    }

    fn ensure_remote_root(&self, http: &HttpClient) -> Result<()> {
        let segments: Vec<&str> = self
            .cfg
            .remote_path
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let mut current = match self.cfg.provider {
            Provider::Nextcloud => format!(
                "{}/remote.php/dav/files/{}",
                self.cfg.base_url,
                percent_encode_path(&self.cfg.username)
            ),
            Provider::OpenCloud => {
                let space = match &self.cfg.space_id {
                    Some(id) => id.clone(),
                    None => resolve_space(http, &self.cfg.base_url)?,
                };
                format!("{}/dav/spaces/{}", self.cfg.base_url, space)
            }
        };
        for seg in segments {
            current = format!("{current}/{}", percent_encode_path(seg));
            match http.mkcol(&current) {
                Ok(_) => {}
                Err(Error::Auth(m)) => return Err(Error::Auth(m)),
                Err(e) => log::warn!("mkcol {current}: {e}"),
            }
        }
        Ok(())
    }

    fn local_path(&self, rel: &str) -> PathBuf {
        // Defence in depth: the planner never emits an unsafe path (discovery
        // drops them), but a bug upstream must not be able to write outside
        // the synced folder.
        debug_assert!(crate::is_safe_rel(rel), "unsafe relative path: {rel:?}");
        self.cfg.local_root.join(rel)
    }

    fn url(&self, ep: &Endpoints, rel: &str) -> String {
        format!("{}/{}", ep.remote_root, percent_encode_path(rel))
    }

    fn endpoints(&self, http: &HttpClient) -> Result<Endpoints> {
        match self.cfg.provider {
            Provider::Nextcloud => {
                let base = format!(
                    "{}/remote.php/dav/files/{}",
                    self.cfg.base_url,
                    percent_encode_path(&self.cfg.username)
                );
                let root = join_remote(&base, &self.cfg.remote_path);
                Ok(Endpoints {
                    remote_root: root.clone(),
                    upload_root: format!(
                        "{}/remote.php/dav/uploads/{}",
                        self.cfg.base_url, self.cfg.username
                    ),
                    space_root: root,
                })
            }
            Provider::OpenCloud => {
                let space = match &self.cfg.space_id {
                    Some(id) => id.clone(),
                    None => resolve_space(http, &self.cfg.base_url)?,
                };
                let root = format!("{}/dav/spaces/{}", self.cfg.base_url, space);
                let root = join_remote(&root, &self.cfg.remote_path);
                Ok(Endpoints {
                    remote_root: root.clone(),
                    upload_root: format!("{}/dav/spaces/{}", self.cfg.base_url, space),
                    space_root: format!("{}/dav/spaces/{}", self.cfg.base_url, space),
                })
            }
        }
    }

    /// ETag of the sync root. `None` when the server does not report one or
    /// the probe fails, which disables the fast path for this run.
    fn root_etag(&self, http: &HttpClient, ep: &Endpoints) -> Result<Option<String>> {
        match http.propfind(&ep.remote_root, "0") {
            Ok(entries) => Ok(entries
                .first()
                .map(|e| e.etag.trim_matches('"').to_string())
                .filter(|etag| !etag.is_empty())),
            Err(Error::NotFound(_)) => Ok(None),
            Err(Error::Auth(m)) => Err(Error::Auth(m)),
            Err(e) => {
                log::warn!("root etag probe failed: {e}");
                Ok(None)
            }
        }
    }

    fn discover_remote(
        &self,
        http: &HttpClient,
        ep: &Endpoints,
        sink: &dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<(Vec<RemoteEntry>, bool)> {
        let mut out = Vec::new();
        let mut queue = vec![(String::new(), ep.remote_root.clone())];
        let mut complete = true;
        while let Some((prefix, url)) = queue.pop() {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            sink.event(Progress::new(
                "checking",
                if prefix.is_empty() { "/" } else { &prefix },
            ));
            let entries = match http.propfind(&url, "1") {
                Ok(e) => e,
                Err(Error::NotFound(_)) => {
                    return Ok((out, false));
                }
                Err(Error::Auth(m)) => return Err(Error::Auth(m)),
                Err(e) => {
                    log::warn!("propfind {url}: {e}");
                    complete = false;
                    continue;
                }
            };
            for mut entry in entries {
                let rel = if prefix.is_empty() {
                    entry.path.clone()
                } else {
                    format!("{prefix}/{}", entry.path)
                };
                entry.path = rel.clone();
                if !crate::is_safe_rel(&rel) {
                    // Never let a server-chosen path leave the synced folder.
                    log::warn!("ignoring unsafe remote path: {rel:?}");
                    complete = false;
                    continue;
                }
                if !accept_remote_entry(&rel, entry.is_dir, &self.cfg) {
                    // Issue #271: the local scan never yields hidden or
                    // excluded paths, so a remote entry the local side would
                    // not produce must not look like a local deletion.
                    continue;
                }
                if entry.is_dir {
                    queue.push((
                        rel.clone(),
                        format!("{}/{}", ep.remote_root, percent_encode_path(&rel)),
                    ));
                }
                out.push(entry);
            }
        }
        Ok((out, complete))
    }
}

/// Whether a remote entry may enter reconciliation. The local scan never
/// yields hidden or excluded paths, so letting them in would make every such
/// remote entry look like a local deletion and propagate it to the server
/// (issue #271).
pub fn accept_remote_entry(rel: &str, is_dir: bool, cfg: &EngineConfig) -> bool {
    crate::is_safe_rel(rel) && !crate::local::ignored(rel, is_dir, cfg)
}

impl SyncSummary {
    fn processed_dry(&mut self, action: &Action, sink: &dyn ProgressSink) {
        let (kind, path) = match action {
            Action::Download { rel, .. } => ("download", rel),
            Action::Upload { rel } => ("upload", rel),
            Action::DeleteLocal { rel } | Action::DeleteRemote { rel } => ("delete", rel),
            Action::Conflict { rel } => ("conflict", rel),
            Action::MoveLocal { to, .. } | Action::MoveRemote { to, .. } => ("move", to),
            Action::MkdirLocal { rel } | Action::MkdirRemote { rel } => ("mkdir", rel),
            Action::RecordOnly { .. } => return,
        };
        sink.event(Progress::new(kind, path));
        match kind {
            "download" => self.downloaded += 1,
            "upload" => self.uploaded += 1,
            "delete" => self.deleted_local += 1,
            "conflict" => self.conflicts += 1,
            _ => {}
        }
    }
}

/// Parent directory of a relative path, `""` for top-level entries.
fn parent_dir(rel: &str) -> String {
    rel.rsplit_once('/')
        .map(|(dir, _)| dir.to_string())
        .unwrap_or_default()
}

fn inode_of_meta(meta: Option<&std::fs::Metadata>) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.map(|m| m.ino()).unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

fn join_remote(base: &str, path: &str) -> String {
    let p = path.trim().trim_matches('/');
    if p.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{}", percent_encode_path(p))
    }
}

fn resolve_space(http: &HttpClient, base: &str) -> Result<String> {
    let url = format!("{base}/graph/v1.0/me/drives");
    let mut reader = http.get_reader(&url)?;
    let mut body = String::new();
    use std::io::Read;
    reader.read_to_string(&mut body)?;
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| Error::Config(format!("graph: {e}")))?;
    let drives = json
        .get("value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Config("graph: no drives".into()))?;
    let personal = drives
        .iter()
        .find(|d| d.get("driveType").and_then(|v| v.as_str()) == Some("personal"));
    let chosen = personal.or_else(|| {
        drives
            .iter()
            .find(|d| d.get("driveType").and_then(|v| v.as_str()) != Some("virtual"))
    });
    chosen
        .and_then(|d| d.get("id").and_then(|v| v.as_str()))
        .map(str::to_string)
        .ok_or_else(|| Error::Config("graph: no usable drive".into()))
}

/// Build the conflicted-copy name the app knows how to parse:
/// `stem (Nextcloud conflicted copy YYYY-MM-DD HH-MM-SS).ext`
pub fn conflict_name(path: &std::path::Path) -> Result<String> {
    conflict_name_n(path, 1)
}

pub fn conflict_name_n(path: &std::path::Path, n: u32) -> Result<String> {
    let suffix = if n == 1 {
        String::new()
    } else {
        format!(" {n}")
    };
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Config("invalid file name".into()))?;
    let ext = path.extension().and_then(|s| s.to_str());
    let now = crate::http::now_epoch();
    let d = civil_from_days(now / 86400);
    let secs = now % 86400;
    let date = format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        d.0,
        d.1,
        d.2,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    );
    Ok(match ext {
        Some(e) => format!("{stem} (Nextcloud conflicted copy {date}){suffix}.{e}"),
        None => format!("{stem} (Nextcloud conflicted copy {date}){suffix}"),
    })
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_name_is_parseable_by_the_app() {
        let name = conflict_name(std::path::Path::new("/tmp/Informe final.docx")).unwrap();
        assert!(name.starts_with("Informe final (Nextcloud conflicted copy "));
        assert!(name.ends_with(").docx"));
        // Replica of src/core/conflict_files.rs parsing rules.
        let marker = " (Nextcloud conflicted copy ";
        let idx = name.find(marker).unwrap();
        let after = &name[idx + marker.len()..];
        assert_eq!(&after[4..5], "-");
        assert_eq!(&after[7..8], "-");
        let close = after.find(')').unwrap();
        let date = &after[..close];
        assert_eq!(date.len(), 19, "date must be YYYY-MM-DD HH-MM-SS: {date}");
        assert_eq!(&name[idx + marker.len() + close + 1..], ".docx");
    }

    #[test]
    fn conflict_name_without_extension() {
        let name = conflict_name(std::path::Path::new("/tmp/LICENSE")).unwrap();
        assert!(name.ends_with(')'));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20730), (2026, 10, 4));
    }

    #[test]
    fn joins_remote_paths() {
        assert_eq!(join_remote("https://h/a", ""), "https://h/a");
        assert_eq!(
            join_remote("https://h/a", "/Docs/x y"),
            "https://h/a/Docs/x%20y"
        );
    }
}
