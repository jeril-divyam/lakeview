//! A thin async client for the lakeFS OpenAPI (`/api/v1`).
//!
//! Only the read paths the browser needs are implemented. Every list call
//! follows pagination until the server stops or `max_entries` is reached.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::config::Profile;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    key: String,
    secret: String,
    page_size: u32,
}

#[derive(Debug, Deserialize)]
struct Pagination {
    has_more: bool,
    #[serde(default)]
    next_offset: String,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    pagination: Pagination,
    results: Vec<T>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    pub id: String,
    #[serde(default)]
    pub creation_date: i64,
    #[serde(default)]
    pub default_branch: String,
    #[serde(default)]
    pub storage_namespace: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefEntry {
    pub id: String,
    #[serde(default)]
    pub commit_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Tag,
}

#[derive(Debug, Clone)]
pub struct NamedRef {
    pub id: String,
    pub commit_id: String,
    pub kind: RefKind,
    /// True when this is the repository's default branch.
    pub is_default: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObjectStats {
    pub path: String,
    pub path_type: String,
    #[serde(default)]
    pub physical_address: String,
    #[serde(default)]
    pub checksum: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub mtime: i64,
    #[serde(default)]
    pub content_type: Option<String>,
}

impl ObjectStats {
    pub fn is_dir(&self) -> bool {
        self.path_type == "common_prefix"
    }

    /// Final path segment, with the trailing slash kept for directories.
    pub fn name(&self) -> &str {
        let trimmed = self.path.trim_end_matches('/');
        let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
        if base.is_empty() { &self.path } else { base }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Commit {
    pub id: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub committer: String,
    #[serde(default)]
    pub creation_date: i64,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub metadata: Option<std::collections::HashMap<String, String>>,
}

impl Commit {
    pub fn short_id(&self) -> &str {
        let n = self.id.len().min(8);
        &self.id[..n]
    }

    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or("").trim()
    }
}

/// Shape of a lakeFS error body: `{"message": "..."}`.
#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

impl Client {
    pub fn new(profile: &Profile, page_size: u32) -> Result<Self> {
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(!profile.verify_tls)
            .timeout(std::time::Duration::from_secs(profile.timeout_secs))
            .user_agent(concat!("lakeview/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the HTTP client")?;

        // Accept both `https://host` and `https://host/api/v1`.
        let trimmed = profile.endpoint.trim_end_matches('/');
        let base = match trimmed.strip_suffix("/api/v1") {
            Some(root) => format!("{root}/api/v1"),
            None => format!("{trimmed}/api/v1"),
        };

        Ok(Self {
            http,
            base,
            key: profile.access_key_id.clone(),
            secret: profile.secret_access_key.clone(),
            page_size: page_size.clamp(1, 1000),
        })
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{}", self.base, path))
            .basic_auth(&self.key, Some(&self.secret))
    }

    /// Turn a non-2xx response into an error carrying the server's message.
    async fn check(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<ApiError>(&body)
            .map(|e| e.message)
            .unwrap_or_else(|_| body.chars().take(200).collect());

        let hint = match status.as_u16() {
            401 => " — check access_key_id / secret_access_key",
            403 => " — the credentials lack permission for this resource",
            404 => " — not found",
            _ => "",
        };
        if detail.trim().is_empty() {
            bail!("{what}: HTTP {status}{hint}");
        }
        bail!("{what}: HTTP {status}{hint}: {detail}");
    }

    /// Follow `next_offset` until exhausted or `max_entries` collected.
    async fn paged<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        what: &str,
        max_entries: usize,
    ) -> Result<Vec<T>> {
        let mut out: Vec<T> = Vec::new();
        let mut after = String::new();
        loop {
            let mut req = self
                .get(path)
                .query(&[("amount", self.page_size.to_string())]);
            if !after.is_empty() {
                req = req.query(&[("after", after.as_str())]);
            }
            if !query.is_empty() {
                req = req.query(query);
            }

            let resp = req.send().await.with_context(|| what.to_string())?;
            let resp = Self::check(resp, what).await?;
            let page: Page<T> = resp
                .json()
                .await
                .with_context(|| format!("{what}: unexpected response body"))?;

            out.extend(page.results);
            if !page.pagination.has_more
                || page.pagination.next_offset.is_empty()
                || out.len() >= max_entries
            {
                break;
            }
            after = page.pagination.next_offset;
        }
        Ok(out)
    }

    /// A single capped page, for probes that only need to know whether an entry
    /// or two exist rather than the whole listing.
    async fn first_page<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        amount: u32,
        what: &str,
    ) -> Result<Vec<T>> {
        let resp = self
            .get(path)
            .query(&[("amount", amount.to_string())])
            .send()
            .await
            .with_context(|| what.to_string())?;
        let resp = Self::check(resp, what).await?;
        let page: Page<T> = resp
            .json()
            .await
            .with_context(|| format!("{what}: unexpected response body"))?;
        Ok(page.results)
    }

    /// Whether expanding this repository would list any ref at all. Mirrors the
    /// browser's rule — a lone default branch is not worth a row of its own —
    /// but answers it with two capped listings instead of every ref, so the
    /// whole repository pane can be probed on start-up.
    pub async fn has_listable_refs(
        &self,
        repo: &str,
        default_branch: &str,
        include_tags: bool,
    ) -> Result<bool> {
        let branches: Vec<RefEntry> = self
            .first_page(
                &format!("/repositories/{}/branches", enc(repo)),
                2,
                "listing branches",
            )
            .await?;
        if branches_are_listable(&branches, default_branch) {
            return Ok(true);
        }
        if include_tags {
            let tags: Vec<RefEntry> = self
                .first_page(
                    &format!("/repositories/{}/tags", enc(repo)),
                    1,
                    "listing tags",
                )
                .await?;
            return Ok(!tags.is_empty());
        }
        Ok(false)
    }

    pub async fn repositories(&self) -> Result<Vec<Repository>> {
        self.paged("/repositories", &[], "listing repositories", 10_000)
            .await
    }

    /// Branches first (default branch pinned to the top), then tags.
    pub async fn refs(&self, repo: &str, include_tags: bool) -> Result<Vec<NamedRef>> {
        let default_branch = self
            .repository(repo)
            .await
            .map(|r| r.default_branch)
            .unwrap_or_default();

        let branches: Vec<RefEntry> = self
            .paged(
                &format!("/repositories/{}/branches", enc(repo)),
                &[],
                "listing branches",
                10_000,
            )
            .await?;

        let mut out: Vec<NamedRef> = branches
            .into_iter()
            .map(|b| NamedRef {
                is_default: b.id == default_branch,
                id: b.id,
                commit_id: b.commit_id,
                kind: RefKind::Branch,
            })
            .collect();
        out.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.id.cmp(&b.id)));

        if include_tags {
            // A repo without tags is normal; don't fail the whole listing.
            if let Ok(tags) = self
                .paged::<RefEntry>(
                    &format!("/repositories/{}/tags", enc(repo)),
                    &[],
                    "listing tags",
                    10_000,
                )
                .await
            {
                out.extend(tags.into_iter().map(|t| NamedRef {
                    id: t.id,
                    commit_id: t.commit_id,
                    kind: RefKind::Tag,
                    is_default: false,
                }));
            }
        }
        Ok(out)
    }

    pub async fn repository(&self, repo: &str) -> Result<Repository> {
        let resp = self
            .get(&format!("/repositories/{}", enc(repo)))
            .send()
            .await
            .context("fetching repository")?;
        let resp = Self::check(resp, "fetching repository").await?;
        resp.json().await.context("parsing repository")
    }

    /// One directory level: `delimiter=/` collapses deeper keys into prefixes.
    pub async fn list_objects(
        &self,
        repo: &str,
        reference: &str,
        prefix: &str,
    ) -> Result<Vec<ObjectStats>> {
        let mut entries: Vec<ObjectStats> = self
            .paged(
                &format!(
                    "/repositories/{}/refs/{}/objects/ls",
                    enc(repo),
                    enc(reference)
                ),
                &[
                    ("prefix", prefix.to_string()),
                    ("delimiter", "/".to_string()),
                ],
                "listing objects",
                50_000,
            )
            .await?;
        // Directories first, then files, each alphabetical.
        entries.sort_by(|a, b| {
            b.is_dir()
                .cmp(&a.is_dir())
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(entries)
    }

    /// Fetch at most `limit` bytes of an object using a Range request.
    pub async fn get_object_head(
        &self,
        repo: &str,
        reference: &str,
        path: &str,
        limit: u64,
    ) -> Result<Vec<u8>> {
        let resp = self
            .get(&format!(
                "/repositories/{}/refs/{}/objects",
                enc(repo),
                enc(reference)
            ))
            .query(&[("path", path)])
            .header("Range", format!("bytes=0-{}", limit.saturating_sub(1)))
            .send()
            .await
            .context("downloading object")?;
        let resp = Self::check(resp, "downloading object").await?;
        let bytes = resp.bytes().await.context("reading object body")?;
        Ok(bytes.to_vec())
    }

    /// Stream a whole object into `sink`, returning the bytes written.
    ///
    /// No `Range` header, unlike `get_object_head`: a download is the whole
    /// object, whatever `preview_bytes` caps the preview at. The body is written
    /// through as it arrives rather than collected, so an object far larger than
    /// memory still lands.
    pub async fn download_object(
        &self,
        repo: &str,
        reference: &str,
        path: &str,
        sink: &mut (impl AsyncWrite + Unpin),
    ) -> Result<u64> {
        let resp = self
            .get(&format!(
                "/repositories/{}/refs/{}/objects",
                enc(repo),
                enc(reference)
            ))
            .query(&[("path", path)])
            .send()
            .await
            .context("downloading object")?;
        let resp = Self::check(resp, "downloading object").await?;

        let mut written = 0u64;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading object body")?;
            sink.write_all(&chunk).await.context("writing to disk")?;
            written += chunk.len() as u64;
        }
        sink.flush().await.context("writing to disk")?;
        Ok(written)
    }

    pub async fn commits(&self, repo: &str, reference: &str) -> Result<Vec<Commit>> {
        self.paged(
            &format!(
                "/repositories/{}/refs/{}/commits",
                enc(repo),
                enc(reference)
            ),
            &[],
            "listing commits",
            2_000,
        )
        .await
    }

    /// Cheap round-trip that also validates the credentials.
    pub async fn verify(&self) -> Result<()> {
        let resp = self
            .get("/repositories")
            .query(&[("amount", "1")])
            .send()
            .await
            .context("connecting to lakeFS")?;
        Self::check(resp, "connecting to lakeFS").await?;
        Ok(())
    }
}

/// Whether a repository's branches are worth listing under it, given no more
/// than the first two. Must agree with the browser's own rule (`RefsSlot::
/// visible`), which hides a lone default branch because the repository row
/// already selects it — if the two drift, the pane's chevron stops matching
/// what expanding it shows.
fn branches_are_listable(branches: &[RefEntry], default_branch: &str) -> bool {
    branches.len() > 1 || branches.iter().any(|b| b.id != default_branch)
}

/// Percent-encode a single path segment (repo ids and refs may contain `/`).
fn enc(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> RefEntry {
        RefEntry {
            id: id.into(),
            commit_id: "c0ffee".into(),
        }
    }

    /// Each case here has a twin in `app::tests`, over the same repository shape
    /// but the full ref list. They must reach the same answer.
    #[test]
    fn a_lone_default_branch_is_not_listable() {
        assert!(!branches_are_listable(&[entry("main")], "main"));
    }

    #[test]
    fn a_second_branch_makes_them_listable() {
        // The probe caps at two, which is all it takes to know.
        assert!(branches_are_listable(&[entry("main"), entry("dev")], "main"));
    }

    #[test]
    fn a_lone_branch_that_is_not_the_default_is_listable() {
        assert!(branches_are_listable(&[entry("orphan")], "main"));
    }

    #[test]
    fn no_branches_are_not_listable() {
        assert!(!branches_are_listable(&[], "main"));
    }

    fn profile(endpoint: String) -> Profile {
        Profile {
            endpoint,
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
            default_repo: None,
            default_ref: None,
            verify_tls: true,
            timeout_secs: 5,
            description: None,
        }
    }

    /// Serves one canned 200 on a loopback port, handing back the request head
    /// it was sent so the caller can check what was asked for.
    async fn serve_once(body: String) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());

            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        });

        (format!("http://{addr}"), rx)
    }

    /// The whole body reaches the sink, in however many chunks it arrives — and
    /// no `Range` header goes out, which is what separates a download from the
    /// capped fetch the preview makes.
    #[tokio::test]
    async fn a_download_streams_the_whole_body_and_asks_for_all_of_it() {
        let body = "x".repeat(200_000);
        let (endpoint, request) = serve_once(body.clone()).await;
        let client = Client::new(&profile(endpoint), 500).unwrap();

        let mut sink: Vec<u8> = Vec::new();
        let written = client
            .download_object("repo", "main", "data/big.bin", &mut sink)
            .await
            .unwrap();

        assert_eq!(written, body.len() as u64);
        assert_eq!(sink, body.as_bytes());

        let head = request.await.unwrap();
        assert!(
            !head.to_ascii_lowercase().contains("range:"),
            "a download is the whole object: {head}"
        );
        assert!(head.contains("path=data%2Fbig.bin"), "{head}");
        assert!(
            head.contains("authorization:") || head.contains("Authorization:"),
            "{head}"
        );
    }

    /// A failed download reports the server's own message rather than writing a
    /// file full of the error body.
    #[tokio::test]
    async fn a_download_that_fails_says_why() {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let body = r#"{"message":"path not found"}"#;
            let head = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        });

        let client = Client::new(&profile(format!("http://{addr}")), 500).unwrap();
        let mut sink: Vec<u8> = Vec::new();
        let err = client
            .download_object("repo", "main", "nope", &mut sink)
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("path not found"), "{err}");
        assert!(sink.is_empty(), "nothing is written when the fetch fails");
    }
}
