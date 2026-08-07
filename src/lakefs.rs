//! A thin async client for the lakeFS OpenAPI (`/api/v1`).
//!
//! Only the read paths the browser needs are implemented. Every list call
//! follows pagination until the server stops or `max_entries` is reached.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

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
