//! `plain fmt`: forge fmt's formatter. The printer (pp/, state/, comments/, config.rs, iter.rs) is Foundry
//! 1.7.1's forge-fmt (crates/fmt, crates/common/src/comments, crates/config/src/fmt.rs; MIT, Copyright (c) 2021
//! Georgios Konstantopoulos, see LICENSE-MIT here) with only its import paths changed and unused code removed,
//! so the output is forge fmt's byte for byte. It parses with solar-parse, the parser forge itself uses.

const DEBUG: bool = false;
const DEBUG_INDENT: bool = false;

mod comments;
mod config;
mod iter;
mod pp;
mod state;

use comments::{
    Comment, Comments,
    inline_config::{InlineConfig, InlineConfigItem},
};
pub use config::*;
use solar_parse::{
    Parser,
    ast::{self, SourceUnit, Span},
    interface::{Session, source_map::FileName},
};
use std::{path::{Path, PathBuf}, sync::Arc};

/// Formats one file's source; Err holds solar's diagnostics.
pub fn format(src: &str, config: &FormatterConfig) -> Result<String, String> {
    let sess = session();
    sess.enter_sequential(|| format_in(&sess, FileName::Stdin, src.to_string(), Arc::new(config.clone())))
}

fn session() -> Session {
    Session::builder().with_buffer_emitter(Default::default()).single_threaded().build()
}

fn format_in(sess: &Session, name: FileName, src: String, config: Arc<FormatterConfig>) -> Result<String, String> {
    let file = sess.source_map().new_source_file(name, src).map_err(|e| e.to_string())?;
    let arena = ast::Arena::new();
    let mut parser = Parser::from_source_file(sess, &arena, &file);
    let unit = parser.parse_file().map_err(|e| {
        e.emit();
    });
    let errors = || sess.dcx.emitted_errors().and_then(|r| r.err()).map(|d| d.to_string()).unwrap_or_else(|| "parse error".into());
    let Ok(unit) = unit else { return Err(errors()) };
    let unit: &SourceUnit<'_> = arena.alloc(unit);
    if sess.dcx.has_errors().is_err() {
        return Err(errors());
    }
    let comments = Comments::new(&file, sess.source_map(), true, config.wrap_comments, matches!(config.style, IndentStyle::Tab).then_some(config.tab_width));
    let inline_config = parse_inline_config(sess, &comments, unit);
    let mut state = state::State::new(sess.source_map(), config, inline_config, comments);
    state.print_source_unit(unit);
    Ok(state.s.eof())
}

fn parse_inline_config<'ast>(sess: &Session, comments: &Comments, ast: &'ast SourceUnit<'ast>) -> InlineConfig<()> {
    let parse_item = |mut item: &str, cmnt: &Comment| -> Option<(Span, InlineConfigItem<()>)> {
        if let Some(prefix) = cmnt.prefix() {
            item = item.strip_prefix(prefix).unwrap_or(item);
        }
        if let Some(suffix) = cmnt.suffix() {
            item = item.strip_suffix(suffix).unwrap_or(item);
        }
        let item = item.trim_start().strip_prefix("forgefmt:")?.trim();
        match item.parse::<InlineConfigItem<()>>() {
            Ok(item) => Some((cmnt.span, item)),
            Err(e) => {
                sess.dcx.warn(e.to_string()).span(cmnt.span).emit();
                None
            }
        }
    };
    let items = comments.iter().flat_map(|cmnt| {
        let mut found = Vec::with_capacity(2);
        if let Some(item) = cmnt.lines.first().and_then(|l| parse_item(l, cmnt)) {
            found.push(item);
        }
        if cmnt.lines.len() > 1
            && let Some(item) = cmnt.lines.last().and_then(|l| parse_item(l, cmnt))
        {
            found.push(item);
        }
        found
    });
    InlineConfig::from_ast(items, ast, sess.source_map())
}

/// foundry.toml's `[fmt]`, overridden by `[profile.default.fmt]`, then `[profile.$FOUNDRY_PROFILE.fmt]`, then
/// plain.toml's `[fmt]`. A key forge doesn't know is an error, not a silent difference.
pub fn load_config(root: &Path) -> FormatterConfig {
    let mut t = toml::Table::new();
    let mut merge = |v: Option<&toml::Value>| {
        if let Some(v) = v.and_then(|v| v.as_table()) {
            t.extend(v.clone());
        }
    };
    let read = |f: &str| std::fs::read_to_string(root.join(f)).ok().map(|s| s.parse::<toml::Table>().unwrap_or_else(|e| crate::die(&format!("{f}: {e}"))));
    if let Some(f) = read("foundry.toml") {
        let profile = |name: &str| f.get("profile").and_then(|p| p.get(name)).and_then(|p| p.get("fmt"));
        merge(f.get("fmt"));
        merge(profile("default"));
        if let Some(name) = crate::project::var("FOUNDRY_PROFILE").filter(|n| !n.is_empty() && n != "default") {
            merge(profile(&name));
        }
    }
    if let Some(p) = read("plain.toml") {
        merge(p.get("fmt"));
    }
    serde::Deserialize::deserialize(toml::Value::Table(t)).unwrap_or_else(|e: toml::de::Error| crate::die(&format!("[fmt]: {}", e.message())))
}

/// `plain fmt [paths...] [--check]`: what forge fmt formats (src, test and script, or the given files and
/// folders, minus `[fmt] ignore` and the libs), in place, or with --check only listed, exiting 1 if any differ.
pub fn run(args: &[String], check: bool) {
    let p = crate::project::Project::load();
    let config = Arc::new(load_config(&p.root));
    let cwd = std::env::current_dir().unwrap();
    if args.len() == 1 && args[0] == "-" {
        let mut src = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut src).unwrap_or_else(|e| crate::die(&e.to_string()));
        let out = format(&src, &config).unwrap_or_else(|e| crate::die(&format!("stdin: {e}")));
        print!("{out}");
        if check && out != src {
            std::process::exit(1);
        }
        return;
    }
    // ponytail: ignore globs use --match-path's matcher, where `*` also crosses `/` (forge's glob crate: only `**`)
    let ignored = |rel: &str| config.ignore.iter().any(|g| crate::test::glob_matches(g.strip_prefix("./").unwrap_or(g), rel));
    let rel = |path: &Path| {
        let abs = cwd.join(path);
        abs.strip_prefix(&p.root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| abs.to_string_lossy().into())
    };
    let libs: Vec<PathBuf> = p.libs.iter().map(|l| p.root.join(l)).collect();
    let mut files: Vec<PathBuf> = vec![];
    let walk_dir = |dir: &Path, skip_libs: bool, files: &mut Vec<PathBuf>| {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let path = e.path();
                if skip_libs && libs.iter().any(|l| path.starts_with(l)) {
                    continue;
                }
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "sol") && !ignored(&rel(&path)) {
                    files.push(path);
                }
            }
        }
    };
    if args.is_empty() {
        for d in &p.dirs {
            walk_dir(&p.root.join(d), true, &mut files);
        }
    } else {
        for a in args {
            let path = PathBuf::from(a);
            if path.is_dir() {
                let in_lib = libs.iter().any(|l| cwd.join(&path).starts_with(l));
                walk_dir(&path, !in_lib, &mut files);
            } else if path.extension().is_some_and(|x| x == "sol") {
                files.push(path); // a file named explicitly is formatted even when ignored, as in forge
            } else if !path.exists() {
                crate::die(&format!("{a}: no such file or folder"));
            } else {
                eprintln!("warning: {a}: not a .sol file, skipped");
            }
        }
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        eprintln!("nothing to format");
        return;
    }
    // biggest first, so the longest file doesn't start last; a session per file keeps one file's errors its own
    files.sort_by_key(|f| std::cmp::Reverse(std::fs::metadata(f).map(|m| m.len()).unwrap_or(0)));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(files.len());
    let results: Vec<(PathBuf, Result<Option<String>, String>)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut out = vec![];
                    while let Some(f) = files.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                        let r = std::fs::read_to_string(f).map_err(|e| e.to_string()).and_then(|src| {
                            let sess = session();
                            let fmt = sess.enter_sequential(|| format_in(&sess, FileName::Real(f.clone()), src.clone(), config.clone()))?;
                            Ok((fmt != src).then_some(fmt))
                        });
                        out.push((f.clone(), r));
                    }
                    out
                })
            })
            .collect();
        let mut all: Vec<_> = workers.into_iter().flat_map(|w| w.join().unwrap()).collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        all
    });
    let (mut changed, mut failed) = (0, false);
    for (f, r) in results {
        let name = rel(&f);
        match r {
            Err(e) => {
                failed = true;
                eprintln!("{}", e.trim_end());
            }
            Ok(None) => {}
            Ok(Some(out)) if check => {
                changed += 1;
                let src = std::fs::read_to_string(&f).unwrap_or_default();
                let line = src.lines().zip(out.lines()).position(|(a, b)| a != b).unwrap_or_else(|| src.lines().count().min(out.lines().count()));
                println!("{name}:{}: not formatted", line + 1);
            }
            Ok(Some(out)) => {
                changed += 1;
                std::fs::write(&f, out).unwrap_or_else(|e| crate::die(&format!("{name}: {e}")));
                println!("formatted {name}");
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    if check && changed > 0 {
        eprintln!("{changed} of {} files need formatting (run plain fmt)", files.len());
        std::process::exit(1);
    }
}
