//! Streaming downloads and uploads, with retries and chunking.

use crate::http::HttpClient;
use crate::{CancelToken, Error, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use sha1::{Digest, Sha1};

const CHUNK: u64 = 10 * 1024 * 1024;
const CHECKSUM_LIMIT: u64 = 100 * 1024 * 1024;

pub fn download(
    client: &HttpClient,
    url: &str,
    dest: &Path,
    expected_etag: Option<&str>,
    mtime: Option<i64>,
    max_retries: u32,
    cancel: &CancelToken,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_path(dest);
    let mut last_err = None;
    for attempt in 0..=max_retries {
        if cancel.is_cancelled() {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Cancelled);
        }
        match try_download(client, url, &tmp, expected_etag) {
            Ok(()) => {
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

fn try_download(client: &HttpClient, url: &str, tmp: &Path, expected: Option<&str>) -> Result<()> {
    let mut reader = client.get_reader(url)?;
    let mut out = std::fs::File::create(tmp)?;
    std::io::copy(&mut reader, &mut out)?;
    out.sync_all()?;
    let _ = expected;
    Ok(())
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
) -> Result<()> {
    let size = std::fs::metadata(local)?.len();
    if size >= threshold {
        match provider {
            crate::Provider::Nextcloud => chunked_v2(
                client, local, upload_root, dest_url, mtime, max_retries, cancel,
            ),
            crate::Provider::OpenCloud => {
                tus(client, local, space_root, dest_url, mtime, max_retries, cancel)
            }
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
                Ok(_) => return Ok(()),
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
        Error::Http { status, .. } => *status >= 500 || *status == 429 || *status == 408,
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

pub fn sha1_hex(path: &Path) -> Result<String> {
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

fn chunked_v2(
    client: &HttpClient,
    local: &Path,
    upload_root: &str,
    dest_url: &str,
    mtime: i64,
    max_retries: u32,
    cancel: &CancelToken,
) -> Result<()> {
    let transfer = format!("{}-{}", crate::http::now_epoch(), std::process::id());
    let base = format!("{}/{}/", upload_root.trim_end_matches('/'), transfer);
    client.mkcol(&base)?;
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
        let url = format!("{base}{:05}", idx);
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
            let _ = client.delete(&base);
            return Err(last.unwrap_or_else(|| Error::Transport("chunk failed".into())));
        }
        idx += 1;
    }
    let src = format!("{base}.file");
    let total = std::fs::metadata(local)?.len().to_string();
    let mt = mtime.to_string();
    client
        .request_raw(
            "MOVE",
            &src,
            &[
                ("Destination", dest_url),
                ("Overwrite", "T"),
                ("OC-Total-Length", &total),
                ("X-OC-Mtime", &mt),
            ],
            None,
        )
        .map(drop)
}

fn tus(
    client: &HttpClient,
    local: &Path,
    space_root: &str,
    dest_url: &str,
    mtime: i64,
    max_retries: u32,
    cancel: &CancelToken,
) -> Result<()> {
    let rel = dest_url
        .strip_prefix(space_root.trim_end_matches('/'))
        .unwrap_or(dest_url)
        .trim_start_matches('/');
    let size = std::fs::metadata(local)?.len();
    let filename = crate::http::base64_encode(rel.as_bytes());
    let meta = format!("filename {filename}, mtime {mtime}");
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
        assert_eq!(sha1_hex(&p).unwrap(), "a9993e364706816aba3e25717850c26c9cd0d89d");
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
