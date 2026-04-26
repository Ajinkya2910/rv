// src/registry/mod.rs — Fetching and parsing package metadata from CRAN + Bioconductor
//
// This is the module that talks to the outside world. It downloads the
// PACKAGES file from CRAN and Bioconductor, parses it, and gives us a
// structured database of all available packages.
//
// RUST CONCEPT: mod.rs
// When a module is a directory (src/registry/), Rust looks for mod.rs
// as the entry point. Sub-modules like `parser.rs` are declared here.
//
// RUST CONCEPT: Ownership & Borrowing (The Big One)
// Rust has no garbage collector. Instead, every value has exactly ONE owner.
// When the owner goes out of scope, the value is dropped (freed).
//
//   let s = String::from("hello");  // s owns the string
//   let t = s;                       // ownership MOVES to t. s is now invalid!
//   // println!("{}", s);            // COMPILE ERROR: s was moved
//
// To let multiple places READ a value without taking ownership, you use
// references (&):
//   let s = String::from("hello");
//   let len = calculate_length(&s);  // borrows s, doesn't take ownership
//   println!("{}", s);               // s is still valid!
//
// Don't worry — the compiler tells you exactly what's wrong. Just follow
// the error messages and you'll learn fast.

mod parser;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// --- Data Structures ---
//
// RUST CONCEPT: #[derive(...)]
// These are auto-generated trait implementations:
//   Debug    — lets you print with {:?} (like Python's __repr__)
//   Clone    — lets you make copies with .clone()
//   Serialize/Deserialize — JSON/TOML conversion via serde

/// Represents a single R package's metadata, parsed from PACKAGES file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMetadata {
    /// Package name (e.g., "DESeq2")
    pub name: String,

    /// Version string (e.g., "1.42.0")
    pub version: String,

    /// Where this package comes from
    pub source: PackageSource,

    /// Hard dependencies: packages that MUST be installed
    /// Parsed from the "Depends:" field in PACKAGES
    pub depends: Vec<Dependency>,

    /// Imported packages: used via namespace but not attached
    /// Parsed from the "Imports:" field
    pub imports: Vec<Dependency>,

    /// C/C++ header dependencies (LinkingTo field)
    /// These packages provide header files for compilation
    pub linking_to: Vec<String>,

    /// Whether this package has native code (C/C++/Fortran)
    /// True if the package has a src/ directory
    pub needs_compilation: bool,

    /// Free-text system requirements (e.g., "GNU make, libcurl")
    pub system_requirements: Option<String>,
}

/// Where a package comes from
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PackageSource {
    Cran,
    Bioconductor,
    // Future: GitHub, R-universe
}
/// Common interface for package metadata, regardless of source.
///
/// RUST CONCEPT: trait
/// Like a Python abstract base class or a Java interface — a contract that
/// types can implement. The resolver works against this trait, so it doesn't
/// care if a package came from CRAN, Bioconductor, or GitHub.
///
/// All methods return references (&) — no ownership transfer, no allocations.
pub trait PackageMeta {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn depends(&self) -> &[Dependency];
    fn imports(&self) -> &[Dependency];
    fn linking_to(&self) -> &[String];
    fn needs_compilation(&self) -> bool;
    fn system_requirements(&self) -> Option<&str>;
      fn source_label(&self) -> &'static str;  
}

impl PackageMeta for PackageMetadata {
    fn name(&self) -> &str { &self.name }
    fn version(&self) -> &str { &self.version }
    fn depends(&self) -> &[Dependency] { &self.depends }
    fn imports(&self) -> &[Dependency] { &self.imports }
    fn linking_to(&self) -> &[String] { &self.linking_to }
    fn needs_compilation(&self) -> bool { self.needs_compilation }
    fn system_requirements(&self) -> Option<&str> {
        self.system_requirements.as_deref()
    }
     fn source_label(&self) -> &'static str {   // NEW
        match self.source {
            PackageSource::Cran => "cran",
            PackageSource::Bioconductor => "bioc",
        }
     }
}

// RUST CONCEPT: Display trait
// Implementing `Display` lets you control how a type prints with `{}`.
// Like Python's __str__.
impl std::fmt::Display for PackageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `self` is a reference to this enum value.
            // `write!` is like format! but writes to a formatter.
            PackageSource::Cran => write!(f, "cran"),
            PackageSource::Bioconductor => write!(f, "bioc"),
        }
    }
}

/// A dependency with optional version constraint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    /// Version constraint like ">= 1.2.0" or None for any version
    pub version_req: Option<String>,
}

/// The complete registry: all packages from all sources
#[derive(Debug)]
pub struct Registry {
    /// All packages indexed by name, each with one or more versions
    /// (sorted newest first). Currently only latest from CRAN/Bioc,
    /// but Archive versions can be added later.
    pub packages: HashMap<String, Vec<PackageMetadata>>,

    /// R version detected on this system
    pub r_version: String,

    /// Bioconductor release version (e.g., "3.19")
    pub bioc_version: String,
}

// RUST CONCEPT: `impl` blocks
// This is where you define methods on a struct — like a class in Python.
// `pub fn` means the function is public (accessible from other modules).
// `async fn` means the function is asynchronous (can use .await).

impl Registry {
    /// Fetch package metadata from CRAN and Bioconductor.
    ///
    /// This downloads the PACKAGES.gz files, parses them, and builds
    /// an in-memory database of all available packages.
    pub async fn fetch() -> Result<Self> {
        // Detect R version on this system
        let r_version = detect_r_version()?;

        // Map R version to Bioconductor release
        let bioc_version = map_bioc_version(&r_version)?;

        // RUST CONCEPT: async/await
        // `fetch_cran_packages().await` pauses this function until the HTTP
        // request completes, but doesn't block the thread — other async tasks
        // can run while we wait. Like Python's `await`.

        // Fetch CRAN packages
        let cran_url = "https://cloud.r-project.org/src/contrib/PACKAGES.gz";
        let cran_packages = fetch_and_parse(cran_url, PackageSource::Cran)
            .await
            .context("Failed to fetch CRAN package index")?;

        // Fetch Bioconductor packages
        let bioc_url = format!(
            "https://bioconductor.org/packages/{}/bioc/src/contrib/PACKAGES.gz",
            bioc_version
        );
        let bioc_packages = fetch_and_parse(&bioc_url, PackageSource::Bioconductor)
            .await
            .context("Failed to fetch Bioconductor package index")?;
        // Fetch Bioconductor annotation packages
        let bioc_annotation_url = format!(
            "https://bioconductor.org/packages/{}/data/annotation/src/contrib/PACKAGES.gz",
            bioc_version
        );
        let bioc_annotation = fetch_and_parse(&bioc_annotation_url, PackageSource::Bioconductor)
            .await
            .unwrap_or_else(|_| Vec::new());  // Don't fail if annotation repo is unavailable

        // Merge into a single HashMap
        // RUST CONCEPT: `mut` — variables are immutable by default in Rust.
        // You must explicitly say `mut` to allow modification.
        // This prevents accidental mutations and makes code easier to reason about.
        let mut packages: HashMap<String, Vec<PackageMetadata>> = HashMap::new();

        for pkg in cran_packages {
            packages.entry(pkg.name.clone()).or_default().push(pkg);
        }

        // Bioconductor packages go first (higher priority)
        for pkg in bioc_packages {
            packages.entry(pkg.name.clone()).or_default().insert(0, pkg);
        }
        for pkg in bioc_annotation {
            packages.entry(pkg.name.clone()).or_default().insert(0, pkg);
        }

        println!(
            "  Loaded {} CRAN + Bioconductor packages",
            packages.len()
        );

        Ok(Registry {
            packages,
            r_version,
            bioc_version,
        })
    }

    /// Look up a package by name — returns the latest/best version as a trait object.
    ///
    /// RUST CONCEPT: &dyn Trait
    /// Returns a reference to "something that implements PackageMeta" — could be
    /// a CRAN package, a Bioconductor package, or (later) a GitHub package.
    /// The resolver doesn't need to know which.
    pub fn get(&self, name: &str) -> Option<&dyn PackageMeta> {
        self.packages
            .get(name)
            .and_then(|versions| versions.first())
            .map(|p| p as &dyn PackageMeta)
    }

    /// CRAN/Bioc-only lookup that returns the concrete type.
    /// Used by archive-version logic which needs to clone PackageMetadata.
    pub fn get_concrete(&self, name: &str) -> Option<&PackageMetadata> {
        self.packages.get(name).and_then(|versions| versions.first())
    }
    /// Get all available versions of a package (for backtracking resolver)
    pub fn get_all_versions(&self, name: &str) -> Option<&Vec<PackageMetadata>> {
        self.packages.get(name)
    }
    /// Fetch archived versions of a package from CRAN Archive.
    /// Returns versions sorted newest first.
    pub async fn fetch_archive_versions(&mut self, pkg_name: &str) -> Result<Vec<PackageMetadata>> {
        let url = format!(
            "https://cloud.r-project.org/src/contrib/Archive/{}/",
            pkg_name
        );

        let response = reqwest::get(&url).await;

        let body = match response {
            Ok(resp) if resp.status().is_success() => {
                resp.text().await.unwrap_or_default()
            }
            _ => return Ok(Vec::new()), // No archive exists
        };

        // Parse the HTML listing for .tar.gz files
        // Lines look like: <a href="rlang_1.0.0.tar.gz">rlang_1.0.0.tar.gz</a>
        let mut versions: Vec<PackageMetadata> = Vec::new();

        for line in body.lines() {
            // Look for href="pkgname_version.tar.gz"
            let pattern = format!("{}_", pkg_name);
            if let Some(start) = line.find(&pattern) {
                let rest = &line[start + pattern.len()..];
                if let Some(end) = rest.find(".tar.gz") {
                    let version_str = &rest[..end];

                    // Create a minimal PackageMetadata for this archived version
                    // We don't have full dependency info — we'd need to download
                    // and parse each tarball's DESCRIPTION for that.
                    // For now, clone the latest version's metadata but swap the version.
                    if let Some(latest) = self.get_concrete(pkg_name) {
                        let mut archived = latest.clone();
                        archived.version = version_str.to_string();
                        versions.push(archived);
                    }
                }
            }
        }

        // Sort newest first using our RVersion comparison
        versions.sort_by(|a, b| {
            let va = crate::version::RVersion::parse(&a.version);
            let vb = crate::version::RVersion::parse(&b.version);
            match (va, vb) {
                (Some(a), Some(b)) => b.cmp(&a), // reverse: newest first
                _ => std::cmp::Ordering::Equal,
            }
        });

        // Add to our registry for future lookups
        if !versions.is_empty() {
            let entry = self.packages.entry(pkg_name.to_string()).or_default();
            for v in &versions {
                if !entry.iter().any(|existing| existing.version == v.version) {
                    entry.push(v.clone());
                }
            }
        }

        Ok(versions)
    }
}

/// Fetch a PACKAGES.gz file from a URL and parse it
async fn fetch_and_parse(url: &str, source: PackageSource) -> Result<Vec<PackageMetadata>> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    // Download the gzipped PACKAGES file
    let response = reqwest::get(url).await?.bytes().await?;

    // Decompress gzip
    // RUST CONCEPT: Vec<u8> is a vector of bytes — like Python's bytes/bytearray.
    let mut decoder = GzDecoder::new(&response[..]);
    let mut content = String::new();
    decoder
        .read_to_string(&mut content)
        .context("Failed to decompress PACKAGES.gz")?;

    // Parse the PACKAGES format into our structs
    let packages = parser::parse_packages(&content, source)?;

    Ok(packages)
}

/// Detect the installed R version by running `R --version`
fn detect_r_version() -> Result<String> {
    // TEMPORARY: fake old R version to test constraint checking
       //return Ok("4.3.0".to_string());

    use std::process::Command;

    // RUST CONCEPT: std::process::Command
    // Like Python's subprocess.run(). Runs an external command.
    let output = Command::new("R")
        .arg("--version")
        .output()
        .context("R is not installed. Install R first: https://cran.r-project.org")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse version from output like "R version 4.4.0 (2024-04-24)"
    // RUST CONCEPT: `if let` is a concise pattern match for a single case.
    // It's like: if the pattern matches, run this code; otherwise skip.
    if let Some(line) = stdout.lines().next() {
        if let Some(version_str) = line.strip_prefix("R version ") {
            if let Some(version) = version_str.split_whitespace().next() {
                return Ok(version.to_string());
            }
        }
    }

    // RUST CONCEPT: anyhow::bail! is a macro that creates an error and returns early.
    // It's shorthand for: return Err(anyhow::anyhow!("message"))
    anyhow::bail!("Could not detect R version from `R --version` output")
}

/// Map R version to Bioconductor release version
fn map_bioc_version(r_version: &str) -> Result<String> {
    // RUST CONCEPT: match with string patterns
    // This hardcodes the R → Bioconductor mapping.
    // In a production version, we'd fetch this from Bioconductor's config.yaml.

    // Extract major.minor from r_version (e.g., "4.4.0" → "4.4")
    let parts: Vec<&str> = r_version.split('.').collect();
    let major_minor = format!("{}.{}", parts[0], parts[1]);

    let bioc = match major_minor.as_str() {
        "4.3" => "3.18",
        "4.4" => "3.19",
        "4.5" => "3.20",
        "4.6" => "3.21",
        _ => {
            anyhow::bail!(
                "Unknown R version {} — cannot determine Bioconductor release. \
                 Supported: R 4.3-4.6",
                r_version
            );
        }
    };

    Ok(bioc.to_string())
}
