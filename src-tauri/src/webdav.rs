use std::path::Path;
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use quick_xml::Reader;
use quick_xml::events::Event;
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode, Url};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use unicode_normalization::UnicodeNormalization;

pub const CHUNK_SIZE: u64 = 10 * 1024 * 1024;

const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

const PROPFIND_BODY: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:prop>
    <d:getetag/><d:getlastmodified/><d:getcontentlength/><d:resourcetype/><oc:fileid/>
  </d:prop>
</d:propfind>"#;

#[derive(Debug, thiserror::Error)]
pub enum DavError {
    #[error("login required")]
    Unauthorized,
    #[error("precondition failed (changed on server)")]
    PreconditionFailed,
    #[error("server storage is full")]
    InsufficientStorage,
    #[error("network error: {0}")]
    Network(String),
    #[error("server returned HTTP {0}")]
    Other(u16),
    #[error("bad server response: {0}")]
    Parse(String),
    #[error("local file error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<reqwest::Error> for DavError {
    fn from(e: reqwest::Error) -> Self {
        DavError::Network(e.to_string())
    }
}

pub type DavResult<T> = Result<T, DavError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub path: String,
    pub is_dir: bool,
    pub etag: String,
    pub mtime: i64,
    pub size: i64,
    pub fileid: Option<String>,
}

/// Upload precondition.
#[derive(Debug, Clone)]
pub enum Cond {
    IfMatch(String),
    IfNoneMatch,
}

#[derive(Debug, Clone, Default)]
pub struct PutResult {
    pub etag: Option<String>,
    pub fileid: Option<String>,
}

pub fn user_agent() -> String {
    format!(
        "Mozilla/5.0 (Macintosh) NCSync/{}",
        env!("CARGO_PKG_VERSION")
    )
}

pub fn http_client() -> Client {
    Client::builder()
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        .build()
        .expect("http client")
}

pub fn encode_path(path: &str) -> String {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| utf8_percent_encode(s, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn strip_etag(s: &str) -> String {
    s.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .to_string()
}

fn header(resp: &Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Turns non-2xx responses into errors and logs the URL and the start of the body.
async fn check(resp: Response) -> DavResult<Response> {
    let s = resp.status();
    if s.is_success() {
        return Ok(resp);
    }
    let url = resp.url().to_string();
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body
        .chars()
        .take(400)
        .collect::<String>()
        .replace('\n', " ");
    tracing::warn!("HTTP {} for {url}: {snippet}", s.as_u16());
    Err(match s {
        StatusCode::UNAUTHORIZED => DavError::Unauthorized,
        StatusCode::PRECONDITION_FAILED => DavError::PreconditionFailed,
        StatusCode::INSUFFICIENT_STORAGE => DavError::InsufficientStorage,
        s => DavError::Other(s.as_u16()),
    })
}

fn method(m: &str) -> Method {
    Method::from_bytes(m.as_bytes()).expect("method")
}

pub struct Dav {
    client: Client,
    server: String,
    user_enc: String,
    login: String,
    password: String,
    /// `{server}/remote.php/dav/files/{user}/`
    files_base: String,
    /// Decoded URL path of `files_base`, used to turn hrefs into relative paths.
    base_path: String,
}

impl Dav {
    pub fn new(client: Client, server: &str, user_id: &str, login: &str, password: &str) -> Dav {
        let user_enc = utf8_percent_encode(user_id, SEGMENT).to_string();
        let files_base = format!("{server}/remote.php/dav/files/{user_enc}/");
        let base_path = Url::parse(&files_base)
            .map(|u| {
                percent_decode_str(u.path())
                    .decode_utf8_lossy()
                    .into_owned()
            })
            .unwrap_or_default();
        Dav {
            client,
            server: server.to_string(),
            user_enc,
            login: login.to_string(),
            password: password.to_string(),
            files_base,
            base_path,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.files_base, encode_path(path))
    }

    fn req(&self, m: Method, url: &str) -> RequestBuilder {
        self.client
            .request(m, url)
            .basic_auth(&self.login, Some(&self.password))
    }

    pub async fn propfind(&self, path: &str, depth: u8) -> DavResult<Vec<RemoteEntry>> {
        let mut url = self.url(path);
        if !url.ends_with('/') {
            url.push('/');
        }
        // Retry gateway errors (flaky proxy) 3 times, 3 s apart.
        let mut retries = 3;
        let resp = loop {
            let resp = self
                .req(method("PROPFIND"), &url)
                .header("Depth", depth.to_string())
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(PROPFIND_BODY)
                .send()
                .await?;
            match check(resp).await {
                Err(DavError::Other(502..=504)) if retries > 0 => {
                    retries -= 1;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                r => break r?,
            }
        };
        let body = resp.text().await?;
        parse_multistatus(&body, &self.base_path)
    }

    /// Etag of the account root (Depth 0).
    pub async fn root_etag(&self) -> DavResult<String> {
        self.propfind("", 0)
            .await?
            .into_iter()
            .next()
            .map(|e| e.etag)
            .ok_or_else(|| DavError::Parse("empty PROPFIND".into()))
    }

    /// Streams `path` to `dest`. Returns (etag, last-modified secs).
    pub async fn get(&self, path: &str, dest: &Path) -> DavResult<(Option<String>, Option<i64>)> {
        let resp = check(self.req(Method::GET, &self.url(path)).send().await?).await?;
        let etag = header(&resp, "ETag")
            .or_else(|| header(&resp, "OC-ETag"))
            .map(|s| strip_etag(&s));
        let mtime = header(&resp, "Last-Modified").and_then(|s| parse_http_date(&s));
        let mut resp = resp;
        let mut f =
            tokio::io::BufWriter::with_capacity(1 << 20, tokio::fs::File::create(dest).await?);
        while let Some(chunk) = resp.chunk().await? {
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        f.get_ref().sync_all().await?;
        Ok((etag, mtime))
    }

    fn put_result(resp: &Response) -> PutResult {
        PutResult {
            etag: header(resp, "OC-ETag")
                .or_else(|| header(resp, "ETag"))
                .map(|s| strip_etag(&s)),
            fileid: header(resp, "OC-FileId"),
        }
    }

    fn cond(rb: RequestBuilder, cond: &Cond) -> RequestBuilder {
        match cond {
            Cond::IfMatch(etag) => rb.header("If-Match", format!("\"{etag}\"")),
            Cond::IfNoneMatch => rb.header("If-None-Match", "*"),
        }
    }

    pub async fn put(
        &self,
        path: &str,
        file: &Path,
        mtime: i64,
        cond: &Cond,
    ) -> DavResult<PutResult> {
        let data = tokio::fs::read(file).await?;
        let rb = self
            .req(Method::PUT, &self.url(path))
            .header("X-OC-Mtime", mtime.to_string())
            .body(data);
        let resp = check(Self::cond(rb, cond).send().await?).await?;
        Ok(Self::put_result(&resp))
    }

    pub async fn mkcol(&self, path: &str) -> DavResult<Option<String>> {
        let resp = self.req(method("MKCOL"), &self.url(path)).send().await?;
        if resp.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Ok(None); // already exists
        }
        let resp = check(resp).await?;
        Ok(header(&resp, "OC-FileId"))
    }

    /// Deletes a file or folder. 404 counts as success.
    #[cfg(test)]
    pub async fn delete(&self, path: &str) -> DavResult<()> {
        self.delete_if(path, None).await
    }

    /// Deletes only if the etag still matches (when given).
    pub async fn delete_if(&self, path: &str, etag: Option<&str>) -> DavResult<()> {
        let mut rb = self.req(Method::DELETE, &self.url(path));
        if let Some(etag) = etag {
            rb = rb.header("If-Match", format!("\"{etag}\""));
        }
        let resp = rb.send().await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        check(resp).await.map(|_| ())
    }

    /// Chunked upload v2.
    pub async fn chunked_upload(
        &self,
        path: &str,
        file: &Path,
        mtime: i64,
        cond: &Cond,
    ) -> DavResult<PutResult> {
        let dir = format!(
            "{}/remote.php/dav/uploads/{}/ncsync-{}",
            self.server,
            self.user_enc,
            uuid::Uuid::new_v4()
        );
        tracing::info!("chunked upload {path} via {dir}");
        let res = self.chunked_inner(&dir, path, file, mtime, cond).await;
        if res.is_err() {
            let _ = self.req(Method::DELETE, &dir).send().await;
        }
        res
    }

    async fn chunked_inner(
        &self,
        dir: &str,
        path: &str,
        file: &Path,
        mtime: i64,
        cond: &Cond,
    ) -> DavResult<PutResult> {
        let dest = self.url(path);
        check(
            self.req(method("MKCOL"), dir)
                .header("Destination", &dest)
                .send()
                .await?,
        )
        .await?;
        let mut f = tokio::fs::File::open(file).await?;
        let file_len = f.metadata().await?.len().to_string();
        let mut total: u64 = 0;
        let mut n = 0u32;
        loop {
            let mut buf = Vec::with_capacity(CHUNK_SIZE as usize);
            (&mut f).take(CHUNK_SIZE).read_to_end(&mut buf).await?;
            if buf.is_empty() && n > 0 {
                break;
            }
            n += 1;
            total += buf.len() as u64;
            let last = (buf.len() as u64) < CHUNK_SIZE;
            check(
                self.req(Method::PUT, &format!("{dir}/{n:05}"))
                    .header("Destination", &dest)
                    .header("OC-Total-Length", &file_len)
                    .body(buf)
                    .send()
                    .await?,
            )
            .await?;
            if last {
                break;
            }
        }
        let mut rb = self
            .req(method("MOVE"), &format!("{dir}/.file"))
            .header("Destination", &dest)
            .header("X-OC-Mtime", mtime.to_string())
            .header("OC-Total-Length", total.to_string());
        // If-Match / If-None-Match on MOVE would apply to the source `.file`, so the
        // destination is checked with a tagged `If` header or a PROPFIND instead.
        match cond {
            Cond::IfMatch(etag) => rb = rb.header("If", format!("<{dest}> ([\"{etag}\"])")),
            Cond::IfNoneMatch => match self.propfind(path, 0).await {
                Ok(_) => return Err(DavError::PreconditionFailed),
                Err(DavError::Other(404)) => {}
                Err(e) => return Err(e),
            },
        }
        let resp = check(rb.send().await?).await?;
        Ok(Self::put_result(&resp))
    }
}

pub fn parse_http_date(s: &str) -> Option<i64> {
    let t = httpdate::parse_http_date(s.trim()).ok()?;
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// Turns an href into a relative NFC path below `base_path`. Returns None if outside it.
fn href_to_path(href: &str, base_path: &str) -> Option<String> {
    let raw_path = if href.starts_with("http://") || href.starts_with("https://") {
        Url::parse(href).ok()?.path().to_string()
    } else {
        href.to_string()
    };
    let decoded = percent_decode_str(&raw_path).decode_utf8().ok()?;
    let rel = decoded.strip_prefix(base_path).or_else(|| {
        // Root href may lack the trailing slash.
        (decoded.as_ref() == base_path.trim_end_matches('/')).then_some("")
    })?;
    Some(rel.trim_matches('/').nfc().collect())
}

#[derive(Default)]
struct Builder {
    href: Option<String>,
    etag: Option<String>,
    lastmod: Option<String>,
    len: Option<String>,
    fileid: Option<String>,
    is_dir: bool,
}

/// Parses a WebDAV multistatus body.
pub fn parse_multistatus(xml: &str, base_path: &str) -> DavResult<Vec<RemoteEntry>> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur: Option<Builder> = None;
    let mut field: Option<&'static str> = None;
    let mut buf = String::new();
    let field_of = |name: &str| -> Option<&'static str> {
        match name {
            "href" => Some("href"),
            "getetag" => Some("etag"),
            "getlastmodified" => Some("lastmod"),
            "getcontentlength" => Some("len"),
            "fileid" => Some("fileid"),
            _ => None,
        }
    };
    loop {
        let ev = reader
            .read_event()
            .map_err(|e| DavError::Parse(e.to_string()))?;
        match ev {
            Event::Start(e) => {
                let name = e.local_name();
                match name.as_ref() {
                    "response" => cur = Some(Builder::default()),
                    "collection" => {
                        if let Some(b) = cur.as_mut() {
                            b.is_dir = true
                        }
                    }
                    n => {
                        if let Some(f) = field_of(n) {
                            field = Some(f);
                            buf.clear();
                        }
                    }
                }
            }
            Event::Empty(e) => {
                if e.local_name().as_ref() == "collection"
                    && let Some(b) = cur.as_mut()
                {
                    b.is_dir = true;
                }
            }
            Event::Text(t) => {
                if field.is_some() {
                    buf.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) => {
                if field.is_some() {
                    buf.push_str(&t.into_inner());
                }
            }
            Event::GeneralRef(r) => {
                if field.is_some() {
                    if let Ok(Some(c)) = r.resolve_char_ref() {
                        buf.push(c);
                    } else {
                        let s = match r.into_inner().as_ref() {
                            "amp" => "&",
                            "lt" => "<",
                            "gt" => ">",
                            "quot" => "\"",
                            "apos" => "'",
                            _ => "",
                        };
                        buf.push_str(s);
                    }
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                let name = name.as_ref();
                if name == "response" {
                    if let Some(entry) = cur.take().and_then(|b| finish(b, base_path)) {
                        out.push(entry);
                    }
                } else if let (Some(f), Some(b)) = (field, cur.as_mut())
                    && field_of(name) == Some(f)
                {
                    let v = buf.trim().to_string();
                    if !v.is_empty() {
                        match f {
                            "href" => b.href = Some(v),
                            "etag" => b.etag = Some(v),
                            "lastmod" => b.lastmod = Some(v),
                            "len" => b.len = Some(v),
                            _ => b.fileid = Some(v),
                        }
                    }
                    field = None;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn finish(b: Builder, base_path: &str) -> Option<RemoteEntry> {
    let href = b.href?;
    let Some(path) = href_to_path(&href, base_path) else {
        tracing::warn!("href outside files root: {href}");
        return None;
    };
    Some(RemoteEntry {
        path,
        is_dir: b.is_dir,
        etag: strip_etag(&b.etag.unwrap_or_default()),
        mtime: b.lastmod.as_deref().and_then(parse_http_date).unwrap_or(0),
        size: if b.is_dir {
            0
        } else {
            b.len.and_then(|s| s.parse().ok()).unwrap_or(0)
        },
        fileid: b.fileid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns">
 <d:response>
  <d:href>/nc/remote.php/dav/files/al%20ice/</d:href>
  <d:propstat><d:prop>
    <d:getetag>&quot;root1&quot;</d:getetag>
    <d:getlastmodified>Sun, 06 Nov 1994 08:49:37 GMT</d:getlastmodified>
    <d:resourcetype><d:collection/></d:resourcetype>
    <oc:fileid>1</oc:fileid>
  </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getcontentlength/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/nc/remote.php/dav/files/al%20ice/Docs/Cafe%CC%81%20%26%20more.txt</d:href>
  <d:propstat><d:prop>
    <d:getetag>"abc"</d:getetag>
    <d:getlastmodified>Sun, 06 Nov 1994 08:49:37 GMT</d:getlastmodified>
    <d:getcontentlength>42</d:getcontentlength>
    <d:resourcetype/>
    <oc:fileid>77</oc:fileid>
  </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/nc/remote.php/dav/files/al%20ice/Sub/</d:href>
  <d:propstat><d:prop>
    <d:getetag>"s1"</d:getetag>
    <d:resourcetype><d:collection/></d:resourcetype>
  </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
</d:multistatus>"#;

    #[test]
    fn parses_multistatus() {
        let dav = Dav::new(http_client(), "https://h/nc", "al ice", "l", "p");
        assert_eq!(dav.base_path, "/nc/remote.php/dav/files/al ice/");
        let v = parse_multistatus(XML, &dav.base_path).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].path, "");
        assert!(v[0].is_dir);
        assert_eq!(v[0].etag, "root1");
        assert_eq!(v[1].path, "Docs/Caf\u{e9} & more.txt"); // NFC
        assert!(!v[1].is_dir);
        assert_eq!(v[1].size, 42);
        assert_eq!(v[1].mtime, 784111777);
        assert_eq!(v[1].etag, "abc");
        assert_eq!(v[1].fileid.as_deref(), Some("77"));
        assert_eq!(v[2].path, "Sub");
        assert!(v[2].is_dir);
    }

    #[test]
    fn encodes_segments() {
        assert_eq!(encode_path("a b/c#d?.txt"), "a%20b/c%23d%3F.txt");
        let dav = Dav::new(http_client(), "https://h", "u", "l", "p");
        assert_eq!(dav.url("x y"), "https://h/remote.php/dav/files/u/x%20y");
    }
}
