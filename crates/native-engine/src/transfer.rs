//! Streaming downloads and uploads, with retries and chunking.

use crate::http::HttpClient;
use crate::{CancelToken, Error, Progress, ProgressSink, Result};
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CHUNK: u64 = 10 * 1024 * 1024;
const CHECKSUM_LIMIT: u64 = 100 * 1024 * 1024;

/// Emits byte-level progress at most every 250 ms so the UI can show how much
/// is copied and how much is left without flooding the channel.
pub struct ProgressReporter<'a> {
    sink: &'a dyn ProgressSink,
    action: &'static str,
    path: String,
    total: u64,
    done: u64,
    last: std::time::Instant,
}

impl<'a> ProgressReporter<'a> {
    pub fn new(sink: &'a dyn ProgressSink, action: &'static str, path: &str, total: u64) -> Self {
        let mut reporter = Self {
            sink,
            action,
            path: path.to_string(),
            total,
            done: 0,
            last: std::time::Instant::now(),
        };
        reporter.emit();
        reporter
    }

    /// Continue from bytes already on disk (resumed downloads).
    pub fn resume_from(&mut self, done: u64) {
        self.done = done;
        self.emit();
    }

    pub fn add(&mut self, bytes: u64) {
        self.done += bytes;
        if self.last.elapsed() >= Duration::from_millis(250) {
            self.emit();
        }
    }

    pub fn finish(&mut self) {
        self.done = self.done.max(self.total);
        self.emit();
    }

    fn emit(&mut self) {
        self.sink
            .event(Progress::new(self.action, self.path.clone()).with_bytes(self.done, self.total));
        self.last = std::time::Instant::now();
    }
}

#[allow(clippy::too_many_arguments)]
pub fn download(
    client: &HttpClient,
    url: &str,
    dest: &Path,
    expected_etag: Option<&str>,
    expected_checksum: Option<&str>,
    mtime: Option<i64>,
    total: u64,
    max_retries: u32,
    cancel: &CancelToken,
    sink: &dyn ProgressSink,
    path: &str,
    multipart_threshold: u64,
    multipart_streams: u32,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_path(dest);
    let mut reporter = ProgressReporter::new(sink, "download", path, total);
    let mut last_err = None;
    for attempt in 0..=max_retries {
        if cancel.is_cancelled() {
            discard_part(&tmp);
            return Err(Error::Cancelled);
        }
        let outcome = if multipart_streams > 1
            && total >= multipart_threshold
            && ranges_supported(client, url)
        {
            try_download_ranges(
                client,
                url,
                &tmp,
                total,
                multipart_streams,
                expected_etag,
                &mut reporter,
            )
        } else {
            try_download(client, url, &tmp, expected_etag, &mut reporter)
        };
        match outcome {
            Ok(()) => {
                if let Some(expected) = expected_checksum {
                    let actual = sha1_hex(&tmp)?;
                    if !actual.eq_ignore_ascii_case(expected) {
                        discard_part(&tmp);
                        last_err = Some(Error::Transport("checksum mismatch".into()));
                        if attempt == max_retries {
                            break;
                        }
                        backoff(attempt);
                        continue;
                    }
                }
                set_mtime(&tmp, mtime)?;
                if std::fs::rename(&tmp, dest).is_err() {
                    let _ = std::fs::remove_file(dest);
                    std::fs::rename(&tmp, dest)?;
                }
                // The download is finished: the range bookkeeping is stale
                // and, left behind, it would upload as a stray file (#272).
                let _ = std::fs::remove_file(ranges_sidecar(&tmp));
                let _ = std::fs::remove_file(version_sidecar(&tmp));
                return Ok(());
            }
            Err(e) => {
                discard_part(&tmp);
                let retry = retryable(&e);
                last_err = Some(e);
                if !retry || attempt == max_retries {
                    break;
                }
                backoff(attempt);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| Error::Transport("download failed".into())))
}

/// Whether the server honours byte ranges (issue #272).
fn ranges_supported(client: &HttpClient, url: &str) -> bool {
    matches!(client.get_range(url, 0, 0), Ok((_, 206)))
}

/// Download `url` as parallel byte ranges into `tmp`, skipping the ranges a
/// previous attempt already finished (issue #272).
fn try_download_ranges(
    client: &HttpClient,
    url: &str,
    tmp: &Path,
    total: u64,
    streams: u32,
    expected_etag: Option<&str>,
    reporter: &mut ProgressReporter,
) -> Result<()> {
    let plan = split_ranges(total, streams);
    // Bytes from another version of the file must never be mixed in (#272).
    if version_of(tmp).as_deref() != expected_etag {
        discard_part(tmp);
    }
    let done = completed_ranges(tmp);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(tmp)?;
    file.set_len(total)?;
    remember_version(tmp, expected_etag)?;

    let mut already = 0u64;
    for range in &plan {
        if done.contains(range) {
            already += range.1 - range.0 + 1;
        }
    }
    if already > 0 {
        reporter.resume_from(already);
    }

    let pending: Vec<(u64, u64)> = plan.iter().copied().filter(|r| !done.contains(r)).collect();
    let failure: std::sync::Mutex<Option<Error>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for (start, end) in pending {
            let file = &file;
            let failure = &failure;
            scope.spawn(move || {
                if failure.lock().map(|slot| slot.is_some()).unwrap_or(true) {
                    return;
                }
                if let Err(error) = fetch_range(client, url, tmp, file, start, end) {
                    if let Ok(mut slot) = failure.lock() {
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                    }
                }
            });
        }
    });
    if let Some(error) = failure.into_inner().unwrap_or(None) {
        return Err(error);
    }
    file.sync_all()?;
    reporter.add(total.saturating_sub(already));
    Ok(())
}

/// Fetch one byte range straight into its offset inside the part file.
fn fetch_range(
    client: &HttpClient,
    url: &str,
    tmp: &Path,
    file: &std::fs::File,
    start: u64,
    end: u64,
) -> Result<()> {
    use std::os::unix::fs::FileExt;

    let (mut reader, status) = client.get_range(url, start, end)?;
    if status != 206 {
        return Err(Error::Transport(format!(
            "server ignored the byte range {start}-{end}"
        )));
    }
    let mut buffer = vec![0u8; 64 * 1024];
    let mut offset = start;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all_at(&buffer[..read], offset)?;
        offset += read as u64;
    }
    if offset != end + 1 {
        return Err(Error::Transport(format!(
            "range {start}-{end} came back short"
        )));
    }
    mark_range_complete(tmp, (start, end))?;
    Ok(())
}

fn try_download(
    client: &HttpClient,
    url: &str,
    tmp: &Path,
    expected: Option<&str>,
    reporter: &mut ProgressReporter,
) -> Result<()> {
    // Resume a partial download when a previous attempt left bytes behind.
    let offset = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
    let (mut reader, status) = client.get_reader_from(url, offset)?;
    let mut out = if offset > 0 && status == 206 {
        reporter.resume_from(offset);
        std::fs::OpenOptions::new().append(true).open(tmp)?
    } else {
        if offset > 0 {
            reporter.resume_from(0);
        }
        std::fs::File::create(tmp)?
    };
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        reporter.add(n as u64);
    }
    out.sync_all()?;
    // A WebDAV server answers `GET` on a collection with an HTML page rather
    // than an error. Writing that page as the file content replaces a folder
    // with a 114-byte file, and every child of it then fails with ENOTDIR.
    let len = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
    if len < 512 {
        if let Ok(bytes) = std::fs::read(tmp) {
            if is_webdav_banner(&bytes) {
                discard_part(tmp);
                return Err(Error::Transport(
                    "the server returned its WebDAV page instead of file content (collection?)"
                        .to_string(),
                ));
            }
        }
    }
    let _ = expected;
    Ok(())
}

/// True for the HTML page WebDAV servers return when a collection is fetched
/// as if it were a file.
pub fn is_webdav_banner(bytes: &[u8]) -> bool {
    const MARKER: &[u8] = b"This is the WebDAV interfac";
    bytes.len() < 512 && bytes.windows(MARKER.len()).any(|window| window == MARKER)
}

#[allow(clippy::too_many_arguments)]
pub fn upload(
    client: &HttpClient,
    local: &Path,
    dest_url: &str,
    provider: crate::Provider,
    upload_root: &str,
    space_root: &str,
    mtime: i64,
    threshold: u64,
    max_retries: u32,
    cancel: &CancelToken,
    sink: &dyn ProgressSink,
    path: &str,
) -> Result<()> {
    let size = std::fs::metadata(local)?.len();
    let mut reporter = ProgressReporter::new(sink, "upload", path, size);
    if size >= threshold {
        match provider {
            crate::Provider::Nextcloud => chunked_v2(
                client,
                local,
                upload_root,
                dest_url,
                mtime,
                max_retries,
                cancel,
                &mut reporter,
            ),
            crate::Provider::OpenCloud => tus(
                client,
                local,
                space_root,
                dest_url,
                mtime,
                max_retries,
                cancel,
                &mut reporter,
            ),
        }
    } else {
        let mut last = None;
        for attempt in 0..=max_retries {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let cs = if size < CHECKSUM_LIMIT {
                Some(format!("SHA1:{}", sha1_hex(local)?))
            } else {
                None
            };
            match client.put_file(dest_url, local, Some(mtime), cs.as_deref()) {
                Ok(_) => {
                    reporter.finish();
                    return Ok(());
                }
                Err(e) => {
                    let retry = retryable(&e);
                    last = Some(e);
                    if !retry || attempt == max_retries {
                        break;
                    }
                    backoff(attempt);
                }
            }
        }
        Err(last.unwrap_or_else(|| Error::Transport("upload failed".into())))
    }
}

fn retryable(e: &Error) -> bool {
    match e {
        Error::Http { status, .. } => {
            // 423 is a temporary lock, 507 a transient quota/quota-refresh signal.
            *status >= 500 || matches!(*status, 408 | 423 | 429 | 507)
        }
        Error::Transport(_) => true,
        _ => false,
    }
}

fn backoff(attempt: u32) {
    let secs = 1u64 << attempt.min(4);
    std::thread::sleep(Duration::from_secs(secs));
}

/// Remove a partial file together with its range bookkeeping (issue #272).
fn discard_part(part: &Path) {
    let _ = std::fs::remove_file(part);
    let _ = std::fs::remove_file(ranges_sidecar(part));
    let _ = std::fs::remove_file(version_sidecar(part));
}

fn temp_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

#[cfg(unix)]
fn set_mtime(path: &Path, mtime: Option<i64>) -> Result<()> {
    if let Some(m) = mtime {
        let ft = filetime::FileTime::from_unix_time(m, 0);
        filetime::set_file_mtime(path, ft).map_err(Error::Io)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mtime(_path: &Path, _mtime: Option<i64>) -> Result<()> {
    Ok(())
}

/// The `Upload-Metadata` header OpenCloud expects: comma-separated
/// `key base64value` pairs, as built by the official client
/// (`oc-desktop src/libsync/propagateuploadtus.cpp:88`).
pub fn tus_metadata(rel: &str, mtime: i64) -> String {
    format!(
        "filename {}, mtime {mtime}",
        crate::http::base64_encode(rel.as_bytes())
    )
}

pub fn sha1_hex_bytes(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn sha1_hex(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[allow(clippy::too_many_arguments)]
fn chunked_v2(
    client: &HttpClient,
    local: &Path,
    upload_root: &str,
    dest_url: &str,
    mtime: i64,
    max_retries: u32,
    cancel: &CancelToken,
    reporter: &mut ProgressReporter,
) -> Result<()> {
    // A deterministic id keeps the same server-side upload folder across
    // attempts, so a retry can skip the chunks that already landed instead of
    // starting over.
    let size = std::fs::metadata(local)?.len();
    let seed = format!("{dest_url}|{size}|{mtime}");
    let transfer = sha1_hex_bytes(seed.as_bytes())[..16].to_string();
    let base = format!("{}/{}/", upload_root.trim_end_matches('/'), transfer);
    client.mkcol(&base)?;
    // Chunks already present on the server from a previous attempt.
    let mut present: Vec<String> = client
        .propfind(&base, "1")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| e.path.rsplit('/').next().map(str::to_string))
        .collect();
    let mut f = std::fs::File::open(local)?;
    let mut idx = 1u32;
    let mut buf = vec![0u8; CHUNK as usize];
    loop {
        if cancel.is_cancelled() {
            let _ = client.delete(&base);
            return Err(Error::Cancelled);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let name = format!("{:05}", idx);
        let url = format!("{base}{name}");
        if present.contains(&name) {
            present.retain(|p| *p != name);
            reporter.add(n as u64);
            idx += 1;
            continue;
        }
        let mut last = None;
        let mut ok = false;
        for attempt in 0..=max_retries {
            match client.put_bytes(&url, &buf[..n], &[]) {
                Ok(_) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    let retry = retryable(&e);
                    last = Some(e);
                    if !retry || attempt == max_retries {
                        break;
                    }
                    backoff(attempt);
                }
            }
        }
        if !ok {
            // Keep the upload folder: the next attempt resumes from here.
            return Err(last.unwrap_or_else(|| Error::Transport("chunk failed".into())));
        }
        reporter.add(n as u64);
        idx += 1;
    }
    let src = format!("{base}.file");
    let total = std::fs::metadata(local)?.len().to_string();
    let mt = mtime.to_string();
    // A resumed upload can mix chunks from an earlier run whose content has
    // since changed (same size and mtime, different bytes). The server
    // validates this checksum on MOVE and rejects the file instead of
    // publishing corrupt content.
    let checksum = format!("SHA1:{}", sha1_hex(local)?);
    let moved = client.request_raw(
        "MOVE",
        &src,
        &[
            ("Destination", dest_url),
            ("Overwrite", "T"),
            ("OC-Total-Length", &total),
            ("X-OC-Mtime", &mt),
            ("OC-Checksum", &checksum),
        ],
        None,
    )?;
    let status = moved.status().as_u16();
    if !(200..=299).contains(&status) {
        // Without this check a failed MOVE reported a successful upload and
        // left the file only as loose chunks on the server.
        return Err(Error::Http {
            status,
            url: dest_url.to_string(),
            body: "chunked move".into(),
        });
    }
    reporter.finish();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn tus(
    client: &HttpClient,
    local: &Path,
    space_root: &str,
    dest_url: &str,
    mtime: i64,
    max_retries: u32,
    cancel: &CancelToken,
    reporter: &mut ProgressReporter,
) -> Result<()> {
    let rel = dest_url
        .strip_prefix(space_root.trim_end_matches('/'))
        .unwrap_or(dest_url)
        .trim_start_matches('/');
    let size = std::fs::metadata(local)?.len();
    let meta = tus_metadata(rel, mtime);
    let len = size.to_string();
    let mut last = None;
    for attempt in 0..=max_retries {
        let resp = client.request_raw(
            "POST",
            space_root,
            &[
                ("Tus-Resumable", "1.0.0"),
                ("Upload-Length", &len),
                ("Upload-Metadata", &meta),
                ("Content-Length", "0"),
            ],
            None,
        );
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let retry = retryable(&e);
                last = Some(e);
                if !retry || attempt == max_retries {
                    break;
                }
                backoff(attempt);
                continue;
            }
        };
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(Error::Auth(format!("tus create {status}")));
        }
        if !(200..=299).contains(&status) {
            let e = Error::Http {
                status,
                url: space_root.to_string(),
                body: "tus create".into(),
            };
            let retry = retryable(&e);
            last = Some(e);
            if !retry || attempt == max_retries {
                break;
            }
            backoff(attempt);
            continue;
        }
        let location = resp
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let Some(location) = location else {
            return Err(Error::Transport("tus: missing Location".into()));
        };
        let location = absolute(space_root, &location);
        let mut offset = head_offset(client, &location).unwrap_or(0);
        let mut f = std::fs::File::open(local)?;
        if offset > 0 {
            std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(offset))?;
        }
        let mut buf = vec![0u8; CHUNK as usize];
        let mut failed = None;
        while offset < size {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let want = std::cmp::min(CHUNK, size - offset) as usize;
            let mut read = 0;
            while read < want {
                let n = f.read(&mut buf[read..want])?;
                if n == 0 {
                    break;
                }
                read += n;
            }
            let off = offset.to_string();
            let cl = read.to_string();
            let res = client.request_raw(
                "PATCH",
                &location,
                &[
                    ("Tus-Resumable", "1.0.0"),
                    ("Upload-Offset", &off),
                    ("Content-Type", "application/offset+octet-stream"),
                    ("Content-Length", &cl),
                ],
                Some(&buf[..read]),
            );
            match res {
                Ok(r) if (200..=299).contains(&r.status().as_u16()) => {
                    offset += read as u64;
                    reporter.add(read as u64);
                }
                Ok(r) => {
                    failed = Some(Error::Http {
                        status: r.status().as_u16(),
                        url: location.clone(),
                        body: "tus patch".into(),
                    });
                    break;
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        if failed.is_none() && offset >= size {
            return Ok(());
        }
        last = failed;
        if attempt == max_retries {
            break;
        }
        backoff(attempt);
    }
    Err(last.unwrap_or_else(|| Error::Transport("tus upload failed".into())))
}

fn head_offset(client: &HttpClient, url: &str) -> Result<u64> {
    let resp = client.request_raw("HEAD", url, &[("Tus-Resumable", "1.0.0")], None)?;
    if !(200..=299).contains(&resp.status().as_u16()) {
        return Ok(0);
    }
    Ok(resp
        .headers()
        .get("Upload-Offset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

fn absolute(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    if let Some(idx) = base.find("://") {
        let rest = &base[idx + 3..];
        let host_end = rest.find('/').map(|i| idx + 3 + i).unwrap_or(base.len());
        if location.starts_with('/') {
            return format!("{}{}", &base[..host_end], location);
        }
    }
    format!("{}/{}", base.trim_end_matches('/'), location)
}

#[allow(dead_code)]
fn write_all(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_of_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha1_hex(&p).unwrap(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn temp_path_is_stable_so_a_restart_can_resume() {
        let p = Path::new("/tmp/foto.jpg");
        let t = temp_path(p);
        let name = t.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("foto.jpg"));
        assert!(name.ends_with(".part"));
        assert_eq!(t, temp_path(p));
    }

    #[test]
    fn absolute_location() {
        assert_eq!(
            absolute("https://h:9200/dav/spaces/x", "/dav/x/1"),
            "https://h:9200/dav/x/1"
        );
        assert_eq!(
            absolute("https://h:9200/dav/spaces/x", "https://o/p"),
            "https://o/p"
        );
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    use std::sync::Mutex;

    struct Recorder(Mutex<Vec<(u64, u64)>>);

    impl ProgressSink for Recorder {
        fn event(&self, p: Progress) {
            self.0.lock().unwrap().push((p.bytes_done, p.bytes_total));
        }
    }

    #[test]
    fn reporter_emits_start_add_and_finish() {
        let sink = Recorder(Mutex::new(Vec::new()));
        let mut reporter = ProgressReporter::new(&sink, "download", "Docs/big.bin", 1000);
        reporter.add(400);
        reporter.finish();
        let seen = sink.0.lock().unwrap().clone();
        assert_eq!(seen.first(), Some(&(0, 1000)));
        assert_eq!(seen.last(), Some(&(1000, 1000)));
    }

    #[test]
    fn reporter_resumes_from_bytes_already_on_disk() {
        let sink = Recorder(Mutex::new(Vec::new()));
        let mut reporter = ProgressReporter::new(&sink, "download", "Docs/big.bin", 1000);
        reporter.resume_from(600);
        reporter.add(400);
        reporter.finish();
        let seen = sink.0.lock().unwrap().clone();
        assert!(seen.contains(&(600, 1000)));
        assert_eq!(seen.last(), Some(&(1000, 1000)));
    }
}

#[cfg(test)]
mod chunk_resume_tests {
    use super::*;

    /// Resumes a chunked v2 upload that already has its first chunk on the
    /// server. Needs a live Nextcloud:
    /// `NXS_E2E_NC_URL=http://127.0.0.1:8081 cargo test -- --ignored chunked_resume`
    #[test]
    #[ignore]
    fn chunked_resume_reuses_chunks_already_uploaded() {
        let base =
            std::env::var("NXS_E2E_NC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
        let user = "admin";
        let pass = "admin1234";
        let opts = crate::http::HttpOptions {
            base_url: base.clone(),
            username: user.into(),
            password: pass.into(),
            trust_invalid_certificates: false,
            proxy: None,
            timeout: std::time::Duration::from_secs(60),
        };
        let client = crate::http::HttpClient::new(&opts).expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let name = format!("resume-{}.bin", std::process::id());
        let local = dir.path().join(&name);
        let mut data = vec![0u8; CHUNK as usize + 4096];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        std::fs::write(&local, &data).expect("write");

        let mtime = 1_700_000_000i64;
        let dest = format!("{base}/remote.php/dav/files/{user}/nxs-resume/{name}");
        let upload_root = format!("{base}/remote.php/dav/uploads/{user}");
        let seed = format!("{dest}|{}|{mtime}", data.len());
        let id = sha1_hex_bytes(seed.as_bytes())[..16].to_string();
        let folder = format!("{upload_root}/{id}/");
        client.mkcol(&folder).expect("mkcol");
        let _ = client.mkcol(&format!("{base}/remote.php/dav/files/{user}/nxs-resume/"));
        client
            .put_bytes(&format!("{folder}00001"), &data[..CHUNK as usize], &[])
            .expect("seed first chunk");

        let sink = crate::NullSink;
        let cancel = crate::CancelToken::new();
        upload(
            &client,
            &local,
            &dest,
            crate::Provider::Nextcloud,
            &upload_root,
            &base,
            mtime,
            CHUNK,
            0,
            &cancel,
            &sink,
            &name,
        )
        .expect("upload resumes");

        let mut reader = client.get_reader(&dest).expect("download");
        let mut out = Vec::new();
        std::io::copy(&mut reader, &mut out).expect("read");
        assert_eq!(out.len(), data.len(), "remote size matches");
        assert_eq!(out, data, "resumed upload is byte identical");
        let _ = client.delete(&dest);
    }
}

/// Split `size` bytes into the ranges a parallel download requests (#272).
/// Ranges are contiguous and cover the whole file; the last one takes the
/// remainder so no byte is left out.
pub fn split_ranges(size: u64, streams: u32) -> Vec<(u64, u64)> {
    if size == 0 {
        return Vec::new();
    }
    let streams = u64::from(streams.max(1)).min(size);
    let chunk = size / streams;
    let mut out = Vec::with_capacity(streams as usize);
    let mut start = 0;
    for index in 0..streams {
        let end = if index + 1 == streams {
            size - 1
        } else {
            start + chunk - 1
        };
        out.push((start, end));
        start = end + 1;
    }
    out
}

/// Ranges already written into `part`, read from its `<part>.ranges` sidecar.
/// A missing or unreadable sidecar means nothing was finished yet.
pub fn completed_ranges(part: &std::path::Path) -> Vec<(u64, u64)> {
    let Ok(text) = std::fs::read_to_string(ranges_sidecar(part)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let (start, end) = line.split_once('-')?;
            Some((start.parse().ok()?, end.parse().ok()?))
        })
        .collect()
}

/// Remember a finished range so a restart does not fetch it again (#272).
pub fn mark_range_complete(part: &std::path::Path, range: (u64, u64)) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ranges_sidecar(part))?;
    writeln!(file, "{}-{}", range.0, range.1)
}

fn ranges_sidecar(part: &std::path::Path) -> std::path::PathBuf {
    let mut name = part.as_os_str().to_owned();
    name.push(".ranges");
    std::path::PathBuf::from(name)
}

/// The etag a part file was started from, so a restart does not mix versions
/// of the same file (issue #272).
fn version_sidecar(part: &std::path::Path) -> std::path::PathBuf {
    let mut name = part.as_os_str().to_owned();
    name.push(".etag");
    std::path::PathBuf::from(name)
}

fn version_of(part: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(version_sidecar(part))
        .ok()
        .map(|value| value.trim().to_string())
}

fn remember_version(part: &std::path::Path, etag: Option<&str>) -> std::io::Result<()> {
    std::fs::write(version_sidecar(part), etag.unwrap_or_default())
}

#[cfg(test)]
mod multipart_download_tests {
    use super::{completed_ranges, mark_range_complete, split_ranges};

    #[test]
    fn ranges_cover_the_whole_file_once() {
        assert_eq!(split_ranges(10, 3), vec![(0, 2), (3, 5), (6, 9)]);
        assert_eq!(split_ranges(1, 4), vec![(0, 0)]);
        assert!(split_ranges(0, 4).is_empty());
        let covered: u64 = split_ranges(1_000_000, 7)
            .iter()
            .map(|(start, end)| end - start + 1)
            .sum();
        assert_eq!(covered, 1_000_000);
    }

    #[test]
    fn a_finished_range_survives_in_the_sidecar() {
        let dir = std::env::temp_dir().join(format!("nextsync-ranges-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("file.part");
        assert!(completed_ranges(&part).is_empty());

        mark_range_complete(&part, (0, 99)).unwrap();
        mark_range_complete(&part, (100, 199)).unwrap();

        assert_eq!(completed_ranges(&part), vec![(0, 99), (100, 199)]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
