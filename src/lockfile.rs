// src/lockfile.rs — Lockfile generation and parsing
//
// The lockfile (rv.lock) captures the exact resolved state so it can
// be reproduced later. It records every package, version, source,
// and checksum.
//
// Format: TOML (like Cargo.lock) — human-readable and diffable.
//
// RUST CONCEPT: File I/O
// Rust's file I/O uses std::fs (filesystem) and std::io (input/output).
// Unlike Python where `open()` is a builtin, Rust uses explicit imports.
// Error handling is via Result — no exceptions.

use crate::resolver::ResolvedDeps;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The structure of an rv.lock file
///
/// RUST CONCEPT: Serialize + Deserialize
/// With these derives, this struct can be converted to/from TOML
/// automatically. `serde` handles all the parsing.
///
///   let toml_string = toml::to_string(&lockfile)?;  // struct → string
///   let lockfile: Lockfile = toml::from_str(&text)?;  // string → struct
#[derive(Debug, Serialize, Deserialize)]
pub struct Lockfile {
    /// Metadata about the lockfile
    pub metadata: LockfileMetadata,

    /// All locked packages
    #[serde(rename = "package")]
    pub packages: Vec<LockedPackage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LockfileMetadata {
    /// rv version that generated this lockfile
    pub rv_version: String,

    /// R version the lockfile was generated for
    pub r_version: String,

    /// Bioconductor release version
    pub bioc_version: String,

    /// When the lockfile was generated (ISO 8601)
    pub generated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub source: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,

    pub deps: Vec<String>,

    // ── GitHub-source fields ──────────────────────────────────────────
    // All optional. None for CRAN/Bioc packages. Lockfiles written before
    // GitHub support deserialize cleanly because every new field is Option.

    /// "owner/repo", e.g. "satijalab/seurat-data"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,

    /// Resolved 40-char commit SHA. Field name is `ref` in the TOML
    /// because that's what users will read; `r#ref` here because `ref`
    /// is a Rust keyword.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,

    /// SHA-256 of the tarball at `ref`. Used for integrity verification
    /// on `rv restore`. Mismatch → bail loudly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tarball_sha256: Option<String>,

    /// Subdirectory inside the repo, for monorepos.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
}

/// Write a lockfile from resolved dependencies
///
/// RUST CONCEPT: PathBuf vs Path
///   - PathBuf is like String — an owned, mutable filesystem path
///   - Path is like &str — a borrowed reference to a path
///   - PathBuf : Path :: String : &str
///
/// We return PathBuf because we're creating a new path.
pub fn write(resolved: &ResolvedDeps) -> Result<PathBuf> {
    // Build the lockfile structure
    let lockfile = Lockfile {
        metadata: LockfileMetadata {
            rv_version: env!("CARGO_PKG_VERSION").to_string(),
            r_version: "4.4.0".to_string(), // TODO: get from registry
            bioc_version: "3.19".to_string(), // TODO: get from registry
            generated_at: chrono_now(),
        },
        packages: resolved
            .packages
            .iter()
            .map(|pkg| {
                
                // For GitHub packages, lift provenance fields out of
                // ResolvedPackage.github_source.
                let (repo, r#ref, tarball_sha256, subdir) = match &pkg.github_source {
                    Some(gh) => (
                        Some(format!("{}/{}", gh.owner, gh.repo)),
                        Some(gh.commit_sha.clone()),
                        Some(gh.tarball_sha256.clone()),
                        gh.subdir.clone(),
                    ),
                    None => (None, None, None, None),
                };

                LockedPackage {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    source: pkg.source.clone(),
                    sha256: pkg.sha256.clone(),
                    deps: pkg.dependencies.clone(),
                    repo,
                    r#ref,
                    tarball_sha256,
                    subdir,
                }
            })
            .collect(),
    };

    // Serialize to TOML
    // RUST CONCEPT: toml::to_string_pretty() converts any Serialize
    // struct into a formatted TOML string.
    let toml_content = toml::to_string_pretty(&lockfile)
        .context("Failed to serialize lockfile to TOML")?;

    // Add a header comment (TOML doesn't support comments in serde,
    // so we prepend them manually)
    let content = format!(
        "# This file is auto-generated by rv. Do not edit manually.\n\
         # Commit this file to version control for reproducible installs.\n\
         # Restore with: rv restore\n\n\
         {}",
        toml_content
    );

    // Write to rv.lock in the current directory
    let path = PathBuf::from("rv.lock");
    std::fs::write(&path, content).context("Failed to write rv.lock")?;

    Ok(path)
}

/// Read an existing lockfile
///
/// RUST CONCEPT: impl AsRef<Path>
/// This is a "generic bound" — it means the function accepts anything
/// that can be converted to a Path reference. This includes:
///   - &str ("rv.lock")
///   - String
///   - PathBuf
///   - &Path
/// It's Rust's way of being flexible about path inputs without
/// multiple overloads.
pub fn read(path: impl AsRef<Path>) -> Result<Lockfile> {
    let content = std::fs::read_to_string(path.as_ref())
        .context("Failed to read rv.lock — are you in a project directory?")?;

    let lockfile: Lockfile = toml::from_str(&content).context(
        "Failed to parse rv.lock — the file may be corrupted. \
         Delete it and run `rv lock` again.",
    )?;

    Ok(lockfile)
}

/// Simple timestamp function (avoids adding chrono dependency for MVP)
pub fn chrono_now() -> String {
    // RUST CONCEPT: SystemTime for timestamps
    // For a proper implementation, you'd use the `chrono` crate.
    // For MVP, we use a Unix timestamp.
    use std::time::SystemTime;

    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();

    format!("{}", duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::{ResolvedDeps, ResolvedPackage};

    #[test]
    fn test_lockfile_roundtrip() {
        // Create a resolved deps set
        let resolved = ResolvedDeps {
            packages: vec![
                ResolvedPackage {
                    name: "BiocGenerics".to_string(),
                    version: "0.48.1".to_string(),
                    source: "bioc".to_string(),
                    needs_compilation: false,
                    dependencies: vec![],
                    sha256: None,
                    github_source: None,
                },
                ResolvedPackage {
                    name: "S4Vectors".to_string(),
                    version: "0.40.2".to_string(),
                    source: "bioc".to_string(),
                    needs_compilation: true,
                    dependencies: vec!["BiocGenerics".to_string()],
                    sha256: None,
                    github_source: None,
                },
            ],
            duration_secs: 0.1,
        };

        // Write lockfile
        let path = write(&resolved).unwrap();

        // Read it back
        let lockfile = read(&path).unwrap();

        assert_eq!(lockfile.packages.len(), 2);
        assert_eq!(lockfile.packages[0].name, "BiocGenerics");
        assert_eq!(lockfile.packages[1].name, "S4Vectors");
        assert_eq!(lockfile.packages[1].deps, vec!["BiocGenerics"]);

        // Cleanup
        std::fs::remove_file(path).ok();
    }

#[test]
    fn test_lockfile_github_roundtrip() {
        use crate::resolver::GitHubSource;

        let resolved = ResolvedDeps {
            packages: vec![ResolvedPackage {
                name: "SeuratData".to_string(),
                version: "0.2.2.9002".to_string(),
                source: "github".to_string(),
                needs_compilation: false,
                dependencies: vec!["Seurat".to_string()],
                sha256: None,
                github_source: Some(GitHubSource {
                    owner: "satijalab".to_string(),
                    repo: "seurat-data".to_string(),
                    commit_sha: "3e51f44303069b64f5dc4d68e6a3d4a343f55c39".to_string(),
                    subdir: None,
                    tarball_sha256:
                        "881ebe70a2a6c6574916925d9e3a70b66c32806ddc38a472d03f90e0a146fc00"
                            .to_string(),
                }),
            }],
            duration_secs: 0.0,
        };

        let path = std::env::temp_dir().join("rv-test-gh.lock");
        // Reuse write() but redirect to temp — simplest is to write+read+cleanup
        // through the real write() since it always writes to "rv.lock".
        // For unit purposes, we serialize/deserialize directly:
        let lockfile = Lockfile {
            metadata: LockfileMetadata {
                rv_version: "test".to_string(),
                r_version: "4.4.0".to_string(),
                bioc_version: "3.19".to_string(),
                generated_at: "0".to_string(),
            },
            packages: vec![LockedPackage {
                name: resolved.packages[0].name.clone(),
                version: resolved.packages[0].version.clone(),
                source: resolved.packages[0].source.clone(),
                sha256: None,
                deps: resolved.packages[0].dependencies.clone(),
                repo: Some("satijalab/seurat-data".to_string()),
                r#ref: Some(
                    "3e51f44303069b64f5dc4d68e6a3d4a343f55c39".to_string(),
                ),
                tarball_sha256: Some(
                    "881ebe70a2a6c6574916925d9e3a70b66c32806ddc38a472d03f90e0a146fc00"
                        .to_string(),
                ),
                subdir: None,
            }],
        };

        let toml = toml::to_string_pretty(&lockfile).unwrap();
        assert!(toml.contains("repo = \"satijalab/seurat-data\""));
        assert!(toml.contains("ref = \"3e51f44"));
        assert!(toml.contains("tarball_sha256 ="));

        let back: Lockfile = toml::from_str(&toml).unwrap();
        assert_eq!(back.packages[0].repo.as_deref(), Some("satijalab/seurat-data"));
        assert_eq!(back.packages[0].r#ref.as_ref().unwrap().len(), 40);
        let _ = path; // unused
    }

    #[test]
    fn test_lockfile_old_format_loads() {
        // Lockfile written before GitHub support — no repo/ref/tarball_sha256/subdir.
        let old_toml = r#"
[metadata]
rv_version = "0.1.0"
r_version = "4.4.0"
bioc_version = "3.19"
generated_at = "0"

[[package]]
name = "BiocGenerics"
version = "0.48.1"
source = "bioc"
deps = []
"#;
        let lockfile: Lockfile = toml::from_str(old_toml).unwrap();
        assert_eq!(lockfile.packages.len(), 1);
        assert!(lockfile.packages[0].repo.is_none());
        assert!(lockfile.packages[0].r#ref.is_none());
    }
}
