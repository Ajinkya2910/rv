// src/resolver.rs — Dependency resolution engine
//
// This is the BRAIN of rv. Given a list of packages the user wants,
// it walks the entire dependency tree and produces a flat, ordered list
// of every package that needs to be installed, in the correct order
// (dependencies before dependents).
//
// RUST CONCEPT: Lifetimes (Don't Panic!)
// You might see `'a` in Rust code. These are "lifetime annotations."
// They tell the compiler how long references are valid. For now, we
// avoid them by cloning data. As you get comfortable, you'll learn
// to use references more efficiently. Clone is fine for an MVP.
//
// ALGORITHM: Topological Sort
// We need to install packages in dependency order — BiocGenerics before
// S4Vectors before IRanges before GenomicRanges before DESeq2.
// This is a topological sort on a directed acyclic graph (DAG).
// We use a simple DFS (depth-first search) approach.

use crate::registry::{PackageSource, Registry};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// The result of dependency resolution
#[derive(Debug)]
pub struct ResolvedDeps {
    /// Packages in installation order (dependencies first)
    pub packages: Vec<ResolvedPackage>,

    /// How long resolution took (in seconds)
    pub duration_secs: f64,
}

/// A single resolved package with all info needed for installation
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    pub source: String, // "cran" or "bioc"
    pub needs_compilation: bool,

    /// Direct dependencies of this package
    pub dependencies: Vec<String>,

    /// SHA256 hash of the source tarball (for lockfile)
    pub sha256: Option<String>,
}

/// Resolve the full dependency tree for a list of requested packages.
///
/// This is the main entry point. It:
/// 1. Walks the dependency tree recursively (DFS)
/// 2. Collects all transitive dependencies
/// 3. Returns them in topological order (install order)
///
/// RUST CONCEPT: &[String] is a "slice" — a reference to a contiguous
/// sequence of Strings. It's the idiomatic way to accept "a list of strings"
/// as a function parameter. Works with Vec<String>, arrays, etc.
pub fn resolve(registry: &Registry, requested: &[String]) -> Result<ResolvedDeps> {
    let start = Instant::now();

    // Track which packages we've already visited (avoid infinite loops
    // from circular dependencies, which shouldn't exist but might)
    //
    // RUST CONCEPT: HashSet is like Python's set().
    // Fast O(1) lookup for "have we seen this package?"
    let mut visited: HashSet<String> = HashSet::new();

    // The resolved packages in dependency-first order
    let mut resolved: Vec<ResolvedPackage> = Vec::new();

    // Resolve each requested package
    for pkg_name in requested {
        resolve_recursive(registry, pkg_name, &mut visited, &mut resolved)
            .with_context(|| format!("Failed to resolve package '{}'", pkg_name))?;
    }

    let duration = start.elapsed();

    Ok(ResolvedDeps {
        packages: resolved,
        duration_secs: duration.as_secs_f64(),
    })
}

/// Recursively resolve a single package and all its dependencies.
///
/// RUST CONCEPT: &mut means "mutable reference" — we're borrowing
/// `visited` and `resolved` and are allowed to modify them.
/// Only ONE mutable reference can exist at a time (prevents data races).
///
/// The algorithm:
/// 1. If already visited, skip (already resolved)
/// 2. Look up the package in the registry
/// 3. Recursively resolve all its dependencies FIRST
/// 4. Then add this package to the resolved list
///
/// This naturally produces topological order because we always
/// process dependencies before the dependent.
fn resolve_recursive(
    registry: &Registry,
    pkg_name: &str,
    visited: &mut HashSet<String>,
    resolved: &mut Vec<ResolvedPackage>,
) -> Result<()> {
    // Already processed? Skip.
    // RUST CONCEPT: .contains() on a HashSet is O(1)
    if visited.contains(pkg_name) {
        return Ok(());
    }

    // Mark as visited BEFORE recursing (prevents infinite loops)
    visited.insert(pkg_name.to_string());

    // Look up the package in the registry
    let metadata = registry.get(pkg_name).with_context(|| {
        format!(
            "Package '{}' not found in CRAN or Bioconductor. \
             Check the spelling or ensure you have the right Bioconductor version.",
            pkg_name
        )
    })?;

    // Collect all dependency names (Depends + Imports + LinkingTo)
    //
    // RUST CONCEPT: Iterator chaining
    // We chain three iterators together, extract the name from each,
    // and collect into a Vec. This is zero-cost — the compiler optimizes
    // it into a single loop.
    let dep_names: Vec<String> = metadata
        .depends()
        .iter()
        .map(|d| d.name.clone())
        .chain(metadata.imports().iter().map(|d| d.name.clone()))
        .chain(metadata.linking_to().iter().cloned())
        .collect();

    // Resolve each dependency first (recursion!)
    for dep_name in &dep_names {
        // Skip packages not in registry (might be base R packages we missed)
        if registry.get(dep_name).is_some() {
            resolve_recursive(registry, dep_name, visited, resolved)?;
        }
    }

    // NOW add this package (after all deps are resolved)
    resolved.push(ResolvedPackage {
        name: metadata.name().to_string(),
        version: metadata.version().to_string(),
        source: metadata.source_label().to_string(),
        needs_compilation: metadata.needs_compilation(),
        dependencies: dep_names,
        sha256: None, // Computed later during download
    });

    Ok(())
}

/// Find all dependency paths from any root to a specific package.
/// Used by the `rv why <package>` command.
///
/// Example: rv why rlang
/// → DESeq2 → ggplot2 → rlang
/// → DESeq2 → SummarizedExperiment → MatrixGenerics → rlang
pub fn find_dependency_paths(
    registry: &Registry,
    target: &str,
) -> Result<Vec<Vec<String>>> {
    // First, resolve to get only the packages in our tree
    // For now, we read the lockfile to know what's in scope
    let lockfile = crate::lockfile::read("rv.lock");

    let in_scope: Vec<String> = match &lockfile {
        Ok(lf) => lf.packages.iter().map(|p| p.name.clone()).collect(),
        Err(_) => {
            anyhow::bail!(
                "No rv.lock found. Run `rv lock <packages>` first, then use `rv why`."
            );
        }
    };

    let mut paths = Vec::new();

    for pkg_name in &in_scope {
        if let Some(metadata) = registry.get(pkg_name) {
            let all_deps: Vec<&str> = metadata
                .depends()
                .iter()
                .map(|d| d.name.as_str())
                .chain(metadata.imports().iter().map(|d| d.name.as_str()))
                .collect();

            if all_deps.contains(&target) {
                paths.push(vec![pkg_name.clone(), target.to_string()]);
            }
        }
    }

    Ok(paths)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Dependency, PackageMetadata, PackageSource};

    /// Helper: create a simple test registry
    fn test_registry() -> Registry {
        let mut packages = HashMap::new();

        // BiocGenerics (no deps — leaf node)
        packages.insert(
            "BiocGenerics".to_string(),
            vec![PackageMetadata {
                name: "BiocGenerics".to_string(),
                version: "0.48.1".to_string(),
                source: PackageSource::Bioconductor,
                depends: vec![],
                imports: vec![],
                linking_to: vec![],
                needs_compilation: false,
                system_requirements: None,
            }],
        );

        // S4Vectors depends on BiocGenerics
        packages.insert(
            "S4Vectors".to_string(),
            vec![PackageMetadata {
                name: "S4Vectors".to_string(),
                version: "0.40.2".to_string(),
                source: PackageSource::Bioconductor,
                depends: vec![Dependency {
                    name: "BiocGenerics".to_string(),
                    version_req: Some(">= 0.44.0".to_string()),
                }],
                imports: vec![],
                linking_to: vec![],
                needs_compilation: true,
                system_requirements: None,
            }],
        );

        // IRanges depends on S4Vectors
        packages.insert(
            "IRanges".to_string(),
            vec![PackageMetadata {
                name: "IRanges".to_string(),
                version: "2.36.0".to_string(),
                source: PackageSource::Bioconductor,
                depends: vec![Dependency {
                    name: "S4Vectors".to_string(),
                    version_req: Some(">= 0.38.0".to_string()),
                }],
                imports: vec![],
                linking_to: vec![],
                needs_compilation: true,
                system_requirements: None,
            }],
        );

        Registry {
            packages,
            r_version: "4.4.0".to_string(),
            bioc_version: "3.19".to_string(),
        }
    }

    #[test]
    fn test_resolve_order() {
        let registry = test_registry();
        let resolved = resolve(&registry, &["IRanges".to_string()]).unwrap();

        // Should be: BiocGenerics, S4Vectors, IRanges (dependency order)
        let names: Vec<&str> = resolved.packages.iter().map(|p| p.name.as_str()).collect();

        assert_eq!(names, vec!["BiocGenerics", "S4Vectors", "IRanges"]);
    }

    #[test]
    fn test_no_duplicates() {
        let registry = test_registry();
        let resolved = resolve(
            &registry,
            &["IRanges".to_string(), "S4Vectors".to_string()],
        )
        .unwrap();

        // S4Vectors should appear only once even though both IRanges
        // and the user requested it
        let s4_count = resolved
            .packages
            .iter()
            .filter(|p| p.name == "S4Vectors")
            .count();

        assert_eq!(s4_count, 1);
    }
}
