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
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_path(dest);
    let mut reporter = ProgressReporter::new(sink, "download", path, total);
    let mut last_err = None;
    for attempt in 0..=max_retries {
        if cancel.is_cancelled() {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Cancelled);
        }
        match try_download(client, url, &tmp, expected_etag, &mut reporter) {
            Ok(()) => {
                if let Some(expected) = expected_checksum {
                    let actual = sha1_hex(&tmp)?;
                    if !actual.eq_ignore_ascii_case(expected) {
                        let _ = std::fs::remove_file(&tmp);
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
                return Ok(());
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
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
                let _ = std::fs::remove_file(tmp);
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

fn temp_path(dest: &Path) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{}.part", std::process::id(), n));
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
    fn temp_path_keeps_extension_and_is_unique() {
        let p = Path::new("/tmp/foto.jpg");
        let t = temp_path(p);
        let name = t.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("foto.jpg."));
        assert!(name.ends_with(".part"));
        assert_ne!(t, temp_path(p));
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
