// src/sysreq.rs — System dependency checking and resolution
//
// Detection strategy (in order of precedence):
//   1. Capability-based probes — `pkg-config --exists` or `which <tool>`.
//      This is what actually matters: can the R build find the library?
//      Works on any OS, any install method (apt / rpm / brew / HPC modules / manual).
//   2. Package-manager fallback — dpkg (Debian/Ubuntu), rpm (RHEL/Rocky/Fedora),
//      brew (macOS). Used only if capability probe has no entry for the lib.
//
// Why capability-first:
//   - On HPC clusters, compilers and libs come from `module load`, not `apt`.
//     dpkg / rpm will say "not installed" even though the tool is on PATH.
//   - On a random dev machine, someone may have built openssl from source.
//     pkg-config will find it; dpkg won't.
//   - The R build system itself uses pkg-config to locate libraries, so
//     if pkg-config reports the lib, R will find it too.

use crate::resolver::ResolvedDeps;
use anyhow::Result;
use std::process::Command;

/// Result of auditing system dependencies
#[derive(Debug)]
pub struct SysreqReport {
    pub found: Vec<InstalledDep>,
    pub missing: Vec<MissingDep>,
}

#[derive(Debug)]
pub struct InstalledDep {
    pub name: String,
    pub version: String,
}

#[derive(Debug)]
pub struct MissingDep {
    pub name: String,
    pub needed_by: Vec<String>,
}

/// R package → required system libraries (Debian-style canonical names).
/// RHEL/macOS names are translated via RPM_MAP / BREW_MAP below.
const SYSREQ_MAP: &[(&str, &[&str])] = &[
    // Bioconductor packages
    ("rhdf5", &["libhdf5-dev"]),
    ("HDF5Array", &["libhdf5-dev"]),
    ("Rhtslib", &["libbz2-dev", "liblzma-dev", "libcurl4-openssl-dev"]),
    ("Rsamtools", &["libbz2-dev", "liblzma-dev"]),

    // Common CRAN packages with system deps
    ("curl", &["libcurl4-openssl-dev"]),
    ("openssl", &["libssl-dev"]),
    ("xml2", &["libxml2-dev"]),
    ("httr", &["libcurl4-openssl-dev", "libssl-dev"]),
    ("git2r", &["libgit2-dev"]),
    ("sodium", &["libsodium-dev"]),
    ("RcppGSL", &["libgsl-dev"]),
    ("gsl", &["libgsl-dev"]),
    ("nloptr", &["cmake"]),
    ("sf", &["libgdal-dev", "libgeos-dev", "libproj-dev"]),
    ("terra", &["libgdal-dev", "libgeos-dev", "libproj-dev"]),
    ("magick", &["libmagick++-dev"]),
    ("av", &["libavfilter-dev"]),
    ("ragg", &["libfreetype6-dev", "libpng-dev", "libtiff5-dev"]),
    ("textshaping", &["libharfbuzz-dev", "libfribidi-dev"]),
    ("systemfonts", &["libfontconfig1-dev"]),
    ("cairo", &["libcairo2-dev"]),
    ("rjags", &["jags"]),
    ("RMySQL", &["libmariadb-dev"]),
    ("RPostgres", &["libpq-dev"]),
    ("odbc", &["unixodbc-dev"]),

    // Compilation essentials
    ("Rcpp", &["build-essential"]),
    ("RcppArmadillo", &["build-essential"]),
    ("RcppEigen", &["build-essential"]),
];

/// RUST CONCEPT: Enums with data
/// Like Python's Enum but each variant can carry its own typed payload.
/// This is Rust's standard way to model "one of these cases, each with
/// different data" — cleaner than Python's match-on-isinstance patterns.
///
/// Probe = how to detect a given library without calling a package manager.
enum Probe {
    /// Look for an executable on PATH, e.g. `gcc`, `gfortran`, `cmake`.
    /// Equivalent to: `which <name>` returning 0.
    Bin(&'static str),

    /// Use pkg-config to check for a development library, e.g. `libcurl`, `openssl`.
    /// Equivalent to: `pkg-config --exists <name>` returning 0.
    Pc(&'static str),
}

/// PRIMARY detection map: Debian canonical name → capability probe.
///
/// Ordering principle: if a library exposes itself via `pkg-config`, use `Pc`.
/// If it exposes itself as a binary (compilers, *-config tools), use `Bin`.
const CAPABILITY_MAP: &[(&str, Probe)] = &[
    // --- Compilers & build tools (binary probes) ---
    ("build-essential", Probe::Bin("gcc")),
    ("gfortran",        Probe::Bin("gfortran")),
    ("cmake",           Probe::Bin("cmake")),

    // --- Libraries discoverable via pkg-config ---
    ("libcurl4-openssl-dev", Probe::Pc("libcurl")),
    ("libssl-dev",           Probe::Pc("openssl")),
    ("libxml2-dev",          Probe::Pc("libxml-2.0")),
    ("libhdf5-dev",          Probe::Pc("hdf5")),
    ("libgsl-dev",           Probe::Pc("gsl")),
    ("libgit2-dev",          Probe::Pc("libgit2")),
    ("libsodium-dev",        Probe::Pc("libsodium")),
    ("libfontconfig1-dev",   Probe::Pc("fontconfig")),
    ("libharfbuzz-dev",      Probe::Pc("harfbuzz")),
    ("libfribidi-dev",       Probe::Pc("fribidi")),
    ("libfreetype6-dev",     Probe::Pc("freetype2")),
    ("libpng-dev",           Probe::Pc("libpng")),
    ("libtiff5-dev",         Probe::Pc("libtiff-4")),
    ("libcairo2-dev",        Probe::Pc("cairo")),
    ("libpq-dev",            Probe::Pc("libpq")),
    ("libproj-dev",          Probe::Pc("proj")),
    ("libavfilter-dev",      Probe::Pc("libavfilter")),

    // --- Libraries that ship their own *-config tool instead of pkg-config ---
    ("libgdal-dev",     Probe::Bin("gdal-config")),
    ("libgeos-dev",     Probe::Bin("geos-config")),
    ("libmariadb-dev",  Probe::Bin("mariadb_config")),
    ("unixodbc-dev",    Probe::Bin("odbc_config")),
    ("libmagick++-dev", Probe::Bin("Magick++-config")),

    // --- Binaries / libs without pkg-config files ---
    ("jags",        Probe::Bin("jags")),
    ("libbz2-dev",  Probe::Bin("bzip2")),   // bzip2 rarely ships pkg-config
    ("liblzma-dev", Probe::Bin("xz")),      // similarly for lzma
];

/// Debian canonical name → RHEL/Rocky/Fedora package name.
/// Used as fallback when capability probe has no entry (or we're installing).
const RPM_MAP: &[(&str, &str)] = &[
    ("libcurl4-openssl-dev", "libcurl-devel"),
    ("libssl-dev",           "openssl-devel"),
    ("libxml2-dev",          "libxml2-devel"),
    ("libhdf5-dev",          "hdf5-devel"),
    ("libgsl-dev",           "gsl-devel"),
    ("libgit2-dev",          "libgit2-devel"),
    ("libsodium-dev",        "libsodium-devel"),
    ("libfontconfig1-dev",   "fontconfig-devel"),
    ("libharfbuzz-dev",      "harfbuzz-devel"),
    ("libfribidi-dev",       "fribidi-devel"),
    ("libfreetype6-dev",     "freetype-devel"),
    ("libpng-dev",           "libpng-devel"),
    ("libtiff5-dev",         "libtiff-devel"),
    ("libcairo2-dev",        "cairo-devel"),
    ("libmagick++-dev",      "ImageMagick-c++-devel"),
    ("libpq-dev",            "libpq-devel"),
    ("libmariadb-dev",       "mariadb-devel"),
    ("libgdal-dev",          "gdal-devel"),
    ("libgeos-dev",          "geos-devel"),
    ("libproj-dev",          "proj-devel"),
    ("libbz2-dev",           "bzip2-devel"),
    ("liblzma-dev",          "xz-devel"),
    ("libavfilter-dev",      "ffmpeg-devel"),
    ("unixodbc-dev",         "unixODBC-devel"),
    ("build-essential",      "gcc-c++"),
    ("gfortran",             "gcc-gfortran"),
    ("cmake",                "cmake"),
    ("jags",                 "jags"),
];

/// Linux apt name → macOS Homebrew name
const BREW_MAP: &[(&str, &str)] = &[
    ("libcurl4-openssl-dev", "curl"),
    ("libssl-dev", "openssl"),
    ("libxml2-dev", "libxml2"),
    ("libhdf5-dev", "hdf5"),
    ("libgsl-dev", "gsl"),
    ("libgit2-dev", "libgit2"),
    ("libsodium-dev", "libsodium"),
    ("libfontconfig1-dev", "fontconfig"),
    ("libharfbuzz-dev", "harfbuzz"),
    ("libfribidi-dev", "fribidi"),
    ("libfreetype6-dev", "freetype"),
    ("libpng-dev", "libpng"),
    ("libtiff5-dev", "libtiff"),
    ("libcairo2-dev", "cairo"),
    ("libmagick++-dev", "imagemagick"),
    ("libpq-dev", "postgresql"),
    ("libmariadb-dev", "mariadb"),
    ("libgdal-dev", "gdal"),
    ("libgeos-dev", "geos"),
    ("libproj-dev", "proj"),
    ("build-essential", "gcc"),
    ("gfortran", "gcc"),
    ("cmake", "cmake"),
];

/// RUST CONCEPT: Enums as lightweight tags
/// Think of this as a Python Enum but zero runtime cost.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LinuxFamily {
    Debian, // Ubuntu, Debian, Mint, ...
    Rhel,   // RHEL, Rocky, CentOS, AlmaLinux, Fedora
    Unknown,
}

/// Detect the Linux distribution family by reading /etc/os-release.
///
/// The `ID=` and `ID_LIKE=` fields are standardized by systemd's os-release spec.
/// Examples:
///   Ubuntu:    ID=ubuntu  ID_LIKE=debian
///   Rocky:     ID=rocky   ID_LIKE="rhel centos fedora"
///   Fedora:    ID=fedora  (no ID_LIKE)
fn linux_family() -> LinuxFamily {
    let content = match std::fs::read_to_string("/etc/os-release") {
        Ok(s) => s,
        Err(_) => return LinuxFamily::Unknown,
    };

    let (mut id, mut id_like) = (String::new(), String::new());
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("ID=") {
            id = v.trim_matches('"').to_lowercase();
        } else if let Some(v) = line.strip_prefix("ID_LIKE=") {
            id_like = v.trim_matches('"').to_lowercase();
        }
    }

    let combined = format!("{} {}", id, id_like);
    if ["debian", "ubuntu", "mint"].iter().any(|k| combined.contains(k)) {
        LinuxFamily::Debian
    } else if ["rhel", "fedora", "centos", "rocky", "almalinux", "ol"]
        .iter().any(|k| combined.contains(k))
    {
        LinuxFamily::Rhel
    } else {
        LinuxFamily::Unknown
    }
}

fn is_macos() -> bool {
    std::env::consts::OS == "macos"
}

// ---------------------------------------------------------------------------
// Capability probes
// ---------------------------------------------------------------------------

/// Is this binary on PATH?
/// Python equivalent: `shutil.which(name) is not None`
fn has_binary(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Does pkg-config know about this library?
fn has_pkgconfig(name: &str) -> bool {
    Command::new("pkg-config")
        .args(["--exists", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Get the version string from pkg-config (best-effort).
fn pkgconfig_version(name: &str) -> String {
    Command::new("pkg-config")
        .args(["--modversion", name])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "detected".to_string())
}

/// Try capability-based detection.
/// Returns `Some(version)` if the library is usable on this system,
/// regardless of how it was installed.
fn check_capability(lib_name: &str) -> Option<String> {
    // RUST CONCEPT: The `?` operator on Option
    // `find(...)` returns Option<&(K, V)>. `.map(...)` transforms the inner.
    // `?` unwraps the Some or early-returns None — like Python's walrus-with-guard.
    let probe = CAPABILITY_MAP
        .iter()
        .find(|(n, _)| *n == lib_name)
        .map(|(_, p)| p)?;

    match probe {
        Probe::Bin(name) => {
            if has_binary(name) {
                Some("detected".to_string())
            } else {
                None
            }
        }
        Probe::Pc(name) => {
            if has_pkgconfig(name) {
                Some(pkgconfig_version(name))
            } else {
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Package-manager fallback checks
// ---------------------------------------------------------------------------

fn check_dpkg_installed(package_name: &str) -> Option<String> {
    let output = Command::new("dpkg")
        .args(["-s", package_name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let is_installed = stdout
        .lines()
        .any(|line| line.starts_with("Status:") && line.contains("installed"));
    if !is_installed {
        return None;
    }
    let version = stdout
        .lines()
        .find(|line| line.starts_with("Version:"))
        .map(|line| line.trim_start_matches("Version:").trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    Some(version)
}

fn check_rpm_installed(package_name: &str) -> Option<String> {
    // Translate Debian-style canonical name → RHEL package name
    let rpm_name = RPM_MAP
        .iter()
        .find(|(deb, _)| *deb == package_name)
        .map(|(_, rpm)| *rpm)
        .unwrap_or(package_name);

    // rpm -q prints "package not installed" to stdout (with non-zero exit) when absent,
    // or "<name>-<version>-<release>" when present. Using --qf makes parsing reliable.
    let output = Command::new("rpm")
        .args(["-q", "--qf", "%{VERSION}", rpm_name])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() || version.contains("not installed") {
        None
    } else {
        Some(version)
    }
}

fn check_brew_installed(linux_name: &str) -> Option<String> {
    // Special cases: these come from Xcode, not Homebrew
    if linux_name == "build-essential" {
        let output = Command::new("cc").arg("--version").output().ok()?;
        if output.status.success() {
            return Some("xcode".to_string());
        }
    }
    if linux_name == "gfortran" {
        let output = Command::new("gfortran").arg("--version").output().ok()?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let version = stdout.lines().next().unwrap_or("installed");
            return Some(version.to_string());
        }
    }
    if linux_name == "libcurl4-openssl-dev" {
        let output = Command::new("curl").arg("--version").output().ok()?;
        if output.status.success() {
            return Some("system".to_string());
        }
    }

    let brew_name = BREW_MAP
        .iter()
        .find(|(linux, _)| *linux == linux_name)
        .map(|(_, brew)| *brew)
        .unwrap_or(linux_name);

    let output = Command::new("brew")
        .args(["list", "--versions", brew_name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    let version = trimmed.split_whitespace().last().unwrap_or("unknown");
    Some(version.to_string())
}

/// Unified install-check: capability first, package manager second.
fn check_installed(package_name: &str) -> Option<String> {
    // Primary: capability-based detection.
    if let Some(v) = check_capability(package_name) {
        return Some(v);
    }
    // Fallback: OS-specific package manager.
    if is_macos() {
        check_brew_installed(package_name)
    } else {
        match linux_family() {
            LinuxFamily::Debian => check_dpkg_installed(package_name),
            LinuxFamily::Rhel => check_rpm_installed(package_name),
            LinuxFamily::Unknown => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn audit(resolved: &ResolvedDeps) -> Result<SysreqReport> {
    let mut required_syslibs: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for pkg in &resolved.packages {
        for (r_pkg, sys_libs) in SYSREQ_MAP {
            if pkg.name == *r_pkg {
                for lib in *sys_libs {
                    required_syslibs
                        .entry(lib.to_string())
                        .or_default()
                        .push(pkg.name.clone());
                }
            }
        }

        if pkg.needs_compilation {
            required_syslibs
                .entry("build-essential".to_string())
                .or_default()
                .push(pkg.name.clone());

            let needs_fortran = pkg
                .dependencies
                .iter()
                .any(|d| ["Matrix", "survival", "minqa"].contains(&d.as_str()));

            if needs_fortran {
                required_syslibs
                    .entry("gfortran".to_string())
                    .or_default()
                    .push(pkg.name.clone());
            }
        }
    }

    let mut found = Vec::new();
    let mut missing = Vec::new();

    for (lib_name, needed_by) in &required_syslibs {
        if let Some(version) = check_installed(lib_name) {
            found.push(InstalledDep {
                name: lib_name.clone(),
                version,
            });
        } else {
            missing.push(MissingDep {
                name: lib_name.clone(),
                needed_by: needed_by.clone(),
            });
        }
    }

    Ok(SysreqReport { found, missing })
}

pub fn get_brew_name(linux_name: &str) -> String {
    BREW_MAP
        .iter()
        .find(|(linux, _)| *linux == linux_name)
        .map(|(_, brew)| brew.to_string())
        .unwrap_or_else(|| linux_name.to_string())
}

/// Install missing system packages using the correct package manager.
pub fn install_missing(report: &SysreqReport) -> Result<()> {
    if report.missing.is_empty() {
        return Ok(());
    }

    if is_macos() {
        let brew_names: Vec<String> = report
            .missing
            .iter()
            .map(|d| {
                BREW_MAP
                    .iter()
                    .find(|(linux, _)| *linux == d.name.as_str())
                    .map(|(_, brew)| brew.to_string())
                    .unwrap_or_else(|| d.name.clone())
            })
            .collect();

        println!("Running: brew install {}", brew_names.join(" "));
        let status = Command::new("brew").arg("install").args(&brew_names).status()?;
        if !status.success() {
            anyhow::bail!(
                "Failed to install. Run manually:\n  brew install {}",
                brew_names.join(" ")
            );
        }
        return Ok(());
    }

    match linux_family() {
        LinuxFamily::Debian => {
            let names: Vec<&str> = report.missing.iter().map(|d| d.name.as_str()).collect();
            println!("Running: sudo apt install -y {}", names.join(" "));
            let status = Command::new("sudo")
                .arg("apt").arg("install").arg("-y")
                .args(&names)
                .status()?;
            if !status.success() {
                anyhow::bail!(
                    "Failed to install. Run manually:\n  sudo apt install {}",
                    names.join(" ")
                );
            }
        }
        LinuxFamily::Rhel => {
            let rpm_names: Vec<String> = report
                .missing
                .iter()
                .map(|d| {
                    RPM_MAP
                        .iter()
                        .find(|(deb, _)| *deb == d.name.as_str())
                        .map(|(_, rpm)| rpm.to_string())
                        .unwrap_or_else(|| d.name.clone())
                })
                .collect();

            // Prefer dnf (modern), fall back to yum.
            let installer = if has_binary("dnf") { "dnf" } else { "yum" };
            println!("Running: sudo {} install -y {}", installer, rpm_names.join(" "));
            let status = Command::new("sudo")
                .arg(installer).arg("install").arg("-y")
                .args(&rpm_names)
                .status()?;
            if !status.success() {
                anyhow::bail!(
                    "Failed to install.\n\
                     If you are on an HPC system, you likely do not have sudo privileges.\n\
                     Ask your admin, or load the appropriate environment modules \
                     (e.g. `module load gcc openssl libcurl`).\n\
                     Manual command: sudo {} install {}",
                    installer,
                    rpm_names.join(" ")
                );
            }
        }
        LinuxFamily::Unknown => {
            anyhow::bail!(
                "Could not detect Linux distribution family. Please install these manually: {}",
                report.missing.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Makevars fixup (macOS-specific, unchanged)
// ---------------------------------------------------------------------------

pub fn check_makevars() -> Option<MakevarsFix> {
    if !is_macos() {
        return None;
    }

    let gfortran_check = Command::new("gfortran").arg("--version").output().ok()?;
    if !gfortran_check.status.success() {
        return None;
    }

    let flibs_output = Command::new("R").args(["CMD", "config", "FLIBS"]).output().ok()?;
    let flibs = String::from_utf8_lossy(&flibs_output.stdout).trim().to_string();

    let bad_paths: Vec<String> = flibs
        .split_whitespace()
        .filter(|s| s.starts_with("-L"))
        .map(|s| s.trim_start_matches("-L").to_string())
        .filter(|path| !std::path::Path::new(path).exists())
        .collect();

    if bad_paths.is_empty() {
        return None;
    }

    let gfortran_path = Command::new("which")
        .arg("gfortran")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let correct_lib = if gfortran_path.contains("/opt/homebrew/") {
        "/opt/homebrew/lib/gcc/current".to_string()
    } else if gfortran_path.contains("/usr/local/") {
        "/usr/local/lib/gcc/current".to_string()
    } else {
        let parent = std::path::Path::new(&gfortran_path)
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("lib/gcc/current"))
            .unwrap_or_default();
        parent.to_string_lossy().to_string()
    };

    let makevars_path = dirs_or_home().join(".R/Makevars");
    if makevars_path.exists() {
        let content = std::fs::read_to_string(&makevars_path).unwrap_or_default();
        if content.contains(&correct_lib) {
            return None;
        }
    }

    Some(MakevarsFix {
        bad_paths,
        correct_lib,
        gfortran_path,
        makevars_path,
    })
}

fn dirs_or_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
}

pub struct MakevarsFix {
    pub bad_paths: Vec<String>,
    pub correct_lib: String,
    pub gfortran_path: String,
    pub makevars_path: std::path::PathBuf,
}

pub fn fix_makevars(fix: &MakevarsFix) -> Result<()> {
    let r_dir = fix.makevars_path.parent().unwrap();
    std::fs::create_dir_all(r_dir)?;

    let makevars_content = format!(
        "# Added by rv — fixes gfortran path for Homebrew\n\
         FC = {}\n\
         FLIBS = -L{} -lgfortran -lquadmath\n",
        fix.gfortran_path, fix.correct_lib
    );

    if fix.makevars_path.exists() {
        let existing = std::fs::read_to_string(&fix.makevars_path)?;
        if !existing.contains("FC =") {
            let updated = format!("{}\n{}", existing.trim(), makevars_content);
            std::fs::write(&fix.makevars_path, updated)?;
        }
    } else {
        std::fs::write(&fix.makevars_path, makevars_content)?;
    }

    Ok(())
}