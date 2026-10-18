mod account;
mod decode;
mod deploy;
mod evm;
mod menu;
mod node;
mod project;
mod rewrite;
mod rpc;
mod solc;
mod stdlib;
mod test;
mod ts;
mod ui;
mod upgrade;
mod verify;

use std::time::Instant;

pub fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1)
}

/// Value of `--name value`, removing both from args.
fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}

fn switch(args: &mut Vec<String>, names: &[&str]) -> bool {
    let n = args.len();
    args.retain(|a| !names.contains(&a.as_str()));
    n != args.len()
}

/// What's left after the known options must not be an option: a mistyped or unsupported flag would otherwise
/// pass for an argument, or be dropped, and change what runs.
fn no_options(args: &[String]) {
    if let Some(a) = args.iter().find(|a| a.starts_with('-') && a.len() > 1 && a.parse::<f64>().is_err()) {
        let hint = match a.as_str() {
            "--rpc-url" | "-r" => " (plain takes --network <name|chain id|url>)",
            "--private-key" | "--mnemonic" | "--keystore" | "--ledger" | "--trezor" => " (plain signs with --account <name> or $PRIVATE_KEY)",
            "--sig" | "-s" => " (plain runs a script's run(), which takes no arguments)",
            "--verify" => " (plain verifies after every broadcast; --no-verify turns it off)",
            "--gas-report" | "--match-path" | "--mp" | "--no-match-test" | "--nmt" | "--no-match-contract" | "--nmc" | "--no-match-path" | "--nmp" => " (not supported yet)",
            _ => "",
        };
        die(&format!("unknown option {a}{hint}; see plain help"));
    }
}

const USAGE: &str = "plain: compile, test and deploy EVM contracts

usage:
  plain                                      in a terminal: a menu for everything below
  plain init [dir]                           scaffold src/Counter.sol + test/Counter.t.sol (in dir, if given)
  plain build [--force] [--check]            compile src/, test/, script/ (solc auto-selected from pragmas);
                                             --check: confirm each contract matches solc's own compile
  plain test [filter] [-v] [--fork net]      run *.t.sol tests (forge-std compatible) and test/**/*.test.ts;
             [--fuzz-runs N] [--seed N]      unchanged suites reuse their results (--rerun: run them anyway,
             [--rerun] [--force] [--watch]   --force: also recompile, --watch: again on every save)
  plain deploy <Contract|script.s.sol> [args] [--network name|id|url] [--account name] [--broadcast]
             [--confirm] [--proxy] [--no-verify]
                                             simulate, then send with --broadcast (--confirm: ask first) and
                                             verify on Sourcify, Blockscout and Etherscan; --proxy: behind an
                                             OpenZeppelin proxy, args going to initialize()
  plain upgrade <proxy> [Contract] [--call fn args...] --network net [--account name] [--broadcast]
                                             check the storage layout, then point the proxy at a new
                                             implementation (default: its contract as the source is now)
  plain verify [--network net] [name|address...] [--export]
                                             verify (again) what plain deployed; --export writes the files to
                                             verify by hand to verify/<network>/
  plain account [new|import <name>]          deployer keys, encrypted with a password (~/.plain/keystores;
                                             Foundry's keystores work too)
  plain node [--port 8545] [--fork net]      local dev chain (anvil/hardhat compatible JSON-RPC)
  plain <name> [args]                        run plugin `plain-<name>` from PATH

networks: any chain on chainlist.org by name or id (base, base-sepolia, arbitrum, linea, 8453, ...),
          local, or a URL; $<NAME>_RPC_URL or plain.toml [networks] picks the RPC
keys:     --account, or $PRIVATE_KEY (local chains use their first dev account); $ETHERSCAN_API_KEY for
          Etherscan; the project's .env is read for these, never exported to other programs
outside the EVM: vm.ffi needs ffi = true, file cheatcodes need fs_permissions (as in forge), and
          test/**/*.test.ts run only with typescript = true (they're programs with your permissions)
";

/// Scaffolds a project in `dir` (created when missing), else in the current directory. Existing files are kept.
pub fn init(dir: Option<&str>) {
    if let Some(d) = dir {
        std::fs::create_dir_all(d).and_then(|_| std::env::set_current_dir(d)).unwrap_or_else(|e| die(&format!("{d}: {e}")));
    }
    let files = [
        ("src/Counter.sol", "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.24;\n\ncontract Counter {\n    uint256 public number;\n\n    function increment() external {\n        number++;\n    }\n}\n"),
        ("test/Counter.t.sol", "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.24;\n\nimport {Test} from \"forge-std/Test.sol\";\nimport {Counter} from \"../src/Counter.sol\";\n\ncontract CounterTest is Test {\n    Counter counter;\n\n    function setUp() public {\n        counter = new Counter();\n    }\n\n    function test_Increment() public {\n        counter.increment();\n        assertEq(counter.number(), 1);\n    }\n}\n"),
        (".gitignore", ".plain/\nnode_modules/\n.env\n"),
    ];
    for (path, body) in files {
        if std::path::Path::new(path).exists() {
            println!("skip {path} (exists)");
            continue;
        }
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).ok();
        std::fs::write(path, body).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        println!("wrote {path}");
    }
    match dir {
        Some(d) => println!("next: cd {d} && plain test"),
        None => println!("next: plain test"),
    }
}

/// `plain test --watch`: the tests, then again whenever something under src/, test/ or script/ or the config
/// changes. Each run is a child process, so a compile error doesn't end the watch.
fn watch(p: &project::Project, args: &[String]) -> ! {
    use std::hash::{Hash, Hasher};
    let stamp = || {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let mut stack: Vec<_> = p.dirs.iter().map(|d| p.root.join(d)).collect();
        stack.extend(["foundry.toml", "plain.toml", "remappings.txt"].map(|f| p.root.join(f)));
        while let Some(path) = stack.pop() {
            let Ok(m) = std::fs::metadata(&path) else { continue };
            if m.is_dir() {
                let names = std::fs::read_dir(&path).into_iter().flatten().flatten().filter(|e| !e.file_name().to_string_lossy().starts_with('.'));
                stack.extend(names.map(|e| e.path()));
            } else {
                (&path, m.len(), m.modified().ok()).hash(&mut h);
            }
        }
        h.finish()
    };
    let exe = std::env::current_exe().unwrap_or_else(|e| die(&e.to_string()));
    let clear = ui::tty() && std::env::var_os("NO_COLOR").is_none();
    loop {
        let before = stamp();
        if clear {
            eprint!("\x1b[2J\x1b[H");
        }
        let ok = std::process::Command::new(&exe).arg("test").args(args).status().is_ok_and(|s| s.success());
        let dirs = p.dirs.iter().map(|d| format!("{d}/")).collect::<Vec<_>>().join(", ");
        let mark = if ok { ui::green("✔") } else { ui::red("✘") };
        eprintln!("\n{mark} {}", ui::dim(&format!("watching {dirs} · ctrl-c to stop")));
        // a save during the run counts too: the stamp is from before it
        while stamp() == before {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        // let the editor finish writing
        std::thread::sleep(std::time::Duration::from_millis(80));
    }
}

fn main() {
    project::load_env();
    // --flag=value is --flag value
    let mut args: Vec<String> = std::env::args().skip(1).flat_map(|a| match a.split_once('=').filter(|_| a.starts_with("--")) {
        Some((k, v)) => vec![k.to_string(), v.to_string()],
        None => vec![a],
    }).collect();
    if args.is_empty() && ui::tty() {
        return menu::run();
    }
    let cmd = if args.is_empty() { "help".to_string() } else { args.remove(0) };
    let t = Instant::now();
    match cmd.as_str() {
        "build" => {
            let p = project::Project::load();
            let check = switch(&mut args, &["--check"]);
            let force = switch(&mut args, &["--force"]);
            no_options(&args);
            let out = solc::build(&p, force, solc::Scope::All);
            eprintln!("compiled {} files with solc {} in {:.2?} ({} contracts)", out.compiled, out.solc, t.elapsed(), out.artifacts.len());
            if check {
                let t = Instant::now();
                let bad = solc::check(&p, &out);
                for b in &bad {
                    eprintln!("MISMATCH {b}");
                }
                if !bad.is_empty() {
                    die(&format!("{} contract(s) differ from solc's own compile of their sources", bad.len()));
                }
                eprintln!("checked: every deployable contract is byte-identical to solc's compile of its standard JSON input ({:.2?})", t.elapsed());
            }
        }
        "test" => {
            let p = project::Project::load();
            if switch(&mut args, &["--watch", "-w"]) {
                watch(&p, &args);
            }
            let verbose = switch(&mut args, &["-v", "-vv", "-vvv", "-vvvv", "-vvvvv", "--verbose"]);
            let force = switch(&mut args, &["--force"]);
            let rerun = switch(&mut args, &["--rerun"]);
            let fork = flag(&mut args, "--fork").or_else(|| flag(&mut args, "--fork-url")).map(|f| rpc::network(&p, &f).unwrap_or_else(|e| die(&e)).url);
            let fuzz_runs = flag(&mut args, "--fuzz-runs").map(|v| v.parse().unwrap_or_else(|_| die("--fuzz-runs N"))).unwrap_or(p.fuzz_runs);
            let seed = flag(&mut args, "--seed").or_else(|| flag(&mut args, "--fuzz-seed")).map(|v| v.parse().unwrap_or_else(|_| die("--seed N"))).unwrap_or(0x5eed);
            // forge's filters are regexes; plain's is one substring of a suite or test name, which these become
            for f in ["--match-test", "--mt", "--match-contract", "--mc"] {
                if let Some(v) = flag(&mut args, f) {
                    args.push(v);
                }
            }
            no_options(&args);
            if args.len() > 1 {
                die(&format!("plain test takes one filter, got {}", args.join(" ")));
            }
            let filter = args.first().cloned();
            let mut ts_files = ts::files(&p.root, &p.dirs[1], filter.as_deref());
            // TypeScript tests are programs with your permissions: only when the project's config asks for them
            let skipped_ts = if p.typescript { 0 } else { std::mem::take(&mut ts_files).len() };
            // a filter compiles only the test files it can match; TypeScript tests need every artifact
            let scope = solc::Scope::Tests(filter.clone().filter(|_| ts_files.is_empty()));
            let out = solc::build(&p, force, scope);
            if out.compiled > 0 {
                eprintln!("compiled {} files in {:.2?}", out.compiled, t.elapsed());
            }
            let has_sol = out.artifacts.iter().any(test::is_test_contract);
            // a filter that matches nothing is a typo, not a pass
            if let Some(f) = filter.as_deref().filter(|f| ts_files.is_empty() && !out.artifacts.iter().any(|a| test::is_test_contract(a) && test::matches(a, f))) {
                die(&format!("no test matches {f:?}"));
            }
            let mut ok = true;
            if has_sol || ts_files.is_empty() {
                let opts = test::Opts { filter: filter.clone(), verbose, fuzz_runs, fork: fork.clone(), seed, allow_internal_revert: p.allow_internal_revert, invariant: p.invariant, quiet_if_empty: !ts_files.is_empty(), cache: !force && !rerun && fork.is_none(), access: std::sync::Arc::new(p.access.clone()), spec: evm::spec(&out.evm) };
                ok &= test::run(out.artifacts, &p.root, &opts);
            }
            if !ts_files.is_empty() {
                ok &= ts::run(&p.root, &ts_files, verbose, fork);
            }

            if skipped_ts > 0 {
                eprintln!("skipped {skipped_ts} TypeScript test file(s): they run as programs on this machine, so plain runs them only with `typescript = true` in plain.toml");
                // a project whose only tests were skipped hasn't passed anything
                ok &= has_sol;
            }
            if !ok {
                std::process::exit(1);
            }
        }
        "deploy" => {
            let p = project::Project::load();
            let network = flag(&mut args, "--network");
            let account = flag(&mut args, "--account");
            let broadcast = switch(&mut args, &["--broadcast"]);
            let verify = !switch(&mut args, &["--no-verify"]);
            let proxy = switch(&mut args, &["--proxy"]);
            let confirm = switch(&mut args, &["--confirm"]);
            no_options(&args);
            if args.is_empty() {
                die("usage: plain deploy <Contract|script.s.sol> [args...] [--network name|id|url] [--account name] [--broadcast] [--proxy] [--no-verify]");
            }
            let target = args.remove(0);
            // talk to the network while solc compiles
            let (out, conn) = std::thread::scope(|s| {
                let conn = network.as_deref().map(|n| s.spawn(|| deploy::connect(&p, n)));
                let out = solc::build(&p, false, solc::Scope::Deploy(target.clone()));
                (out, conn.map(|c| c.join().unwrap()))
            });
            if broadcast && verify {
                // explorers for verification, from chainlist; fetched while the transactions go out
                if let Some(c) = conn.as_ref().map(|c| c.chain) {
                    std::thread::spawn(move || rpc::chain_info(c));
                }
            }
            deploy::run(&p, out, conn, deploy::Opts { target, args, broadcast, verify, account, proxy, upgrade: None, confirm });
        }
        "upgrade" => {
            let p = project::Project::load();
            let network = flag(&mut args, "--network").unwrap_or_else(|| die("plain upgrade needs --network"));
            let account = flag(&mut args, "--account");
            let call = flag(&mut args, "--call");
            let broadcast = switch(&mut args, &["--broadcast"]);
            let verify = !switch(&mut args, &["--no-verify"]);
            let unchecked = switch(&mut args, &["--unsafe-skip-storage-check"]);
            let confirm = switch(&mut args, &["--confirm"]);
            no_options(&args);
            if args.is_empty() {
                die("usage: plain upgrade <proxy name|address> [Contract] [--call function args...] --network net [--account name] [--broadcast]");
            }
            let proxy = args.remove(0);
            let target = (!args.is_empty()).then(|| args.remove(0));
            let conn = deploy::connect(&p, &network);
            let plan = upgrade::plan(&p, &conn, &proxy, target, call);
            let out = solc::build(&p, false, solc::Scope::Deploy(plan.target.clone()));
            if !unchecked {
                let new = out.artifacts.iter().find(|a| !a.bytecode.is_empty() && (a.name == plan.target || format!("{}:{}", a.file, a.name) == plan.target)).cloned();
                upgrade::check(&p, &plan, &new.unwrap_or_else(|| die(&format!("no deployable contract matches {:?}", plan.target))), &out.solc);
            }
            if broadcast && verify {
                let c = conn.chain;
                std::thread::spawn(move || rpc::chain_info(c));
            }
            deploy::run(&p, out, Some(conn), deploy::Opts { target: plan.target.clone(), args, broadcast, verify, account, proxy: false, upgrade: Some(plan), confirm });
        }
        "verify" => {
            let p = project::Project::load();
            let export = switch(&mut args, &["--export"]);
            let network = flag(&mut args, "--network");
            no_options(&args);
            let chain = match &network {
                Some(n) => match n.parse::<u64>().ok().or_else(|| rpc::NETWORKS.iter().find(|x| x.0 == n).map(|x| x.1)) {
                    Some(c) => c,
                    None => {
                        let net = rpc::network(&p, n).unwrap_or_else(|e| die(&e));
                        net.chain.unwrap_or_else(|| rpc::call(&net.url, "eth_chainId", serde_json::json!([])).ok().and_then(|v| u64::from_str_radix(v.as_str()?.trim_start_matches("0x"), 16).ok()).unwrap_or_else(|| die(&format!("{n}: no chain id"))))
                    }
                },
                None => match verify::chains(&p).as_slice() {
                    [c] => *c,
                    [] => die("nothing deployed from this project yet (plain deploy ... --broadcast records deploys in .plain/deploys)"),
                    many => die(&format!("deploys on several chains ({many:?}); pick one with --network")),
                },
            };
            let mut all = verify::load(&p, chain);
            let mut recs: std::collections::BTreeMap<_, _> = all.iter().filter(|(a, r)| args.is_empty() || args.iter().any(|x| x.eq_ignore_ascii_case(&a.to_string()) || *x == r.name || *x == r.contract)).map(|(a, r)| (*a, r.clone())).collect();
            if recs.is_empty() {
                match args.is_empty() {
                    true => die(&format!("nothing plain deployed is recorded for chain {chain}")),
                    false => die(&format!("no deploy recorded on chain {chain} matches {}", args.join(" "))),
                }
            }
            if export {
                let net = network.unwrap_or_else(|| rpc::name_of(chain).map_or(chain.to_string(), String::from));
                let dir = verify::export(&p, chain, &net, &recs);
                println!("wrote {} contract(s) to {}", recs.len(), dir.strip_prefix(&p.root).unwrap_or(&dir).display());
                return;
            }
            let proxies: Vec<_> = recs.iter().filter(|(_, r)| r.proxy).map(|(a, _)| *a).collect();
            let ok = verify::run(&p, chain, &mut recs);
            all.append(&mut recs);
            verify::save(&p, chain, &all);
            for a in proxies {
                verify::link_proxy(&p, chain, a);
            }
            if !ok {
                std::process::exit(1);
            }
        }
        "account" => account::cli(&args),
        "node" => {
            let p = project::Project::load();
            let port = flag(&mut args, "--port").map(|v| v.parse().unwrap_or_else(|_| die("--port N"))).unwrap_or(8545);
            let fork = flag(&mut args, "--fork").map(|f| rpc::network(&p, &f).unwrap_or_else(|e| die(&e)).url);
            let (port, _chain) = node::start(fork, port, false);
            println!("plain node on http://127.0.0.1:{port} (chain 31337); accounts are anvil/hardhat defaults, e.g.");
            println!("  0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 key 0x{}", node::KEYS[0]);
            println!("deploy to it with --network local; plain signs with the first account");
            loop {
                std::thread::park();
            }
        }
        "init" => {
            no_options(&args);
            match &args[..] {
                [] => init(None),
                [dir] => init(Some(dir.as_str())),
                _ => die("usage: plain init [dir]"),
            }
        }
        "help" | "-h" | "--help" => print!("{USAGE}"),
        "version" | "-V" | "--version" => println!("plain {}", env!("CARGO_PKG_VERSION")),
        other => {
            // plugins: `plain foo` runs `plain-foo` from PATH (git-style), with project context in env
            let bin = format!("plain-{other}");
            let p = project::Project::load();
            match std::process::Command::new(&bin).args(&args).env("PLAIN_ROOT", &p.root).env("PLAIN_ARTIFACTS", p.root.join(".plain/out")).status() {
                Ok(s) => std::process::exit(s.code().unwrap_or(1)),
                Err(_) => die(&format!("unknown command {other:?} (no {bin} on PATH)\n\n{USAGE}")),
            }
        }
    }
}
