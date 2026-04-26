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
                    if err_msg.contains("lazy loading failed") && !retry_queue.contains(&name) {
                        // First failure with lazy loading — retry in next tier
                        println!("  {} {} — will retry next tier (lazy loading race)", "↻".yellow(), name.yellow());
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
    // Construct the download URL
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

    // Download the tarball
    // For MVP, use curl command. Production version would use reqwest.
    let download_dir = PathBuf::from("/tmp/rv-downloads");
    std::fs::create_dir_all(&download_dir)?;

    // Use the clean version (without -N suffix) for both URL and filename
    let clean_version = if let Some(pos) = pkg.version.rfind('-') {
        let suffix = &pkg.version[pos+1..];
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
        let size = std::fs::metadata(&tarball_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if size < 1000 {
            std::fs::remove_file(&tarball_path).ok();
        }
    }

    if !tarball_path.exists() {
        // Try original version first
        let status = Command::new("curl")
            .args(["-sL", "-o"])
            .arg(&tarball_path)
            .arg(&url)
            .status()
            .context("curl not found — install curl")?;

        // Check if we got a real file or an error page
        let got_real_file = tarball_path.exists() 
            && std::fs::metadata(&tarball_path).map(|m| m.len()).unwrap_or(0) > 1000;

        // If failed and version has a dash, try without it (CRAN quirk)
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

                if alt_path.exists() && std::fs::metadata(&alt_path).map(|m| m.len()).unwrap_or(0) > 1000 {
                    std::fs::rename(&alt_path, &tarball_path).ok();
                }
            }
        }

        // If still no good file, try annotation repo (Bioconductor)
        if !tarball_path.exists() || std::fs::metadata(&tarball_path).map(|m| m.len()).unwrap_or(0) < 1000 {
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


    // Install using R CMD INSTALL
    // Use project-local library if .rv/lib/ exists
     let lib_arg = get_venv_lib().map(|p| format!("--library={}", p.display()));

    let mut cmd = Command::new("R");
    cmd.args(["CMD", "INSTALL", "--no-test-load"]);
    if let Some(ref lib) = lib_arg {
        cmd.arg(lib);
    }
    cmd.arg(&tarball_path);

    let output = cmd.output().context("R is not installed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Parse the error for a human-friendly message
        let friendly_error = parse_compile_error(&stderr);

        anyhow::bail!("{}", friendly_error);
    }

    Ok(())
}

/// Parse a compilation error and return a human-friendly message
///
/// Instead of dumping 200 lines of g++ output, we extract the actual problem.
fn parse_compile_error(stderr: &str) -> String {
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
    if stderr.contains("std::filesystem") || stderr.contains("is not available in C++") {
        return "C++ standard mismatch: package needs C++17 but R is configured for an older standard.\n  \
                Fix: echo 'CXX_STD = CXX17' >> ~/.R/Makevars".to_string();
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
    if stderr.contains("lazy loading failed") {
        let pkg_name = stderr.lines()
            .find(|l| l.contains("lazy loading failed"))
            .and_then(|l| l.split('\'').nth(1))
            .unwrap_or("unknown");
        return format!(
            "Lazy loading failed for '{}' — a dependency likely hasn't finished registering.\n  This is a timing issue, not a real error. Re-run rv install to retry.",
            pkg_name
        );
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
pub async fn retry_install() -> Result<()> {
    use colored::Colorize;

    let state = load_install_state()?;

    println!(
        "{} {} packages already installed, retrying remaining...",
        "✓".green(),
        state.installed.len()
    );

    // Re-resolve and install only what's missing
    // TODO: In a full implementation, re-read the lockfile and
    // install only packages not in state.installed

    println!("{}", "Retry not fully implemented yet — re-run rv install".yellow());

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
