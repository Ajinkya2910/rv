// src/cli.rs — Command-line interface definition
//
// RUST CONCEPT: Structs and Enums
// In Rust, you define data shapes with `struct` (like a Python dataclass)
// and `enum` (like a tagged union — way more powerful than Python enums).
//
// RUST CONCEPT: Derive Macros
// `#[derive(Parser)]` is a "derive macro" — it auto-generates code at
// compile time. When you write #[derive(Parser)] on a struct, the clap
// library generates all the argument parsing code for you. No manual
// argparse setup needed. It's like Python's dataclasses but for CLI args.

use clap::{Parser, Subcommand};

/// rv — Fast R package manager for life sciences
///
/// This doc comment (///) becomes the --help text automatically!
#[derive(Parser)]
#[command(name = "rv")]
#[command(version)]
#[command(about = "Fast R package manager for life sciences")]
pub struct Cli {
    /// The subcommand to run (resolve, audit, install, etc.)
    #[command(subcommand)]
    pub command: Commands,
}

// RUST CONCEPT: Enums in Rust can hold data!
// Unlike Python enums which are just labels, Rust enums are "sum types."
// Each variant can contain different data. This is perfect for CLI subcommands:
//
//   Commands::Resolve { packages: vec!["DESeq2"] }
//   Commands::Install { packages: vec!["DESeq2"], retry: true }
//
// The compiler guarantees you handle every variant in a `match`.

#[derive(Subcommand)]
pub enum Commands {
    /// Show the full dependency tree for packages
    ///
    /// Example: rv resolve DESeq2 ggplot2
    Resolve {
        /// Package names to resolve
        #[arg(required = true)]
        packages: Vec<String>,
    },

    /// Check system dependencies before installing
    ///
    /// Example: rv audit DESeq2
    Audit {
        /// Package names to audit
        #[arg(required = true)]
        packages: Vec<String>,
    },

    /// Install packages (with pre-flight checks and parallel compilation)
    ///
    /// Example: rv install DESeq2
    /// Example: rv install --retry  (resume after fixing errors)
    Install {
        /// Package names to install
        #[arg(required_unless_present = "retry")]
        packages: Vec<String>,

        /// Resume a previously failed installation
        #[arg(long, default_value_t = false)]
        retry: bool,
    },

    /// Explain why a package is in the dependency tree
    ///
    /// Example: rv why rlang
    Why {
        /// Package name to trace
        #[arg(required = true)]
        package: String,
    },

    /// Generate a lockfile for reproducibility
    ///
    /// Example: rv lock DESeq2 clusterProfiler EnhancedVolcano
    Lock {
        /// Package names to lock
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Restore packages from an rv.lock file
    ///
    /// Example: rv restore
    Restore,
       /// Create and manage project-local R library
    ///
    /// Example: rv venv
    /// Example: rv venv my-project
    /// Example: rv venv --r-version 4.4.0
    Venv {
        /// Name or path for the virtual environment (default: .rv)
        #[arg(default_value = ".rv")]
        path: String,

        /// R version to use (default: auto-detect)
        #[arg(long)]
        r_version: Option<String>,
    },

    /// Show info about the active virtual environment
    VenvInfo,

    /// Remove a virtual environment
    VenvRemove {
        /// Path to the virtual environment to remove (default: .rv)
        #[arg(default_value = ".rv")]
        path: String,
    },
}

