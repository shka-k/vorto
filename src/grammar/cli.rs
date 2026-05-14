//! `vorto grammar …` subcommand dispatcher.
//!
//! Three operations:
//!
//! * `list` — print every built-in recipe with installed/missing status.
//! * `install <name>…` (or `--all`) — fetch, build, and place the
//!   `.so`/`.dylib`/`.dll` into the configured `grammar_dir`.
//! * `remove <name>…` — delete the installed library.
//!
//! The grammar directory is read from the same `Config::load` path the
//! editor uses, so anything installed here is immediately picked up next
//! time the editor starts.

use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{self, Config};

use super::assets;
use super::build;
use super::recipe::{builtin_recipes, find_recipe};

/// Entry point invoked from `main` when `argv[1] == "grammar"`. `args`
/// is everything after the `grammar` token.
pub fn run(args: &[String]) -> Result<()> {
    let cfg = Config::load(config::default_path().as_deref())?;
    let grammar_dir = cfg.grammar_dir.as_path();
    let query_dir = cfg.query_dir.as_path();

    match args.split_first() {
        None => {
            print_usage();
            Ok(())
        }
        Some((cmd, rest)) => match cmd.as_str() {
            "list" | "ls" => list(grammar_dir, query_dir),
            "install" | "add" => install(rest, grammar_dir, query_dir),
            "remove" | "rm" | "uninstall" => remove(rest, grammar_dir),
            "help" | "-h" | "--help" => {
                print_usage();
                Ok(())
            }
            other => {
                print_usage();
                bail!("unknown grammar subcommand: `{}`", other);
            }
        },
    }
}

fn print_usage() {
    eprintln!("usage: vorto grammar <command> [args]");
    eprintln!();
    eprintln!("commands:");
    eprintln!("  list                       show built-in recipes and install status");
    eprintln!("  install <name>... | --all  build and install one or more grammars");
    eprintln!("  remove <name>...           delete installed grammar libraries");
    eprintln!();
    eprintln!("examples:");
    eprintln!("  vorto grammar install rust python");
    eprintln!("  vorto grammar install --all");
    eprintln!("  vorto grammar list");
}

fn list(grammar_dir: &Path, query_dir: &Path) -> Result<()> {
    println!("grammar dir: {}", grammar_dir.display());
    println!("query dir:   {}", query_dir.display());
    println!();
    for r in builtin_recipes() {
        let lib_status = match build::installed_path(r.name, grammar_dir) {
            Some(_) => "lib ✓",
            None => "lib ✗",
        };
        let installed = build::installed_queries(r.name, query_dir);
        let bundled = assets::bundled_query_names(r.name);
        let query_status = match (installed.is_empty(), bundled.is_empty()) {
            (false, _) => format!("queries: {} (installed)", installed.join(",")),
            (true, false) => format!("queries: {} (bundled, not installed)", bundled.join(",")),
            (true, true) => "queries: none bundled".to_string(),
        };
        let subpath = r.subpath.map(|s| format!(" [{}]", s)).unwrap_or_default();
        println!(
            "  {:<12} {}{}\n               {} | {}",
            r.name, r.repo, subpath, lib_status, query_status
        );
    }
    Ok(())
}

fn install(args: &[String], grammar_dir: &Path, query_dir: &Path) -> Result<()> {
    let recipes = match args.first().map(String::as_str) {
        None => {
            bail!("install: need at least one grammar name (or `--all`)");
        }
        Some("--all") => builtin_recipes(),
        _ => {
            let mut out = Vec::new();
            for name in args {
                match find_recipe(name) {
                    Some(r) => out.push(r),
                    None => bail!(
                        "unknown grammar `{}`. Try `vorto grammar list` to see built-ins.",
                        name
                    ),
                }
            }
            out
        }
    };

    let mut failures = Vec::new();
    for r in &recipes {
        if build::is_fully_installed(r.name, grammar_dir, query_dir) {
            eprintln!("==> {} already installed, skipping", r.name);
            continue;
        }
        eprintln!("==> installing {} ({})", r.name, r.repo);
        match build::install(r, grammar_dir, query_dir) {
            Ok(report) => {
                eprintln!("    lib: {}", report.library.display());
                if report.queries.is_empty() {
                    eprintln!("    queries: none shipped in upstream `queries/`");
                } else {
                    let names: Vec<String> = report
                        .queries
                        .iter()
                        .filter_map(|p| {
                            p.file_name().map(|n| n.to_string_lossy().into_owned())
                        })
                        .collect();
                    eprintln!(
                        "    queries: {} ({} files)",
                        names.join(", "),
                        report.queries.len()
                    );
                }
            }
            Err(e) => {
                eprintln!("    failed: {:#}", e);
                failures.push(r.name);
            }
        }
    }
    if !failures.is_empty() {
        bail!("failed to install: {}", failures.join(", "));
    }
    Ok(())
}

fn remove(args: &[String], grammar_dir: &Path) -> Result<()> {
    if args.is_empty() {
        bail!("remove: need at least one grammar name");
    }
    for name in args {
        let removed = build::remove(name, grammar_dir)?;
        if removed {
            eprintln!("removed: {}", name);
        } else {
            eprintln!("not installed: {}", name);
        }
    }
    Ok(())
}
