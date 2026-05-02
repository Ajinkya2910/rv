// src/main.rs — The entry point of the rv program.
//
// RUST CONCEPT: main() is where every Rust program starts, just like Python's
// `if __name__ == "__main__"` or C's main(). The `#[tokio::main]` attribute
// makes it async (we need this for HTTP requests to CRAN/Bioconductor).
//
// RUST CONCEPT: `mod` declarations tell Rust "there's a module here."
// Each `mod foo;` means Rust looks for either:
//   - src/foo.rs (single file module), or
//   - src/foo/mod.rs (directory module with sub-modules)
// This is like Python's import system but more explicit.

// Declare our modules — each is a major component of rv
mod cli;        // Command-line argument parsing
mod registry;   // Fetching package metadata from CRAN + Bioconductor
mod resolver;   // Dependency resolution (the brain of rv)
mod sysreq;     // System dependency checking (apt packages)
mod lockfile;   // rv.lock file generation and reading
mod installer;  // Package installation orchestration
mod version;
mod sat_resolver;
mod source;
// `use` brings items into scope — like `from X import Y` in Python
use anyhow::{Context, Result};
use cli::Cli;
use clap::Parser;

// #[tokio::main] transforms this into an async main function.
// Under the hood, it creates a tokio runtime and blocks on this function.
// Without it, you'd have to write:
//   fn main() {
//       let rt = tokio::runtime::Runtime::new().unwrap();
//       rt.block_on(async { ... });
//   }
#[tokio::main]
async fn main() -> Result<()> {
    // Parse command-line arguments.
    // Cli::parse() uses clap to read args from std::env::args().
    // If the user types something invalid, clap prints help and exits.
    let cli = Cli::parse();

    // Match on the subcommand — like a switch statement, but Rust's `match`
    // is exhaustive: the compiler forces you to handle every possible case.
    // This means you can never forget to handle a command.
    match cli.command {
        cli::Commands::Resolve { packages } => {
            // `rv resolve DESeq2 ggplot2`
            cmd_resolve(&packages).await?;
        }
        cli::Commands::Audit { packages } => {
            // `rv audit DESeq2`
            cmd_audit(&packages).await?;
        }
        cli::Commands::Install { packages, retry } => {
            // `rv install DESeq2` or `rv install --retry`
            cmd_install(&packages, retry).await?;
        }
        cli::Commands::Why { package } => {
            // `rv why rlang`
            cmd_why(&package).await?;
        }
        cli::Commands::Lock { packages } => {
            // `rv lock DESeq2 clusterProfiler`
            cmd_lock(&packages).await?;
        }
        cli::Commands::Restore => {
            cmd_restore().await?;
        }
         cli::Commands::Venv { path, r_version } => {
            cmd_venv_create(&path, r_version).await?;
        }
        cli::Commands::VenvInfo => {
            cmd_venv_info()?;
        }
        cli::Commands::VenvRemove { path } => {
            cmd_venv_remove(&path)?;
        }
    }

    // Ok(()) means "everything succeeded, return nothing."
    // RUST CONCEPT: Rust has no exceptions. Instead, functions return
    // Result<T, E> which is either Ok(value) or Err(error).
    // The `?` operator after function calls means "if this returned an error,
    // propagate it up immediately." It's like automatic try/except.
    Ok(())
}

// --- Command Implementations ---
// Each command follows the same pattern:
// 1. Fetch registry metadata
// 2. Do something with it
// 3. Display results

/// Resolve and display the dependency tree
async fn cmd_resolve(packages: &[String]) -> Result<()> {
    use colored::Colorize;

   let parsed: Vec<source::PackageSource> = packages
        .iter()
        .map(|s| source::PackageSource::parse(s))
        .collect::<Result<Vec<_>>>()?;

    println!("{}", "Resolving dependencies...".dimmed());
    let mut registry = registry::Registry::fetch().await?;
    let root_names = sat_resolver::prepare_github_packages(&mut registry, &parsed).await?;
    let resolved = sat_resolver::resolve_with_constraints(&mut registry, &root_names).await?;

    // Display the tree
    println!(
        "\n{} {} packages resolved in {:.1}s\n",
        "✓".green(),
        resolved.packages.len(),
        resolved.duration_secs
    );

    // Print the dependency tree
    for pkg in &resolved.packages {
        let source_label = match pkg.source.as_str() {
            "bioc" => "(bioc)".blue(),
            "cran" => "(cran)".dimmed(),
            "github" => "(github)".magenta(),
            _ => "(unknown)".dimmed(),
        };

        let compile_flag = if pkg.needs_compilation {
            " ⚙ C++".yellow().to_string()
        } else {
            String::new()
        };

        // RUST CONCEPT: format! is like Python's f-strings.
        // println! is a macro (note the !) that prints to stdout.
        println!("  {} {} {}{}", pkg.name, pkg.version, source_label, compile_flag);
    }

    // Print summary
    let bioc_count = resolved.packages.iter().filter(|p| p.source == "bioc").count();
    let cran_count = resolved.packages.iter().filter(|p| p.source == "cran").count();
    let compile_count = resolved.packages.iter().filter(|p| p.needs_compilation).count();

    println!("\n{}", "Summary:".bold());
    println!("  {} from Bioconductor", bioc_count.to_string().blue());
    println!("  {} from CRAN", cran_count);
    println!(
        "  {} need compilation",
        compile_count.to_string().yellow()
    );

    Ok(())
}

/// Audit system dependencies before installing
async fn cmd_audit(packages: &[String]) -> Result<()> {
    use colored::Colorize;

    let parsed: Vec<source::PackageSource> = packages
        .iter()
        .map(|s| source::PackageSource::parse(s))
        .collect::<Result<Vec<_>>>()?;

    println!("{}", "Resolving dependencies...".dimmed());
    let mut registry = registry::Registry::fetch().await?;
    let root_names = sat_resolver::prepare_github_packages(&mut registry, &parsed).await?;
    let resolved = sat_resolver::resolve_with_constraints(&mut registry, &root_names).await?;

    println!("{}", "Checking system dependencies...".dimmed());
    let report = sysreq::audit(&resolved)?;
    
    // Display results
    for dep in &report.found {
        println!("  {} {} {}", "✓".green(), dep.name, dep.version.dimmed());
    }
    for dep in &report.missing {
        println!(
            "  {} {} — needed by: {}",
            "✗".red(),
            dep.name.red(),
            dep.needed_by.join(", ").dimmed()
        );
    }
    
    if !report.missing.is_empty() {
   if std::env::consts::OS == "macos" {
        println!("\n{}\n  brew install {}", "Fix with:".bold(),
            report.missing.iter()
                .map(|d| sysreq::get_brew_name(&d.name))
                .collect::<Vec<_>>()
                .join(" ")
        );
    } else {
        println!("\n{}\n  sudo apt install {}", "Fix with:".bold(),
            report.missing.iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
}
// Offer to fix Makevars if needed
    if let Some(fix) = sysreq::check_makevars() {
        println!(
            "\n{} R's Fortran paths are misconfigured:",
            "⚠".yellow()
        );
        println!("  R looks in:      {}", fix.bad_paths.join(", "));
        println!("  gfortran is at:  {}", fix.correct_lib);
        println!(
            "\n  {} rv can fix this by writing to {}",
            "→".blue(),
            fix.makevars_path.display()
        );
        // Auto-fix for now; later you can add --fix flag
        sysreq::fix_makevars(&fix)?;
        println!("  {} Makevars updated!", "✓".green());
    }

    Ok(())
}

/// Install packages
async fn cmd_install(packages: &[String], retry: bool) -> Result<()> {
    use colored::Colorize;

    // Day 1: parse package specs (registry name vs. gh:user/repo). 
    // Resolver wiring comes later — for now we just verify the parser
    // and bail early if the user asked for a GitHub package.
    if retry {
        println!("{}", "Retrying failed packages...".dimmed());
        installer::retry_install().await?;
        return Ok(());
    }

    // Parse package specs — registry names pass through, GitHub specs
    // get their metadata fetched and inserted into the registry below.
    let parsed: Vec<source::PackageSource> = packages
        .iter()
        .map(|s| source::PackageSource::parse(s))
        .collect::<Result<Vec<_>>>()?;
    // Warn if installing GitHub packages outside a venv.
    if !retry {
        let has_github = parsed
            .iter()
            .any(|p| matches!(p, source::PackageSource::GitHub(_)));
        let venv_active = std::env::var("RV_VENV").is_ok()
            || std::path::Path::new(".rv/lib").exists();
        if has_github && !venv_active {
            println!(
                "\n{} installing GitHub package outside a virtual environment.",
                "warning:".yellow().bold()
            );
            println!(
                "  GitHub packages may shadow CRAN versions in your system library."
            );
            println!("  Consider running `rv venv` first.\n");
        }
    }

    println!("{}", "Resolving dependencies...".dimmed());
    let mut registry = registry::Registry::fetch().await?;

    // For each gh:... spec: fetch metadata and register in the GitHub bucket.
    // Recursively follow `Remotes:` entries that point at other GitHub repos.
    let root_names = sat_resolver::prepare_github_packages(&mut registry, &parsed).await?;

    let resolved = sat_resolver::resolve_with_constraints(&mut registry, &root_names).await?;

    // Pre-flight system dependency check
    println!("{}", "Pre-flight check: system dependencies...".dimmed());
    let report = sysreq::audit(&resolved)?;

    if !report.missing.is_empty() {
        println!(
            "\n{} Missing {} system libraries:",
            "✗".red(),
            report.missing.len()
        );
        for dep in &report.missing {
            println!("  {} — needed by: {}", dep.name.red(), dep.needed_by.join(", "));
        }

        // Ask user if we should install them
        println!("\n{}", "Install them now? [Y/n]".bold());
        // In a real implementation, read stdin here
        // For now, we'll auto-install
        sysreq::install_missing(&report)?;
    }

    println!("\n{}", "Installing packages...".dimmed());
    installer::install(&resolved,&registry.bioc_version).await?;

    println!(
        "\n{} All {} packages installed successfully!",
        "✓".green(),
        resolved.packages.len()
    );

    Ok(())
}

/// Explain why a package is in the dependency tree
async fn cmd_why(package: &str) -> Result<()> {
    use colored::Colorize;

    let mut registry = registry::Registry::fetch().await?;

    // Find all paths from root packages to the target
    let paths = resolver::find_dependency_paths(&registry, package)?;

    if paths.is_empty() {
        println!("{} is not in the current dependency tree.", package.yellow());
    } else {
        println!("Why is {} needed:\n", package.bold());
        for path in &paths {
            for (i, step) in path.iter().enumerate() {
                let indent = "  ".repeat(i);
                let arrow = if i > 0 { "└── " } else { "" };
                println!("{}{}{}", indent, arrow, step);
            }
            println!();
        }
    }

    Ok(())
}

/// Generate a lockfile
async fn cmd_lock(packages: &[String]) -> Result<()> {
    use colored::Colorize;

    let parsed: Vec<source::PackageSource> = packages
        .iter()
        .map(|s| source::PackageSource::parse(s))
        .collect::<Result<Vec<_>>>()?;

    println!("{}", "Resolving dependencies...".dimmed());
    let mut registry = registry::Registry::fetch().await?;
    let root_names = sat_resolver::prepare_github_packages(&mut registry, &parsed).await?;
    let resolved = sat_resolver::resolve_with_constraints(&mut registry, &root_names).await?;

    let lockfile_path = lockfile::write(&resolved)?;

    println!(
        "\n{} Written to {} ({} packages)",
        "✓".green(),
        lockfile_path.display(),
        resolved.packages.len()
    );

    Ok(())
}
/// Restore packages from rv.lock
async fn cmd_restore() -> Result<()> {
    use colored::Colorize;

    // Read the lockfile
    println!("{}", "Reading rv.lock...".dimmed());
    let lockfile = lockfile::read("rv.lock")?;

    println!(
        "  Found {} packages for R {} / Bioconductor {}",
        lockfile.packages.len(),
        lockfile.metadata.r_version,
        lockfile.metadata.bioc_version
    );

    // ── Verify integrity of every GitHub-sourced lockfile entry ──────────
    // Re-download each pinned tarball and check SHA-256 against the lockfile.
    // Mismatch → bail. This catches lockfile tampering and upstream rewrites
    // before any install activity.
    let github_count = lockfile.packages.iter().filter(|p| p.source == "github").count();

    if github_count > 0 {
        println!("{}", "Verifying GitHub package integrity...".dimmed());

        let cache_dir = match std::env::var("HOME") {
            Ok(h) => std::path::PathBuf::from(h).join(".rv").join("cache"),
            Err(_) => std::env::temp_dir().join("rv-cache"),
        };
        std::fs::create_dir_all(&cache_dir)?;
        let client = reqwest::Client::new();

        for entry in &lockfile.packages {
            if entry.source != "github" {
                continue;
            }

            let repo = entry.repo.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} is source=github but lockfile has no repo field",
                    entry.name
                )
            })?;
            let r#ref = entry.r#ref.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} is source=github but lockfile has no ref field",
                    entry.name
                )
            })?;
            let expected_sha256 = entry.tarball_sha256.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} is source=github but lockfile has no tarball_sha256 — \
                     cannot verify integrity",
                    entry.name
                )
            })?;

            let (owner, repo_name) = repo.split_once('/').ok_or_else(|| {
                anyhow::anyhow!("Invalid repo field in lockfile: '{}'", repo)
            })?;

            let spec = source::GitHubSpec {
                owner: owner.to_string(),
                repo: repo_name.to_string(),
                r#ref: Some(r#ref.clone()),
                subdir: entry.subdir.clone(),
            };

            let short = &r#ref[..7.min(r#ref.len())];
            print!("  {} gh:{}@{} ", "verifying".dimmed(), repo, short);

            let (_path, actual_sha256) =
                registry::github::download_tarball(&spec, r#ref, &cache_dir, &client)
                    .await
                    .with_context(|| {
                        format!("Failed to download {} for verification", entry.name)
                    })?;

            if &actual_sha256 != expected_sha256 {
                println!("{}", "✗".red());
                anyhow::bail!(
                    "SHA-256 mismatch for {} ({}@{}):\n  \
                     expected (lockfile): {}\n  \
                     actual (downloaded): {}\n  \
                     The lockfile may be corrupted, or the upstream tarball was rewritten.",
                    entry.name, repo, r#ref, expected_sha256, actual_sha256
                );
            }
            println!("{}", "✓".green());
        }
    }

    // ── Convert locked packages into ResolvedPackages for the installer ──
    // Populate github_source so the installer knows to use the cached tarball.
    let resolved = resolver::ResolvedDeps {
        packages: lockfile
            .packages
            .iter()
            .map(|pkg| {
                let github_source = if pkg.source == "github" {
                    let repo = pkg.repo.as_ref().unwrap();
                    let (owner, repo_name) = repo.split_once('/').unwrap();
                    Some(resolver::GitHubSource {
                        owner: owner.to_string(),
                        repo: repo_name.to_string(),
                        commit_sha: pkg.r#ref.clone().unwrap(),
                        subdir: pkg.subdir.clone(),
                        tarball_sha256: pkg.tarball_sha256.clone().unwrap(),
                    })
                } else {
                    None
                };

                resolver::ResolvedPackage {
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

    // ── Skip-already-installed optimization (preserved from before) ──────
    println!("{}", "Checking installed packages...".dimmed());
    let already_installed = installer::check_installed_versions(&resolved.packages);

    let to_install: Vec<&resolver::ResolvedPackage> = resolved
        .packages
        .iter()
        .filter(|pkg| !already_installed.contains(&pkg.name))
        .collect();

    if to_install.is_empty() {
        println!(
            "\n{} All {} packages already installed at correct versions.",
            "✓".green(),
            resolved.packages.len()
        );
        return Ok(());
    }

    println!(
        "\n  {} already installed, {} to install",
        already_installed.len(),
        to_install.len()
    );

    installer::install(&resolved, &lockfile.metadata.bioc_version).await?;

    println!(
        "\n{} Environment restored from rv.lock ({} packages)",
        "✓".green(),
        resolved.packages.len()
    );

    Ok(())
}

/// Create a virtual environment
async fn cmd_venv_create(path: &str, r_version: Option<String>) -> Result<()> {
    use colored::Colorize;

    let venv_dir = std::path::PathBuf::from(path);
    let lib_dir = venv_dir.join("lib");

    if lib_dir.exists() {
        println!("{} Virtual environment already exists at {}/", "✓".green(), path);
        return Ok(());
    }

    // Determine R version
    let r_ver = match r_version {
        Some(v) => v,
        None => {
            // Auto-detect from system
            let output = std::process::Command::new("R")
                .args(["--vanilla", "--slave", "-e", "cat(paste0(R.version$major, '.', R.version$minor))"])
                .output();
            match output {
                Ok(out) if out.status.success() => {
                    String::from_utf8_lossy(&out.stdout).trim().to_string()
                }
                _ => {
                    anyhow::bail!("R not found. Specify R version manually: rv venv --r-version 4.4.0");
                }
            }
        }
    };

    // Create directories
    std::fs::create_dir_all(&lib_dir)?;

    // Write config file
    let config = format!(
        "# rv virtual environment\n\
         r_version = \"{}\"\n\
         created = \"{}\"\n",
        r_ver,
        lockfile::chrono_now()
    );
    std::fs::write(venv_dir.join("config.toml"), config)?;

    // Write activate script (bash/zsh)
    let abs_lib = std::fs::canonicalize(&lib_dir).unwrap_or(lib_dir.clone());
    let abs_venv = std::fs::canonicalize(&venv_dir).unwrap_or(venv_dir.clone());

    let activate_content = format!(
        r#"#!/bin/sh
# rv virtual environment activation script
# Usage: source {path}/activate

# Save old values for deactivate
export _RV_OLD_R_LIBS_USER="${{R_LIBS_USER:-}}"
export _RV_OLD_PS1="${{PS1:-}}"

# Set the library path
export R_LIBS_USER="{lib}"
export RV_VENV="{venv}"

# Update prompt to show active environment
export PS1="(rv:{name}) $PS1"

# Define deactivate function
deactivate() {{
    export R_LIBS_USER="$_RV_OLD_R_LIBS_USER"
    export PS1="$_RV_OLD_PS1"
    unset RV_VENV
    unset _RV_OLD_R_LIBS_USER
    unset _RV_OLD_PS1
    unset -f deactivate
    echo "rv environment deactivated"
}}

echo "rv environment active (R {r_ver})"
echo "  Library: {lib}"
echo "  Deactivate: deactivate"
"#,
        path = path,
        lib = abs_lib.display(),
        venv = abs_venv.display(),
        name = venv_dir.file_name().unwrap_or_default().to_string_lossy(),
        r_ver = r_ver
    );
    std::fs::write(venv_dir.join("activate"), activate_content)?;

    // Write .gitignore for the venv
    std::fs::write(venv_dir.join(".gitignore"), "lib/\n")?;

    println!("{} Created virtual environment at {}/", "✓".green(), path);
    println!("  R version: {}", r_ver);
    println!("  Library:   {}/lib/", path);
    println!(
        "\n  To activate:\n    {}",
        format!("source {}/activate", path).bold()
    );
    println!(
        "  To deactivate:\n    {}",
        "deactivate".bold()
    );

    Ok(())
}

/// Show info about active virtual environment
fn cmd_venv_info() -> Result<()> {
    use colored::Colorize;

    match std::env::var("RV_VENV") {
        Ok(path) => {
            let lib_path = std::path::PathBuf::from(&path);
            let count = std::fs::read_dir(&lib_path)
                .map(|entries| entries.filter(|e| e.is_ok()).count())
                .unwrap_or(0);

            println!("  Active environment: {}", path);
            println!("  Packages installed: {}", count);
            println!("  Deactivate: deactivate");
        }
        Err(_) => {
            // Check if .rv exists but isn't activated
            if std::path::Path::new(".rv/lib").exists() {
                println!("{} Virtual environment exists but is not activated", "!".yellow());
                println!("  Run: source .rv/activate");
            } else {
                println!("{} No virtual environment found", "✗".red());
                println!("  Run: rv venv");
            }
        }
    }

    Ok(())
}

/// Remove a virtual environment
fn cmd_venv_remove(path: &str) -> Result<()> {
    use colored::Colorize;

    let venv_dir = std::path::PathBuf::from(path);
    if venv_dir.exists() {
        std::fs::remove_dir_all(&venv_dir)?;
        println!("{} Removed {}/", "✓".green(), path);
    } else {
        println!("{} No virtual environment at {}/", "✗".red(), path);
    }

    Ok(())
}


