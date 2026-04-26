// src/sat_resolver.rs — Version-constraint-aware dependency resolver
//
// This replaces the naive "grab latest and hope" resolver with one
// that actually checks version constraints.
//
// ALGORITHM: Greedy with constraint validation
// 1. For each package, pick the best available version (first in list)
// 2. Collect all version constraints from Depends/Imports/LinkingTo
// 3. After resolving the full tree, validate ALL constraints
// 4. Report conflicts clearly if any constraint fails
//
// FUTURE: Add backtracking — when a constraint fails, try an older
// version from CRAN Archive and re-resolve the affected subtree.

use crate::registry::Registry;
use crate::resolver::{ResolvedDeps, ResolvedPackage};
use crate::version::{RVersion, VersionConstraint};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use colored::Colorize;

/// A constraint on a package, tracked with who imposed it
#[derive(Debug, Clone)]
struct Constraint {
    /// Package being constrained (e.g., "rlang")
    target: String,
    /// The constraint (e.g., ">= 1.1.0")
    constraint: VersionConstraint,
    /// Who requires this (e.g., "ggplot2")
    required_by: String,
}

/// Resolve dependencies with version constraint checking.
pub async fn resolve_with_constraints(
    registry: &mut Registry,
    requested: &[String],
) -> Result<ResolvedDeps> {
    let start = Instant::now();

    // Phase 1: Walk the dependency tree, collecting packages and constraints
    let mut visited: HashSet<String> = HashSet::new();
    let mut resolved: Vec<ResolvedPackage> = Vec::new();
    let mut constraints: Vec<Constraint> = Vec::new();

    for pkg_name in requested {
        collect_with_constraints(
            registry,
            pkg_name,
            &mut visited,
            &mut resolved,
            &mut constraints,
        )?;
    }

    // Phase 2: Check R version compatibility
    check_r_version(registry, &resolved)?;

    // Phase 3: Validate all constraints against resolved versions
    let conflicts = validate_constraints(registry, &constraints);

   if !conflicts.is_empty() {
        // Phase 4: Try archive fallback for conflicting packages
        let mut retried = false;

        for (pkg_name, pkg_conflicts) in &conflicts {
            // Check if any constraint is unsatisfied
            let has_unsatisfied = pkg_conflicts.iter().any(|(_, _, satisfied)| !satisfied);
            if !has_unsatisfied {
                continue;
            }

            // Find the highest version constraint we need to satisfy
            let mut max_required: Option<crate::version::RVersion> = None;
            for (_, constraint_str, satisfied) in pkg_conflicts {
                if !satisfied {
                    if let Some(c) = VersionConstraint::parse(constraint_str) {
                        let v = &c.version;
                        if max_required.as_ref().map_or(true, |m| v > m) {
                            max_required = Some(v.clone());
                        }
                    }
                }
            }

            // Fetch archive versions
            println!(
                "  {} Constraint conflict on {} — checking CRAN Archive...",
                "↻".yellow(),
                pkg_name
            );

            let archive_versions = registry.fetch_archive_versions(pkg_name).await?;

            if archive_versions.is_empty() {
                continue;
            }

            // Find the newest archive version that satisfies all constraints
            let all_constraints_for_pkg: Vec<&Constraint> = constraints
                .iter()
                .filter(|c| c.target == *pkg_name)
                .collect();

            for candidate in &archive_versions {
                let candidate_version = match crate::version::RVersion::parse(&candidate.version) {
                    Some(v) => v,
                    None => continue,
                };

                let satisfies_all = all_constraints_for_pkg.iter().all(|c| {
                    c.constraint.satisfies(&candidate_version)
                });

                if satisfies_all {
                    println!(
                        "  {} Found compatible version: {} {}",
                        "✓".green(),
                        pkg_name,
                        candidate.version
                    );

                    // Put this version first in the registry
                    if let Some(versions) = registry.packages.get_mut(pkg_name) {
                        versions.insert(0, candidate.clone());
                    }

                    retried = true;
                    break;
                }
            }
        }

        if retried {
            // Re-resolve with the updated registry
            let mut visited2: HashSet<String> = HashSet::new();
            let mut resolved2: Vec<ResolvedPackage> = Vec::new();
            let mut constraints2: Vec<Constraint> = Vec::new();

            for pkg_name in requested {
                collect_with_constraints(
                    registry,
                    pkg_name,
                    &mut visited2,
                    &mut resolved2,
                    &mut constraints2,
                )?;
            }

            // Re-validate
            let conflicts2 = validate_constraints(registry, &constraints2);

            if conflicts2.is_empty() {
                let duration = start.elapsed();
                return Ok(ResolvedDeps {
                    packages: resolved2,
                    duration_secs: duration.as_secs_f64(),
                });
            }
        }

        // Still have conflicts — report them
        use colored::Colorize;
        let mut msg = format!("{}\n", "Version conflicts detected:".red().bold());

        for (pkg_name, pkg_conflicts) in &conflicts {
            let available = registry
                .get(pkg_name)
                .map(|p| p.version())
                .unwrap_or("unknown");

            msg.push_str(&format!(
                "\n  {} {} (available: {})\n",
                "✗".red(),
                pkg_name.bold(),
                available
            ));

            for (required_by, constraint_str, satisfied) in pkg_conflicts {
                if *satisfied {
                    msg.push_str(&format!(
                        "    {} {} needs {} {}\n",
                        "✓".green(),
                        required_by,
                        pkg_name,
                        constraint_str
                    ));
                } else {
                    msg.push_str(&format!(
                        "    {} {} needs {} {} — NOT SATISFIED\n",
                        "✗".red(),
                        required_by,
                        pkg_name,
                        constraint_str
                    ));
                }
            }
        }

        msg.push_str(&format!(
            "\n{}",
            "No compatible version found, even in CRAN Archive.".dimmed()
        ));

        anyhow::bail!("{}", msg);
    }

    let duration = start.elapsed();

    Ok(ResolvedDeps {
        packages: resolved,
        duration_secs: duration.as_secs_f64(),
    })
}

/// Recursively collect packages and their version constraints
fn collect_with_constraints(
    registry: &Registry,
    pkg_name: &str,
    visited: &mut HashSet<String>,
    resolved: &mut Vec<ResolvedPackage>,
    constraints: &mut Vec<Constraint>,
) -> Result<()> {
    if visited.contains(pkg_name) {
        return Ok(());
    }
    visited.insert(pkg_name.to_string());

    let metadata = registry.get(pkg_name).with_context(|| {
        format!(
            "Package '{}' not found in CRAN or Bioconductor. \
             Check the spelling or ensure you have the right Bioconductor version.",
            pkg_name
        )
    })?;

    let mut dep_names: Vec<String> = Vec::new();

    // Process Depends — collect names AND constraints
    for dep in metadata.depends() {
         if dep.name == "R" {
            // Don't add R as a package to install, but DO check the constraint
            if let Some(ref ver_req) = dep.version_req {
                if let Some(constraint) = VersionConstraint::parse(ver_req) {
                    let r_version = RVersion::parse(&registry.r_version);
                    if let Some(ref rv) = r_version {
                        if !constraint.satisfies(rv) {
                            anyhow::bail!(
                                "Package '{}' requires R {}, but you have R {}",
                                pkg_name, ver_req, registry.r_version
                            );
                        }
                    }
                }
            }
            continue; // Don't add R to dep_names
        }
        dep_names.push(dep.name.clone());
        if let Some(ref ver_req) = dep.version_req {
            if let Some(constraint) = VersionConstraint::parse(ver_req) {
                constraints.push(Constraint {
                    target: dep.name.clone(),
                    constraint,
                    required_by: pkg_name.to_string(),
                });
            }
        }
    }

    // Process Imports
    for dep in metadata.imports() {
        dep_names.push(dep.name.clone());
        if let Some(ref ver_req) = dep.version_req {
            if let Some(constraint) = VersionConstraint::parse(ver_req) {
                constraints.push(Constraint {
                    target: dep.name.clone(),
                    constraint,
                    required_by: pkg_name.to_string(),
                });
            }
        }
    }

    // Process LinkingTo
    for lt in metadata.linking_to() {
        dep_names.push(lt.clone());
    }

    // Deduplicate dependency names
    dep_names.sort();
    dep_names.dedup();

    // Recurse into dependencies
    for dep_name in &dep_names {
        if registry.get(dep_name).is_some() {
            collect_with_constraints(registry, dep_name, visited, resolved, constraints)?;
        }
    }

    // Add this package after its deps (topological order)
    resolved.push(ResolvedPackage {
        name: metadata.name().to_string(),
        version: metadata.version().to_string(),
        source: metadata.source_label().to_string(),
        needs_compilation: metadata.needs_compilation(),
        dependencies: dep_names,
        sha256: None,
    });

    Ok(())
}

/// Check that the user's R version satisfies all package requirements
fn check_r_version(registry: &Registry, resolved: &[ResolvedPackage]) -> Result<()> {
    let r_version = match RVersion::parse(&registry.r_version) {
        Some(v) => v,
        None => return Ok(()), // Can't parse R version, skip check
    };

    let mut r_conflicts: Vec<(String, String)> = Vec::new();

    for pkg in resolved {
        if let Some(metadata) = registry.get(&pkg.name) {
            // Check if any Depends has an R version constraint
            for dep in metadata.depends() {
                if dep.name == "R" {
                    // This was filtered out by our parser, but just in case
                    if let Some(ref ver_req) = dep.version_req {
                        if let Some(constraint) = VersionConstraint::parse(ver_req) {
                            if !constraint.satisfies(&r_version) {
                                r_conflicts.push((
                                    pkg.name.clone(),
                                    format!("needs R {}, you have R {}", ver_req, r_version),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    if !r_conflicts.is_empty() {
        use colored::Colorize;
        let mut msg = format!(
            "{} Your R version ({}) is incompatible with:\n",
            "✗".red(),
            registry.r_version
        );
        for (pkg, reason) in &r_conflicts {
            msg.push_str(&format!("  {} — {}\n", pkg.red(), reason));
        }
        msg.push_str("\nUpgrade R or use `rv use R@<version>` (coming soon)");
        anyhow::bail!("{}", msg);
    }

    Ok(())
}

/// Validate all collected constraints against the resolved versions
/// Returns a map of package_name → list of (required_by, constraint_string, satisfied)
fn validate_constraints(
    registry: &Registry,
    constraints: &[Constraint],
) -> HashMap<String, Vec<(String, String, bool)>> {
    // Group constraints by target
    let mut by_target: HashMap<&str, Vec<&Constraint>> = HashMap::new();
    for c in constraints {
        by_target.entry(c.target.as_str()).or_default().push(c);
    }

    let mut conflicts: HashMap<String, Vec<(String, String, bool)>> = HashMap::new();

    for (target_name, target_constraints) in &by_target {
        let available = match registry.get(target_name) {
            Some(pkg) => match RVersion::parse(pkg.version()) {
                Some(v) => v,
                None => continue,
            },
            None => continue,
        };

        let mut has_conflict = false;
        let mut details = Vec::new();

        for c in target_constraints {
            let satisfied = c.constraint.satisfies(&available);
            if !satisfied {
                has_conflict = true;
            }
            details.push((
                c.required_by.clone(),
                format!("{}", c.constraint),
                satisfied,
            ));
        }

        if has_conflict {
            conflicts.insert(target_name.to_string(), details);
        }
    }

    conflicts
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Dependency, PackageMetadata, PackageSource};

    fn conflict_registry() -> Registry {
        let mut packages: HashMap<String, Vec<PackageMetadata>> = HashMap::new();

        // rlang 1.0.0 — deliberately OLD version
        packages.insert("rlang".to_string(), vec![PackageMetadata {
            name: "rlang".to_string(),
            version: "1.0.0".to_string(),
            source: PackageSource::Cran,
            depends: vec![],
            imports: vec![],
            linking_to: vec![],
            needs_compilation: false,
            system_requirements: None,
        }]);

        // ggplot2 — needs rlang >= 1.1.0 (conflict! rlang is only 1.0.0)
        packages.insert("ggplot2".to_string(), vec![PackageMetadata {
            name: "ggplot2".to_string(),
            version: "4.0.0".to_string(),
            source: PackageSource::Cran,
            depends: vec![],
            imports: vec![Dependency {
                name: "rlang".to_string(),
                version_req: Some(">= 1.1.0".to_string()),
            }],
            linking_to: vec![],
            needs_compilation: false,
            system_requirements: None,
        }]);

        // dplyr — also needs rlang >= 1.2.0 (also conflicts)
        packages.insert("dplyr".to_string(), vec![PackageMetadata {
            name: "dplyr".to_string(),
            version: "1.1.0".to_string(),
            source: PackageSource::Cran,
            depends: vec![],
            imports: vec![Dependency {
                name: "rlang".to_string(),
                version_req: Some(">= 1.2.0".to_string()),
            }],
            linking_to: vec![],
            needs_compilation: false,
            system_requirements: None,
        }]);

        // mypackage — depends on both ggplot2 and dplyr
        packages.insert("mypackage".to_string(), vec![PackageMetadata {
            name: "mypackage".to_string(),
            version: "0.1.0".to_string(),
            source: PackageSource::Cran,
            depends: vec![],
            imports: vec![
                Dependency { name: "ggplot2".to_string(), version_req: None },
                Dependency { name: "dplyr".to_string(), version_req: None },
            ],
            linking_to: vec![],
            needs_compilation: false,
            system_requirements: None,
        }]);

        Registry {
            packages,
            r_version: "4.4.0".to_string(),
            bioc_version: "3.19".to_string(),
        }
    }

    #[tokio::test]
    async fn test_detects_version_conflict() {
        let mut registry = conflict_registry();
        let result = resolve_with_constraints(
            &mut registry,
            &["mypackage".to_string()],
        ).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("rlang"));
        assert!(err.contains("ggplot2"));
        assert!(err.contains("dplyr"));
    }

    #[tokio::test]
    async fn test_passes_when_constraints_satisfied() {
        let mut packages: HashMap<String, Vec<PackageMetadata>> = HashMap::new();

        // rlang 1.5.0 — satisfies all constraints
        packages.insert("rlang".to_string(), vec![PackageMetadata {
            name: "rlang".to_string(),
            version: "1.5.0".to_string(),
            source: PackageSource::Cran,
            depends: vec![],
            imports: vec![],
            linking_to: vec![],
            needs_compilation: false,
            system_requirements: None,
        }]);

        // ggplot2 needs rlang >= 1.1.0 (satisfied by 1.5.0)
        packages.insert("ggplot2".to_string(), vec![PackageMetadata {
            name: "ggplot2".to_string(),
            version: "4.0.0".to_string(),
            source: PackageSource::Cran,
            depends: vec![],
            imports: vec![Dependency {
                name: "rlang".to_string(),
                version_req: Some(">= 1.1.0".to_string()),
            }],
            linking_to: vec![],
            needs_compilation: false,
            system_requirements: None,
        }]);

        let mut registry = Registry {
            packages,
            r_version: "4.4.0".to_string(),
            bioc_version: "3.19".to_string(),
        };

        let result = resolve_with_constraints(
            &mut registry,
            &["ggplot2".to_string()],
        ).await;

        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.packages.len(), 2); // rlang + ggplot2
    }
}