// src/registry/github.rs — Talking to GitHub.
//
// Two things this module does:
//   1. Resolve a ref (branch/tag/sha) to a 40-char commit SHA.
//   2. Download the tarball at that SHA.
//
// Both go through a local cache. Both authenticate with GITHUB_TOKEN
// when it's set. Errors are classified into actionable user-facing
// messages — not raw HTTP status codes.

use crate::source::GitHubSpec;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::registry::{Dependency, PackageMeta};

const REF_CACHE_TTL_SECS: u64 = 24 * 60 * 60; // refs can move; commit SHAs are eternal.

// --- Public API: resolve_ref ------------------------------------------------

/// Resolve a branch/tag/SHA to a 40-char commit SHA.
///
/// Fast path: if the user provided a SHA already, return it without an
/// API call. Cache hits also skip the API. Otherwise hit GitHub.
pub async fn resolve_ref(
    spec: &GitHubSpec,
    cache_dir: &Path,
    client: &reqwest::Client,
) -> Result<String> {
    let r#ref = spec.r#ref.as_deref().unwrap_or("HEAD");

    // Already a SHA → trust it.
    if is_sha(r#ref) {
        return Ok(r#ref.to_string());
    }

    if let Some(sha) = read_cached_ref(cache_dir, &spec.owner, &spec.repo, r#ref)? {
        return Ok(sha);
    }

    let sha = api_resolve_ref(spec, r#ref, client).await?;
    write_cached_ref(cache_dir, &spec.owner, &spec.repo, r#ref, &sha)?;
    Ok(sha)
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

// --- Ref cache --------------------------------------------------------------

fn ref_cache_path(cache_dir: &Path, owner: &str, repo: &str, r#ref: &str) -> PathBuf {
    cache_dir
        .join("github")
        .join("refs")
        .join(owner)
        .join(repo)
        .join(format!("{}.txt", urlencoding::encode(r#ref)))
}

fn read_cached_ref(
    cache_dir: &Path,
    owner: &str,
    repo: &str,
    r#ref: &str,
) -> Result<Option<String>> {
    let path = ref_cache_path(cache_dir, owner, repo, r#ref);
    if !path.exists() {
        return Ok(None);
    }

    // Stale check.
    let mtime = std::fs::metadata(&path)?
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if now.saturating_sub(mtime) > REF_CACHE_TTL_SECS {
        return Ok(None);
    }

    // Corruption check.
    let content = std::fs::read_to_string(&path)?.trim().to_string();
    if is_sha(&content) {
        Ok(Some(content))
    } else {
        Ok(None)
    }
}

fn write_cached_ref(
    cache_dir: &Path,
    owner: &str,
    repo: &str,
    r#ref: &str,
    sha: &str,
) -> Result<()> {
    let path = ref_cache_path(cache_dir, owner, repo, r#ref);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, sha)?;
    Ok(())
}

// --- GitHub API call --------------------------------------------------------

async fn api_resolve_ref(
    spec: &GitHubSpec,
    r#ref: &str,
    client: &reqwest::Client,
) -> Result<String> {
    #[derive(Deserialize)]
    struct CommitResp {
        sha: String,
    }

    let url = format!(
        "https://api.github.com/repos/{}/{}/commits/{}",
        spec.owner, spec.repo, r#ref
    );

    let mut req = client
        .get(&url)
        .header("User-Agent", "rv")
        .header("Accept", "application/vnd.github+json");

    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
    }

    let resp = req.send().await?;
    let status = resp.status();
    let headers = resp.headers().clone();

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(classify_github_error(status, &headers, &body, spec, r#ref));
    }

    let parsed: CommitResp = resp
        .json()
        .await
        .with_context(|| {
            format!(
                "Parsing commit response for {}/{}@{}",
                spec.owner, spec.repo, r#ref
            )
        })?;
    Ok(parsed.sha)
}

// --- Error classification ---------------------------------------------------
//
// RUST CONCEPT: returning anyhow::Error
// Instead of panicking or returning raw strings, we build an Error value
// with a clean message. Callers either propagate it (with `?`) or print
// it (with `{}` since anyhow::Error implements Display).

fn classify_github_error(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &str,
    spec: &GitHubSpec,
    r#ref: &str,
) -> anyhow::Error {
    use reqwest::StatusCode;

    match status {
        StatusCode::NOT_FOUND => {
            if body.to_lowercase().contains("not found") {
                anyhow!(
                    "GitHub: repository or ref not found: {}/{}@{}\n\
                     - Verify the repo exists and is public\n\
                     - For private repos, set GITHUB_TOKEN with read access",
                    spec.owner, spec.repo, r#ref
                )
            } else {
                anyhow!("GitHub returned 404 for {}/{}@{}",
                        spec.owner, spec.repo, r#ref)
            }
        }
        StatusCode::FORBIDDEN => {
            let remaining = headers
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok());
            let reset = headers
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            if remaining == Some("0") {
                let wait_msg = match reset {
                    Some(reset_ts) => {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let wait_min = reset_ts.saturating_sub(now) / 60;
                        format!("Resets in {} minutes.", wait_min)
                    }
                    None => "Wait an hour.".to_string(),
                };
                anyhow!(
                    "GitHub API rate limit exceeded.\n\
                     - Set GITHUB_TOKEN env var (5000/hour vs. 60/hour)\n\
                     - {}",
                    wait_msg
                )
            } else {
                anyhow!(
                    "GitHub forbidden: {}/{}@{} — check repository permissions",
                    spec.owner, spec.repo, r#ref
                )
            }
        }
        StatusCode::UNAUTHORIZED => {
            anyhow!("GitHub authentication failed — check that GITHUB_TOKEN is valid")
        }
        s if s.is_server_error() => {
            anyhow!("GitHub server error ({}). Try again in a few minutes.", s)
        }
        s => {
            anyhow!("GitHub API error ({}): {}/{}@{}",
                    s, spec.owner, spec.repo, r#ref)
        }
    }
}

// --- Public API: download_tarball ------------------------------------------

/// Download a tarball at a given commit SHA. Returns (cached path, sha256 hex).
///
/// codeload.github.com is the right host for this — it serves the same
/// data git would but without needing git installed, and without the
/// API rate limit (it's a separate quota).
pub async fn download_tarball(
    spec: &GitHubSpec,
    sha: &str,
    cache_dir: &Path,
    client: &reqwest::Client,
) -> Result<(PathBuf, String)> {
    use std::io::Write;

    let pkg_cache = cache_dir
        .join("github")
        .join(&spec.owner)
        .join(&spec.repo);
    std::fs::create_dir_all(&pkg_cache)?;
    let tarball_path = pkg_cache.join(format!("{}.tar.gz", sha));

    // Cache hit?
    if tarball_path.exists() {
        let bytes = std::fs::read(&tarball_path)?;
        if bytes.len() > 1000 {
            let digest = hex::encode(Sha256::digest(&bytes));
            return Ok((tarball_path, digest));
        }
        // Suspiciously small — probably a cached HTML error from a previous run.
        std::fs::remove_file(&tarball_path)?;
    }

    let url = format!(
        "https://codeload.github.com/{}/{}/tar.gz/{}",
        spec.owner, spec.repo, sha
    );

    let mut req = client.get(&url).header("User-Agent", "rv");
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        bail!(
            "codeload.github.com returned {} for {}/{}@{}",
            resp.status(), spec.owner, spec.repo, sha
        );
    }

    let bytes = resp.bytes().await?.to_vec();

    // Sanity checks: too small, or doesn't start with gzip magic.
    if bytes.len() < 1000 {
        bail!(
            "Downloaded file too small ({} bytes) — got HTML instead of tarball. \
             Possible private repo or auth issue. Set GITHUB_TOKEN if needed.",
            bytes.len()
        );
    }
    if bytes[0..2] != [0x1f, 0x8b] {
        bail!("Downloaded file is not a gzip archive — possible HTML error page");
    }

    let digest = hex::encode(Sha256::digest(&bytes));

    // Atomic write: tmp → rename. Avoids leaving a half-written file
    // if rv is interrupted mid-download.
    let tmp = tarball_path.with_extension("tar.gz.tmp");
    std::fs::File::create(&tmp)?.write_all(&bytes)?;
    std::fs::rename(&tmp, &tarball_path)?;

    Ok((tarball_path, digest))
}
// --- GitHub package metadata ------------------------------------------------

/// Metadata for a GitHub-hosted package, parallel to PackageMetadata
/// but tracking GitHub-specific provenance (owner, repo, commit SHA,
/// tarball hash) for lockfile reproducibility.
#[derive(Debug, Clone)]
pub struct GitHubPackageMetadata {
    pub name: String,
    pub version: String,
    pub depends: Vec<Dependency>,
    pub imports: Vec<Dependency>,
    pub linking_to: Vec<String>,
    pub remotes: Vec<String>,
    pub needs_compilation: bool,
    pub system_requirements: Option<String>,

    // GitHub-specific provenance — for lockfile and rv why output.
    pub owner: String,
    pub repo: String,
    pub commit_sha: String,
    pub subdir: Option<String>,
    pub tarball_sha256: String,
}

impl PackageMeta for GitHubPackageMetadata {
    fn name(&self) -> &str { &self.name }
    fn version(&self) -> &str { &self.version }
    fn depends(&self) -> &[Dependency] { &self.depends }
    fn imports(&self) -> &[Dependency] { &self.imports }
    fn linking_to(&self) -> &[String] { &self.linking_to }
    fn needs_compilation(&self) -> bool { self.needs_compilation }
    fn system_requirements(&self) -> Option<&str> {
        self.system_requirements.as_deref()
    }
    fn source_label(&self) -> &'static str { "github" }
}

// --- DESCRIPTION extraction from a tarball ----------------------------------

/// Pull the DESCRIPTION file out of a downloaded tarball.
///
/// GitHub tarballs unpack into a top-level directory whose name varies
/// ({repo}-{sha}, {repo}-{tag}, sometimes truncated). We can't predict
/// the prefix, so we scan entries for the first one whose path ends in
/// `/DESCRIPTION` (or `/{subdir}/DESCRIPTION`) at the right depth.
pub fn extract_description(tarball_path: &Path, subdir: Option<&str>) -> Result<String> {
    use std::io::Read;

    let file = std::fs::File::open(tarball_path)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    let target_suffix = match subdir {
        Some(sub) => format!("/{}/DESCRIPTION", sub.trim_matches('/')),
        None => "/DESCRIPTION".to_string(),
    };

    // Expected path depth — how many '/' separators after the root dir.
    // For "{root}/DESCRIPTION" → 1.
    // For "{root}/sub/DESCRIPTION" → 2.
    // For "{root}/projects/r/DESCRIPTION" → 3.
    let expected_depth = match subdir {
        Some(sub) => sub.split('/').filter(|s| !s.is_empty()).count() + 1,
        None => 1,
    };

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path_str = entry.path()?.to_string_lossy().into_owned();

        if !path_str.ends_with(&target_suffix) {
            continue;
        }

        // Reject DESCRIPTION files at the wrong depth — guards against
        // picking up DESCRIPTION inside test fixtures, vignettes, etc.
        let depth = path_str.split('/').count() - 1;
        if depth != expected_depth {
            continue;
        }

        let mut content = String::new();
        entry.read_to_string(&mut content)?;
        return Ok(content);
    }

    let suggestion = match subdir {
        Some(s) => format!(
            "\nThe subdir '{}' may be wrong. \
             If the package is at the repo root, drop the subdir from the spec.",
            s
        ),
        None => "\nIf the package is in a subdirectory, use 'gh:owner/repo/subdir'.".to_string(),
    };
    bail!("No DESCRIPTION found in tarball{}", suggestion);
}

/// Find the package root inside an extracted tarball.
///
/// GitHub tarballs unpack into a top-level directory whose name we
/// don't know in advance ({repo}-{sha}, {repo}-{tag}, sometimes
/// truncated). Scan first-level dirs, return the one with a DESCRIPTION.
pub fn find_package_root(extract_dir: &Path, subdir: Option<&str>) -> Result<PathBuf> {
    for entry in std::fs::read_dir(extract_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let candidate = match subdir {
            Some(sub) => entry.path().join(sub.trim_matches('/')),
            None => entry.path(),
        };
        if candidate.join("DESCRIPTION").exists() {
            return Ok(candidate);
        }
    }
    bail!(
        "No package root with DESCRIPTION found in {}{}",
        extract_dir.display(),
        match subdir {
            Some(s) => format!(" (subdir: '{}')", s),
            None => String::new(),
        }
    );
}

// --- Top-level orchestration ------------------------------------------------

/// Resolve + download + extract + parse, returning fully-populated metadata.
///
/// This is the function the resolver calls. Errors here surface to the user
/// with all the context they need to fix the request.
pub async fn fetch_metadata(
    spec: &GitHubSpec,
    cache_dir: &Path,
    client: &reqwest::Client,
) -> Result<GitHubPackageMetadata> {
    let sha = resolve_ref(spec, cache_dir, client).await?;
    let (tarball_path, sha256) = download_tarball(spec, &sha, cache_dir, client).await?;
    let description_text = extract_description(&tarball_path, spec.subdir.as_deref())?;

    let parsed = crate::registry::parser::parse_description(&description_text)?;

    // GitHub packages MUST have a Version field. If they don't,
    // R CMD INSTALL would fail anyway — surface the clearer error here.
    let version = parsed.version.ok_or_else(|| {
        anyhow!(
            "GitHub package {}/{} has no Version: field in DESCRIPTION.\n\
             rv requires GitHub packages to have a valid version. \
             File an issue at the upstream repo.",
            spec.owner, spec.repo
        )
    })?;

    Ok(GitHubPackageMetadata {
        name: parsed.name,
        version,
        depends: parsed.depends,
        imports: parsed.imports,
        linking_to: parsed.linking_to,
        remotes: parsed.remotes,
        needs_compilation: parsed.needs_compilation,
        system_requirements: parsed.system_requirements,
        owner: spec.owner.clone(),
        repo: spec.repo.clone(),
        commit_sha: sha,
        subdir: spec.subdir.clone(),
        tarball_sha256: sha256,
    })
}
// --- Tests -----------------------------------------------------------------
//
// These hit the real network and require GITHUB_TOKEN to be set.
// Run with: cargo test github -- --ignored --nocapture
//
// They're #[ignore]d by default so `cargo test` stays fast and offline.

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cache_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_sha_passes_through() {
        // Already-a-SHA case — no network call needed.
        let spec = GitHubSpec {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            r#ref: Some("0000000000000000000000000000000000000000".to_string()),
            subdir: None,
        };
        let client = reqwest::Client::new();
        let cache = test_cache_dir();
        let sha = resolve_ref(&spec, &cache, &client).await.unwrap();
        assert_eq!(sha, "0000000000000000000000000000000000000000");
    }

     #[tokio::test]
    #[ignore] // hits the network
    async fn test_resolve_branch_to_sha() {
        let spec = GitHubSpec {
            owner: "octocat".to_string(),
            repo: "Hello-World".to_string(),
            r#ref: Some("master".to_string()),
            subdir: None,
        };
        let client = reqwest::Client::new();
        let cache = test_cache_dir();
        let sha = resolve_ref(&spec, &cache, &client).await.unwrap();
        assert!(is_sha(&sha), "expected 40-char hex SHA, got: {}", sha);
    }

   #[tokio::test]
    #[ignore] // hits the network
    async fn test_fetch_metadata_real_package() {
        let spec = GitHubSpec {
            owner: "satijalab".to_string(),
            repo: "seurat-data".to_string(),
            r#ref: None,
            subdir: None,
        };
        let client = reqwest::Client::new();
        let cache = test_cache_dir();

        let meta = fetch_metadata(&spec, &cache, &client).await.unwrap();

        // Print first so we can see what we got, even if asserts fail.
        println!("name:           {}", meta.name);
        println!("version:        {}", meta.version);
        println!("commit_sha:     {}", meta.commit_sha);
        println!("tarball_sha256: {}", meta.tarball_sha256);
        println!("depends:        {:?}",
                 meta.depends.iter().map(|d| &d.name).collect::<Vec<_>>());
        println!("imports:        {:?}",
                 meta.imports.iter().map(|d| &d.name).collect::<Vec<_>>());
        println!("remotes:        {:?}", meta.remotes);

        // Structural assertions — these don't depend on what the package
        // happens to declare, just that the pipeline produced sane data.
        assert!(!meta.name.is_empty());
        assert!(!meta.version.is_empty());
        assert_eq!(meta.commit_sha.len(), 40);
        assert_eq!(meta.tarball_sha256.len(), 64);
        assert_eq!(meta.owner, "satijalab");
        assert_eq!(meta.repo, "seurat-data");
    }
}
