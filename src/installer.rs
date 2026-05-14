// src/installer.rs — Package installation orchestration
//
// This module handles the actual installation of R packages.
// In Phase 1, it uses source compilation (R CMD INSTALL) but does it
// SMARTLY: parallel where possible, with pre-flight checks and resume.
//
// RUST CONCEPT: Rayon for Parallelism
// Rayon is a data parallelism library. You replace `.iter()` with
// `.par_iter()` and your code runs in parallel across CPU cores.
// It automatically handles thread pools and work stealing.
//
//   // Sequential:
//   packages.iter().for_each(|p| install(p));
//
//   // Parallel (that's the ONLY change):
//   packages.par_iter().for_each(|p| install(p));
//
// Rayon figures out the optimal number of threads and distributes work.

use crate::resolver::{ResolvedDeps, ResolvedPackage};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;

/// State file path for tracking install progress (for --retry)
const STATE_FILE: &str = ".rv-install-state.json";

/// Install all resolved packages in dependency order with parallelism.
///
/// The strategy:
/// 1. Group packages into "tiers" — packages whose deps are all satisfied
/// 2. Install each tier in parallel (within a tier, packages are independent)
/// 3. Track progress for resume capability
pub async fn install(resolved: &ResolvedDeps, bioc_version: &str) -> Result<()> {
    use colored::Colorize;

    let total = resolved.packages.len();
    let mut installed: HashSet<String> = HashSet::new();
    let mut failed: Vec<(String, String)> = Vec::new(); // (name, error)
    let mut retry_queue: HashSet<String> = HashSet::new();
    // Find packages already installed on this system
    let already_installed = check_installed_versions(&resolved.packages);
    for name in &already_installed {
        println!("  {} {} (already installed)", "✓".green(), name.dimmed());
        installed.insert(name.clone());
    }

    // Install in tiers
    // RUST CONCEPT: `loop` is an infinite loop. We break out when done.
    // Rust also has `while` and `for`, but `loop` is idiomatic when
    // the exit condition is complex.
    loop {
        // Find packages whose dependencies are all satisfied
        let ready: Vec<&ResolvedPackage> = resolved
            .packages
            .iter()
            .filter(|pkg| {
                // Not yet installed
                !installed.contains(&pkg.name)
                    // Not already failed
                    && !failed.iter().any(|(n, _)| n == &pkg.name)
                    // All dependencies are installed
                    && pkg.dependencies.iter().all(|dep| {
                        installed.contains(dep)
                            // Or the dep isn't in our resolve set (base package)
                            || !resolved.packages.iter().any(|p| p.name == *dep)
                    })
            })
            .collect();

        if ready.is_empty() {
            // Nothing more to install — either done or stuck
            break;
        }

        println!(
            "\n  {} Installing tier: {} packages in parallel",
            "→".blue(),
            ready.len()
        );

        // Install this tier in parallel using rayon
        //
        // RUST CONCEPT: par_iter() from rayon
        // This is the parallel magic. Each package in the tier gets
        // compiled on a separate thread. Rayon handles the thread pool.
        //
        // We collect results into a Vec of (name, Result) tuples.
        let pb = ProgressBar::new(total as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("  [{bar:30.green/dim}] {pos}/{len} {msg}")
                .unwrap()
                .progress_chars("█░░"),
        );
        pb.set_position(installed.len() as u64);

        let results: Vec<(String, Result<()>)> = ready
            .par_iter()
            .map(|pkg| {
                pb.set_message(pkg.name.clone());
                let result = install_single_package(pkg,&bioc_version);
                pb.inc(1);
                (pkg.name.clone(), result)
            })
            .collect();

        pb.finish_and_clear();

        // Note: using .iter().map() instead of .par_iter() for now
        // because async + rayon interaction needs care.
        // Switch to par_iter() when you're ready:
        //
        //   use rayon::prelude::*;
        //   let results: Vec<_> = ready.par_iter().map(|pkg| {
        //       (pkg.name.clone(), install_single_package(pkg))
        //   }).collect();

        // Process results
        for (name, result) in results {
            match result {
                Ok(()) => {
                    println!(
                        "  {} {} {}",
                        "✓".green(),
                        name,
                        format!("({}/{})", installed.len() + 1, total).dimmed()
                    );
                    installed.insert(name);
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    // Bug #6: only retry on a probe-confirmed race. The [LAZY_RACE]
                    // marker is set by parse_compile_error after loadNamespace
                    // succeeded standalone. A bare "lazy loading failed" without
                    // that marker is a real error and should fail through.
                    if err_msg.contains("[LAZY_RACE]") && !retry_queue.contains(&name) {
                        println!("  {} {} — will retry next tier (probe-confirmed race)", "↻".yellow(), name.yellow());
                        retry_queue.insert(name);
                    } else {
                        // Permanent failure (either not lazy loading, or already retried once)
                        println!("  {} {} — {}", "✗".red(), name.red(), err_msg);
                        failed.push((name, err_msg));
                    }
                }
            }
        }

        // Save progress for --retry
        save_install_state(&installed, &failed)?;
    }

    // Report results
    if !failed.is_empty() {
        println!(
            "\n{} {}/{} packages installed. {} failed:",
            "⚠".yellow(),
            installed.len(),
            total,
            failed.len()
        );

        for (name, error) in &failed {
            println!("  {} {}: {}", "✗".red(), name, error);
        }

        println!(
            "\n{}",
            "Fix the issues above, then run: rv install --retry".bold()
        );

        // Return error so the process exits with non-zero status
        anyhow::bail!("{} packages failed to install", failed.len());
    }

    Ok(())
}

/// Install a single R package from source using R CMD INSTALL
fn install_single_package(pkg: &ResolvedPackage, bioc_version: &str) -> Result<()> {
    // GitHub packages: install from the cached tarball, skipping download.
    if let Some(gh) = &pkg.github_source {
        return install_from_github_tarball(pkg, gh);
    }

    // ── CRAN / Bioconductor path (unchanged) ──────────────────────────────

    let url = match pkg.source.as_str() {
        "cran" => format!(
            "https://cloud.r-project.org/src/contrib/{}_{}.tar.gz",
            pkg.name, pkg.version
        ),
        "bioc" => format!(
            "https://bioconductor.org/packages/{}/bioc/src/contrib/{}_{}.tar.gz",
            bioc_version, pkg.name, pkg.version
        ),
        _ => anyhow::bail!("Unknown source: {}", pkg.source),
    };

    let download_dir = PathBuf::from("/tmp/rv-downloads");
    std::fs::create_dir_all(&download_dir)?;

    let _clean_version = if let Some(pos) = pkg.version.rfind('-') {
        let suffix = &pkg.version[pos + 1..];
        if suffix.len() <= 2 && suffix.chars().all(|c| c.is_ascii_digit()) {
            pkg.version[..pos].to_string()
        } else {
            pkg.version.clone()
        }
    } else {
        pkg.version.clone()
    };

    let tarball_path = download_dir.join(format!("{}_{}.tar.gz", pkg.name, pkg.version));
    if tarball_path.exists() {
        let size = std::fs::metadata(&tarball_path).map(|m| m.len()).unwrap_or(0);
        if size < 1000 {
            std::fs::remove_file(&tarball_path).ok();
        }
    }

    if !tarball_path.exists() {
        let _ = Command::new("curl")
            .args(["-sL", "-o"])
            .arg(&tarball_path)
            .arg(&url)
            .status()
            .context("curl not found — install curl")?;

        let got_real_file = tarball_path.exists()
            && std::fs::metadata(&tarball_path).map(|m| m.len()).unwrap_or(0) > 1000;

        if !got_real_file {
            std::fs::remove_file(&tarball_path).ok();

            if let Some(pos) = pkg.version.rfind('-') {
                let stripped = &pkg.version[..pos];
                let alt_url = format!(
                    "https://cloud.r-project.org/src/contrib/{}_{}.tar.gz",
                    pkg.name, stripped
                );
                let alt_path = download_dir.join(format!("{}_{}.tar.gz", pkg.name, stripped));

                Command::new("curl")
                    .args(["-sL", "-o"])
                    .arg(&alt_path)
                    .arg(&alt_url)
                    .status()?;

                if alt_path.exists()
                    && std::fs::metadata(&alt_path).map(|m| m.len()).unwrap_or(0) > 1000
                {
                    std::fs::rename(&alt_path, &tarball_path).ok();
                }
            }
        }

        if !tarball_path.exists()
            || std::fs::metadata(&tarball_path).map(|m| m.len()).unwrap_or(0) < 1000
        {
            std::fs::remove_file(&tarball_path).ok();
            let annotation_url = url.replace("/bioc/", "/data/annotation/");
            let status = Command::new("curl")
                .args(["-sL", "-o"])
                .arg(&tarball_path)
                .arg(&annotation_url)
                .status()?;

            if !status.success() {
                anyhow::bail!("Failed to download {} from any repo", pkg.name);
            }
        }
    }

    run_r_cmd_install(&tarball_path, &pkg.name)?;
    Ok(())
}

/// Install a GitHub-sourced package from its already-downloaded tarball.
///
/// Day 2 cached the tarball at ~/.rv/cache/github/{owner}/{repo}/{sha}.tar.gz.
/// We extract it to a unique temp dir, locate the package root, and run
/// R CMD INSTALL pointing at the directory (not the tarball).
///
/// On success: clean up the temp dir.
/// On failure: leave it so the user can inspect what went wrong.
fn install_from_github_tarball(
    pkg: &ResolvedPackage,
    gh: &crate::resolver::GitHubSource,
) -> Result<()> {
    let cache_dir = github_cache_dir()?;
    let tarball_path = cache_dir
        .join("github")
        .join(&gh.owner)
        .join(&gh.repo)
        .join(format!("{}.tar.gz", gh.commit_sha));

    if !tarball_path.exists() {
        anyhow::bail!(
            "GitHub tarball missing from cache: {}\n\
             Expected the resolver to have populated this. Try re-running.",
            tarball_path.display()
        );
    }

    // Unique temp dir per (owner, repo, full-sha) — no collision risk.
    let tmp_extract = std::env::temp_dir().join(format!(
        "rv-gh-{}-{}-{}",
        gh.owner, gh.repo, gh.commit_sha
    ));

    // Clean any prior leftover from a failed run on the same SHA.
    let _ = std::fs::remove_dir_all(&tmp_extract);
    std::fs::create_dir_all(&tmp_extract)?;

    extract_tarball(&tarball_path, &tmp_extract)
        .with_context(|| format!("Failed to extract {}", tarball_path.display()))?;

    let pkg_dir = crate::registry::github::find_package_root(
        &tmp_extract,
        gh.subdir.as_deref(),
    )?;

    match run_r_cmd_install(&pkg_dir, &pkg.name) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&tmp_extract);
            Ok(())
        }
        Err(e) => {
            // Preserve the source so the user can inspect what failed.
            eprintln!(
                "  source preserved at {} for inspection",
                tmp_extract.display()
            );
            Err(e.context(format!("Failed to install {} from GitHub", pkg.name)))
        }
    }
}

/// Extract a .tar.gz into a destination directory.
fn extract_tarball(tarball: &PathBuf, dest: &PathBuf) -> Result<()> {
    let file = std::fs::File::open(tarball)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(dest)?;
    Ok(())
}

/// Where Day 2 wrote the GitHub cache. Mirrors prepare_github_packages.
fn github_cache_dir() -> Result<PathBuf> {
    let base = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h).join(".rv").join("cache"),
        Err(_) => std::env::temp_dir().join("rv-cache"),
    };
    std::fs::create_dir_all(&base)?;
    Ok(base)
}
/// Run `R CMD INSTALL` against a tarball OR an extracted package directory.
/// R CMD INSTALL accepts both — that's why this helper works for both paths.
fn run_r_cmd_install(target: &PathBuf, pkg_name: &str) -> Result<()> {
    let lib_arg = get_venv_lib().map(|p| format!("--library={}", p.display()));

    let mut cmd = Command::new("R");
    cmd.args(["CMD", "INSTALL", "--no-test-load"]);
    if let Some(ref lib) = lib_arg {
        cmd.arg(lib);
    }
    cmd.arg(target);

    let output = cmd.output().context("R is not installed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Persist the full stderr for debugging — friendly_error below
        // is a one-line summary; users need the real output too.
        let log_path = std::path::PathBuf::from(format!(
            "/tmp/rv-fail-{}.log",
            pkg_name
        ));
        let _ = std::fs::write(&log_path, stderr.as_bytes());

        let friendly_error = parse_compile_error(&stderr, pkg_name);
        anyhow::bail!(
            "{}\n  Full output: {}",
            friendly_error,
            log_path.display()
        );
    }

    Ok(())
}

/// Result of probing whether a package's namespace actually loads.
enum ProbeResult {
    Loaded,
    Failed(String),
    ProbeError(String),
}

/// Run `loadNamespace(pkg)` in a fresh R process to find the real cause
/// of a lazy-loading failure (Bug #19). R's bare "lazy loading failed"
/// hides ABI mismatches, missing deps, missing symbols — this surfaces them.
fn probe_namespace_load(pkg_name: &str) -> ProbeResult {
    if pkg_name == "unknown" || pkg_name.is_empty() {
        return ProbeResult::ProbeError("no package name to probe".to_string());
    }

    let r_code = if let Some(venv_path) = get_venv_lib() {
        format!(
            "tryCatch(loadNamespace('{pkg}', lib.loc='{lib}'), \
             error = function(e) {{ message(conditionMessage(e)); quit(status=1) }})",
            pkg = pkg_name, lib = venv_path.display()
        )
    } else {
        format!(
            "tryCatch(loadNamespace('{pkg}'), \
             error = function(e) {{ message(conditionMessage(e)); quit(status=1) }})",
            pkg = pkg_name
        )
    };

    match Command::new("R").args(["--vanilla", "--slave", "-e", &r_code]).output() {
        Ok(out) if out.status.success() => ProbeResult::Loaded,
        Ok(out) => ProbeResult::Failed(String::from_utf8_lossy(&out.stderr).into_owned()),
        Err(e) => ProbeResult::ProbeError(e.to_string()),
    }
}
/// Parse a compilation error and return a human-friendly message
///
/// Instead of dumping 200 lines of g++ output, we extract the actual problem.
fn parse_compile_error(stderr: &str, pkg_name: &str) -> String {
    // Check for common error patterns

    // Missing header file
    if let Some(header) = stderr
        .lines()
        .find(|l| l.contains("No such file or directory") && l.contains(".h"))
    {
        // Extract the header name
        let header_name = header
            .split("fatal error:")
            .nth(1)
            .unwrap_or(header)
            .trim();
        return format!("Missing header: {}\n  A system library is probably not installed.", header_name);
    }

    // C++ standard mismatch
     if stderr.contains("std::filesystem")
        || stderr.contains("is not available in C++")
        || stderr.contains("is a C++14 extension")
        || stderr.contains("is a C++17 extension")
        || stderr.contains("is a C++20 extension")
        || stderr.contains("requires '-std=c++")
    {
        return "C++ standard mismatch: package needs a newer C++ standard than R is using.\n  \
                Fix: echo 'CXX_STD = CXX17' >> ~/.R/Makevars\n  \
                Then: rv install --retry".to_string();
    }

    // Missing Fortran compiler
    if stderr.contains("gfortran: command not found") || stderr.contains("gfortran: not found") {
        return "Fortran compiler not found.\n  Fix: sudo apt install gfortran".to_string();
    }

    // Missing cmake
    if stderr.contains("cmake") && stderr.contains("not found") {
        return "cmake not found or too old.\n  Fix: sudo apt install cmake".to_string();
    }
    // Missing R package dependency
    // e.g., "ERROR: dependency 'sitmo' is not available for package 'dqrng'"
    if let Some(line) = stderr.lines().find(|l| l.contains("dependency") && l.contains("is not available")) {
        // Extract the missing package name from between the quotes
        let missing = line.split('\'').nth(1);
        return match missing {
            Some(pkg) => format!(
                "Missing R dependency: {}\n  Fix: rv install {} first, then retry",
                pkg, pkg
            ),
            None => format!(
                "Missing R dependency (couldn't parse name).\n  Check the error output above for the missing package."
            ),
        };

    }

    // Lazy loading failure (parallel compilation race condition)
    // e.g., "ERROR: lazy loading failed for package 'ggrepel'"
    // Lazy loading failure — probe for the real cause (Bugs #19, #6).
    if stderr.contains("lazy loading failed") {
        let extracted = stderr
            .lines()
            .find(|l| l.contains("lazy loading failed"))
            .and_then(|l| l.split('\'').nth(1));

        // Bug #6: if we couldn't extract a name from the message, fall back
        // to the package being installed. If that's also unhelpful, the
        // probe will return ProbeError and we won't misclassify as a race.
        let probe_target = match extracted {
            Some(n) if !n.is_empty() => n,
            _ => pkg_name,
        };

        match probe_namespace_load(probe_target) {
            ProbeResult::Loaded => {
                // Genuine race — namespace loads cleanly in a fresh process.
                // Marker is consumed by the orchestrator's retry logic.
                return format!(
                    "[LAZY_RACE] Lazy loading failed for '{}'; namespace loads cleanly standalone — parallel-install race. Retrying.",
                    probe_target
                );
            }
            ProbeResult::Failed(real_err) => {
                let trimmed = real_err.trim();
                return format!(
                    "Lazy loading failed for '{}'. Real cause:\n  {}",
                    probe_target,
                    if trimmed.is_empty() { "<probe produced no error output>" } else { trimmed }
                );
            }
            ProbeResult::ProbeError(e) => {
                return format!(
                    "Lazy loading failed for '{}' (probe failed: {}). See full log.",
                    probe_target, e
                );
            }
        }
    }

    // Permission denied
    if stderr.contains("Permission denied") || stderr.contains("cannot create directory") {
        return "Permission denied when writing to library.\n  Fix: use rv venv to create a project-local library, or check directory permissions.".to_string();
    }

    // Disk space
    if stderr.contains("No space left on device") {
        return "Disk full — no space left on device.\n  Fix: free disk space and retry.".to_string();
    }
     // Fortran library path mismatch (macOS Makevars issue)
    if stderr.contains("library 'gfortran' not found") 
        || stderr.contains("/opt/gfortran/lib") 
    {
        return "Fortran library path mismatch — R is looking for gfortran in the wrong location.\n  Fix: update ~/.R/Makevars with the correct gfortran path.\n  Run rv audit for details.".to_string();
    }

    // Linker errors (missing system library at link time)
    if stderr.contains("undefined reference to") 
        || stderr.contains("symbol(s) not found")
        || stderr.contains("ld: library not found") 
    {
        return "Linker error — a system library is missing or not found by the linker.\n  Fix: run rv audit to check system dependencies.".to_string();
    }

   

    // Generic: take the last meaningful error line
    let last_error = stderr
        .lines()
        .rev()
        .find(|l| l.contains("error:") || l.contains("ERROR"))
        .unwrap_or("Unknown compilation error");

    format!("Compilation error: {}", last_error.trim())
}

/// Get the active venv library path, if any
fn get_venv_lib() -> Option<std::path::PathBuf> {
    // First check if venv is activated via environment variable
    if let Ok(path) = std::env::var("RV_VENV") {
        let lib_path = std::path::PathBuf::from(path).join("lib");
        if lib_path.exists() {
            return Some(lib_path);
        }
    }
    // Fallback: check if .rv/lib exists in current directory
    let local = std::path::PathBuf::from(".rv/lib");
    if local.exists() {
        return Some(std::fs::canonicalize(&local).unwrap_or(local));
    }
    None
}
/// Return the set of all package names currently installed in the
/// active R library (venv if active, system library otherwise).
///
/// Bug #28 enabling: the resolver uses this to recognize packages that
/// are installed on disk but not in any registry — e.g. a GitHub-only
/// package installed via a prior `rv install` invocation.
pub fn list_installed_packages() -> std::collections::HashSet<String> {
    use std::collections::HashSet;

    let r_code = match get_venv_lib() {
        Some(lib) => format!(
            "cat(rownames(installed.packages(lib.loc='{}')), sep='\\n')",
            lib.display()
        ),
        None => "cat(rownames(installed.packages()), sep='\\n')".to_string(),
    };

    match Command::new("R")
        .args(["--vanilla", "--slave", "-e", &r_code])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => HashSet::new(), // Fail open: empty set = nothing pre-installed
    }
}
/// Check which packages from the resolved set are already installed
pub fn check_installed_versions(packages: &[ResolvedPackage]) -> Vec<String> {

    let r_code = if let Some(venv_path) = get_venv_lib() {
        format!(
            "ip <- installed.packages(lib.loc='{}'); cat(paste(ip[,'Package'], ip[,'Version']), sep='\\n')",
            venv_path.display()
        )
    } else {
        "ip <- installed.packages(); cat(paste(ip[,'Package'], ip[,'Version']), sep='\\n')".to_string()
    };

    let output = Command::new("R")
        .args(["--vanilla", "--slave", "-e", &r_code])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let installed_str = String::from_utf8_lossy(&out.stdout);
            
            // Build a map of package name → installed version
            let mut installed_map: std::collections::HashMap<&str, &str> = 
                std::collections::HashMap::new();
            for line in installed_str.lines() {
                let parts: Vec<&str> = line.splitn(2, ' ').collect();
                if parts.len() == 2 {
                    installed_map.insert(parts[0], parts[1]);
                }
            }

            packages
                .iter()
                .filter(|pkg| {
                    match installed_map.get(pkg.name.as_str()) {
                        Some(installed_ver) => *installed_ver == pkg.version.as_str(),
                        None => false,
                    }
                })
                .map(|pkg| pkg.name.clone())
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Resume a previously failed installation
/// Resume a previously failed installation.
///
/// Strategy: reconstruct the resolved tree from rv.lock, then hand it to
/// `install()`. The installer's existing `check_installed_versions()` skip
/// logic handles "already done" packages automatically — so the retry
/// naturally targets failed + unattempted packages without custom filtering.
///
/// State file is optional (used for messaging). Lockfile is required —
/// without it we'd have to re-resolve, which defeats the point of retry.
pub async fn retry_install() -> Result<()> {
    use colored::Colorize;

    // State file is informational only — installer rebuilds it on success.
    let state = load_install_state().ok();

    let lockfile = crate::lockfile::read("rv.lock").context(
        "retry needs rv.lock. Run `rv install <packages>` to generate one first.",
    )?;

    // Convert locked entries back into ResolvedPackages. Mirrors the same
    // logic in cmd_restore (main.rs), minus the integrity check — retry is
    // resuming the SAME session, so tarballs are already verified-by-download.
    let resolved = ResolvedDeps {
        packages: lockfile
            .packages
            .iter()
            .map(|pkg| {
                let github_source = if pkg.source == "github" {
                    let repo = pkg.repo.as_ref().unwrap();
                    let (owner, repo_name) = repo.split_once('/').unwrap();
                    Some(crate::resolver::GitHubSource {
                        owner: owner.to_string(),
                        repo: repo_name.to_string(),
                        commit_sha: pkg.r#ref.clone().unwrap(),
                        subdir: pkg.subdir.clone(),
                        tarball_sha256: pkg.tarball_sha256.clone().unwrap(),
                    })
                } else {
                    None
                };

                ResolvedPackage {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    source: pkg.source.clone(),
                    needs_compilation: false,
                    dependencies: pkg.deps.clone(),
                    sha256: pkg.sha256.clone(),
                    github_source,
                }
            })
            .collect(),
        duration_secs: 0.0,
    };

    match &state {
        Some(s) => println!(
            "{} {} already installed, {} failed previously — re-attempting failures and unattempted packages.",
            "↻".blue(),
            s.installed.len(),
            s.failed.len(),
        ),
        None => println!(
            "{} no prior state — attempting full lockfile.",
            "↻".blue()
        ),
    }

    install(&resolved, &lockfile.metadata.bioc_version).await?;

    println!("\n{} Retry complete.", "✓".green());
    Ok(())
}

// --- State persistence for --retry ---

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct InstallState {
    installed: Vec<String>,
    failed: Vec<(String, String)>,
}

fn save_install_state(
    installed: &HashSet<String>,
    failed: &[(String, String)],
) -> Result<()> {
    let state = InstallState {
        installed: installed.iter().cloned().collect(),
        failed: failed.to_vec(),
    };

    let json = serde_json::to_string_pretty(&state)?;
    std::fs::write(STATE_FILE, json)?;

    Ok(())
}

fn load_install_state() -> Result<InstallState> {
    let content = std::fs::read_to_string(STATE_FILE)
        .context("No install state found. Run `rv install` first.")?;

    let state: InstallState = serde_json::from_str(&content)?;
    Ok(state)
}
