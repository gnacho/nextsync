//! Thin synchronous WebDAV client on top of ureq.

use crate::{Error, Result};
use roxmltree::Document;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub etag: String,
    pub fileid: Option<String>,
    pub mtime: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub trust_invalid_certificates: bool,
    pub proxy: Option<String>,
    pub timeout: Duration,
}

pub struct HttpClient {
    agent: ureq::Agent,
    auth: String,
}

impl HttpClient {
    pub fn new(opts: &HttpOptions) -> Result<Self> {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
        let mut tls = ureq::tls::TlsConfig::builder();
        if opts.trust_invalid_certificates {
            tls = tls.disable_verification(true);
        }
        let mut cfg = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .timeout_global(Some(opts.timeout))
            .max_redirects(0)
            .tls_config(tls.build());
        if let Some(proxy) = &opts.proxy {
            let p = ureq::Proxy::new(proxy).map_err(|e| Error::Config(format!("proxy: {e}")))?;
            cfg = cfg.proxy(Some(p));
        }
        let agent = cfg.build().new_agent();
        let auth = format!(
            "Basic {}",
            base64_encode(format!("{}:{}", opts.username, opts.password).as_bytes())
        );
        Ok(Self { agent, auth })
    }

    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let m = ureq::http::Method::from_bytes(method.as_bytes())
            .map_err(|e| Error::Config(format!("method {method}: {e}")))?;
        let mut b = ureq::http::Request::builder()
            .method(m)
            .uri(url)
            .header("Authorization", &self.auth);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = b
            .body(body.unwrap_or(&[]))
            .map_err(|e| Error::Config(format!("request: {e}")))?;
        self.agent.run(req).map_err(Error::from)
    }

    fn check(&self, resp: &ureq::http::Response<ureq::Body>, url: &str) -> Result<u16> {
        let status = resp.status().as_u16();
        match status {
            200..=299 => Ok(status),
            401 | 403 => Err(Error::Auth(format!("{status} {url}"))),
            404 => Err(Error::NotFound(url.to_string())),
            _ => Err(Error::Http {
                status,
                url: url.to_string(),
                body: String::new(),
            }),
        }
    }

    pub fn propfind(&self, url: &str, depth: &str) -> Result<Vec<RemoteEntry>> {
        const BODY: &str = r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><d:getetag/><d:getcontentlength/><d:getlastmodified/><d:resourcetype/><oc:fileid/></d:prop></d:propfind>"#;
        let mut resp = self.request(
            "PROPFIND",
            url,
            &[
                ("Depth", depth),
                ("Content-Type", "application/xml; charset=utf-8"),
            ],
            Some(BODY.as_bytes()),
        )?;
        self.check(&resp, url)?;
        let body = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| Error::Transport(e.to_string()))?;
        let text = String::from_utf8_lossy(&body).to_string();
        parse_multistatus(&text, if depth == "0" { "" } else { url })
    }

    pub fn get_reader(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        let resp = self.request("GET", url, &[], None)?;
        self.check(&resp, url)?;
        Ok(Box::new(resp.into_body().into_reader()))
    }

    pub fn put_file(
        &self,
        url: &str,
        path: &Path,
        mtime: Option<i64>,
        checksum: Option<&str>,
    ) -> Result<u16> {
        let file = std::fs::File::open(path)?;
        let mtime_s = mtime.map(|m| m.to_string()).unwrap_or_default();
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if !mtime_s.is_empty() {
            headers.push(("X-OC-Mtime", &mtime_s));
        }
        if let Some(cs) = checksum {
            headers.push(("OC-Checksum", cs));
        }
        let m = ureq::http::Method::from_bytes(b"PUT").unwrap();
        let mut b = ureq::http::Request::builder()
            .method(m)
            .uri(url)
            .header("Authorization", &self.auth);
        for (k, v) in headers {
            b = b.header(k, v);
        }
        let req = b
            .body(&file)
            .map_err(|e| Error::Config(format!("body: {e}")))?;
        let resp = self.agent.run(req).map_err(Error::from)?;
        self.check(&resp, url)
    }

    pub fn put_bytes(&self, url: &str, bytes: &[u8], extra: &[(&str, &str)]) -> Result<u16> {
        let resp = self.request("PUT", url, extra, Some(bytes))?;
        self.check(&resp, url)
    }

    pub fn mkcol(&self, url: &str) -> Result<u16> {
        let resp = self.request("MKCOL", url, &[], None)?;
        match self.check(&resp, url) {
            Ok(s) => Ok(s),
            Err(Error::Http { status: 405, .. }) => Ok(405),
            Err(e) => Err(e),
        }
    }

    pub fn delete(&self, url: &str) -> Result<()> {
        let resp = self.request("DELETE", url, &[], None)?;
        match self.check(&resp, url) {
            Ok(_) => Ok(()),
            Err(Error::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn move_to(&self, from: &str, to: &str, overwrite: bool) -> Result<u16> {
        let ov = if overwrite { "T" } else { "F" };
        let resp = self.request(
            "MOVE",
            from,
            &[("Destination", to), ("Overwrite", ov)],
            None,
        )?;
        self.check(&resp, from)
    }

    pub fn request_raw(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        self.request(method, url, headers, body)
    }
}

pub fn parse_multistatus(xml: &str, request_url: &str) -> Result<Vec<RemoteEntry>> {
    let doc = Document::parse(xml).map_err(|e| Error::Xml(e.to_string()))?;
    let prefix = url_prefix(request_url);
    let mut out = Vec::new();
    for resp in doc
        .descendants()
        .filter(|n| n.has_tag_name(("DAV:", "response")))
    {
        let href = resp
            .descendants()
            .find(|n| n.has_tag_name(("DAV:", "href")))
            .and_then(|n| n.text())
            .unwrap_or_default();
        let mut path = percent_decode(href.trim());
        if let Some(rest) = path.strip_prefix(&prefix) {
            path = rest.to_string();
        }
        path = path.trim_start_matches('/').to_string();
        if path.ends_with('/') && !path.is_empty() {
            path.pop();
        }
        if path.is_empty() {
            continue;
        }
        let mut is_dir = false;
        let mut size = 0u64;
        let mut etag = String::new();
        let mut fileid = None;
        let mut mtime = None;
        for propstat in resp
            .descendants()
            .filter(|n| n.has_tag_name(("DAV:", "propstat")))
        {
            let ok = propstat
                .descendants()
                .find(|n| n.has_tag_name(("DAV:", "status")))
                .and_then(|n| n.text())
                .map(|t| t.contains("200"))
                .unwrap_or(true);
            if !ok {
                continue;
            }
            for prop in propstat
                .descendants()
                .find(|n| n.has_tag_name(("DAV:", "prop")))
                .into_iter()
                .flat_map(|p| p.children().filter(|c| c.is_element()))
            {
                match prop.tag_name().name() {
                    "getetag" => {
                        etag = prop
                            .text()
                            .unwrap_or_default()
                            .trim()
                            .trim_matches('"')
                            .to_string()
                    }
                    "getcontentlength" => {
                        size = prop.text().unwrap_or_default().trim().parse().unwrap_or(0)
                    }
                    "getlastmodified" => mtime = prop.text().and_then(parse_http_date),
                    "resourcetype" => {
                        is_dir = prop
                            .children()
                            .any(|c| c.has_tag_name(("DAV:", "collection")))
                    }
                    "fileid" => fileid = prop.text().map(|t| t.trim().to_string()),
                    _ => {}
                }
            }
        }
        out.push(RemoteEntry {
            path,
            is_dir,
            size,
            etag,
            fileid,
            mtime,
        });
    }
    Ok(out)
}

fn url_prefix(url: &str) -> String {
    if let Some(idx) = url.find("://") {
        let rest = &url[idx + 3..];
        let slash = rest.find('/').map(|i| idx + 3 + i).unwrap_or(url.len());
        percent_decode(&url[slash..])
    } else {
        percent_decode(url)
    }
}

pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn percent_encode_path(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn base64_encode(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn parse_http_date(value: &str) -> Option<i64> {
    let v = value.trim();
    let month = |m: &str| {
        [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ]
        .iter()
        .position(|x| x.eq_ignore_ascii_case(m))
        .map(|i| i as i64 + 1)
    };
    let parts: Vec<&str> = v.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let mon = month(parts[2])?;
    let year: i64 = parts[3].parse().ok()?;
    let hms: Vec<&str> = parts[4].split(':').collect();
    let h: i64 = hms.first()?.parse().ok()?;
    let m: i64 = hms.get(1)?.parse().ok()?;
    let s: i64 = hms.get(2)?.parse().ok()?;
    Some(days_from_civil(year, mon, day) * 86400 + h * 3600 + m * 60 + s)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns">
<d:response><d:href>/remote.php/dav/files/admin/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getetag>&quot;abc&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/remote.php/dav/files/admin/Docs/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getetag>&quot;def&quot;</d:getetag><oc:fileid>7</oc:fileid></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/remote.php/dav/files/admin/Docs/a%20b.txt</d:href><d:propstat><d:prop><d:resourcetype/><d:getetag>&quot;ghi&quot;</d:getetag><d:getcontentlength>12</d:getcontentlength><d:getlastmodified>Wed, 04 Nov 2026 22:47:17 GMT</d:getlastmodified><oc:fileid>9</oc:fileid></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
<d:propstat><d:prop><oc:checksums/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response>
</d:multistatus>"#;

    #[test]
    fn parses_nextcloud_multistatus() {
        let url = "http://127.0.0.1:8081/remote.php/dav/files/admin/";
        let entries = parse_multistatus(FIXTURE, url).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "Docs");
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].fileid.as_deref(), Some("7"));
        let file = &entries[1];
        assert_eq!(file.path, "Docs/a b.txt");
        assert!(!file.is_dir);
        assert_eq!(file.size, 12);
        assert_eq!(file.etag, "ghi");
        let mtime = file.mtime.expect("mtime parsed");
        assert!(
            mtime > 1_700_000_000 && mtime < 1_900_000_000,
            "mtime {mtime}"
        );
    }

    #[test]
    fn percent_roundtrip() {
        let enc = percent_encode_path("/Docs/a b/ñ.txt");
        assert_eq!(enc, "/Docs/a%20b/%C3%B1.txt");
        assert_eq!(percent_decode(&enc), "/Docs/a b/ñ.txt");
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64_encode(b"admin:pass"), "YWRtaW46cGFzcw==");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
    }
}
