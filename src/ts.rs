//! TypeScript/JavaScript tests: each file runs (in parallel) against its own fresh in-process chain.
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn which(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

pub fn files(root: &Path, test_dir: &str, filter: Option<&str>) -> Vec<PathBuf> {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if p.is_dir() && name != "node_modules" {
                walk(&p, out);
            } else if [".test.ts", ".test.js", ".test.mjs"].iter().any(|s| name.ends_with(s)) {
                out.push(p);
            }
        }
    }
    let mut out = vec![];
    walk(&root.join(test_dir), &mut out);
    out.retain(|p| filter.is_none_or(|f| p.to_string_lossy().contains(f)));
    out.sort();
    out
}

/// Returns true when every file passed.
pub fn run(root: &Path, files: &[PathBuf], verbose: bool, fork: Option<String>) -> bool {
    let (bun, node) = (which("bun"), which("node"));
    if !bun && !node {
        crate::die("TypeScript tests need bun or node on PATH");
    }
    let t0 = Instant::now();
    let results: Vec<(String, bool, String, std::time::Duration)> = files
        .par_iter()
        .map(|f| {
            let t = Instant::now();
            let (port, _chain) = crate::node::start(fork.clone(), 0, !verbose);
            let src = std::fs::read_to_string(f).unwrap_or_default();
            // bun:test files need bun; everything else prefers node's built-in runner
            let mut cmd = if (src.contains("bun:test") && bun) || !node { Command::new("bun") } else { Command::new("node") };
            if cmd.get_program() == "bun" {
                cmd.arg("test");
            } else {
                cmd.arg("--test");
            }
            let out = cmd
                .arg(f)
                .current_dir(root)
                .env("PLAIN_RPC_URL", format!("http://127.0.0.1:{port}"))
                .env("PLAIN_ARTIFACTS", root.join(".plain/out"))
                .output();
            let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
            match out {
                Ok(o) => (rel, o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)), t.elapsed()),
                Err(e) => (rel, false, e.to_string(), t.elapsed()),
            }
        })
        .collect();
    let failed = results.iter().filter(|r| !r.1).count();
    for (f, ok, out, d) in &results {
        println!("{f}: {} ({d:.0?})", if *ok { "passed" } else { "FAILED" });
        if !ok || verbose {
            let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
            for l in &lines[lines.len().saturating_sub(60)..] {
                println!("  {l}");
            }
        }
    }
    println!("{} ts files passed, {failed} failed ({:.2?})", results.len() - failed, t0.elapsed());
    failed == 0
}
