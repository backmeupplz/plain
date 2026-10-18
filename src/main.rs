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

const USAGE: &str = "plain: compile, test and deploy EVM contracts

usage:
  plain                                      in a terminal: a menu for everything below
  plain init                                 scaffold src/Counter.sol + test/Counter.t.sol
  plain build [--force]                      compile src/, test/, script/ (solc auto-selected from pragmas)
  plain test [filter] [-v] [--fork net]      run *.t.sol tests (forge-std compatible) and test/**/*.test.ts;
             [--fuzz-runs N] [--seed N]      unchanged suites reuse their results (--rerun: run them anyway,
             [--rerun] [--force] [--watch]   --force: also recompile, --watch: again on every save)
  plain deploy <Contract|script.s.sol> [args] [--network name|id|url] [--account name] [--broadcast]
             [--proxy] [--no-verify]         simulate, then send with --broadcast and verify on Sourcify,
                                             Blockscout and Etherscan; --proxy: behind an OpenZeppelin proxy,
                                             args going to initialize()
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
          Etherscan; the project's .env is read too
";

pub fn init() {
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
    println!("next: plain test");
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
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() && ui::tty() {
        return menu::run();
    }
    let cmd = if args.is_empty() { "help".to_string() } else { args.remove(0) };
    let t = Instant::now();
    match cmd.as_str() {
        "build" => {
            let p = project::Project::load();
            let out = solc::build(&p, switch(&mut args, &["--force"]), solc::Scope::All);
            eprintln!("compiled {} files with solc {} in {:.2?} ({} contracts)", out.compiled, out.solc, t.elapsed(), out.artifacts.len());
        }
        "test" => {
            let p = project::Project::load();
            if switch(&mut args, &["--watch", "-w"]) {
                watch(&p, &args);
            }
            let verbose = switch(&mut args, &["-v", "--verbose"]);
            let force = switch(&mut args, &["--force"]);
            let rerun = switch(&mut args, &["--rerun"]);
            let fork = flag(&mut args, "--fork").map(|f| rpc::network(&p, &f).unwrap_or_else(|e| die(&e)).url);
            let fuzz_runs = flag(&mut args, "--fuzz-runs").map(|v| v.parse().unwrap_or_else(|_| die("--fuzz-runs N"))).unwrap_or(p.fuzz_runs);
            let seed = flag(&mut args, "--seed").map(|v| v.parse().unwrap_or_else(|_| die("--seed N"))).unwrap_or(0x5eed);
            let filter = args.first().cloned();
            let ts_files = ts::files(&p.root, &p.dirs[1], filter.as_deref());
            // a filter compiles only the test files it can match; TypeScript tests need every artifact
            let scope = solc::Scope::Tests(filter.clone().filter(|_| ts_files.is_empty()));
            let out = solc::build(&p, force, scope);
            if out.compiled > 0 {
                eprintln!("compiled {} files in {:.2?}", out.compiled, t.elapsed());
            }
            let has_sol = out.artifacts.iter().any(test::is_test_contract);
            let mut ok = true;
            if has_sol || ts_files.is_empty() {
                let opts = test::Opts { filter: filter.clone(), verbose, fuzz_runs, fork: fork.clone(), seed, allow_internal_revert: p.allow_internal_revert, invariant: p.invariant, quiet_if_empty: !ts_files.is_empty(), cache: !force && !rerun && fork.is_none() };
                ok &= test::run(out.artifacts, &p.root, &opts);
            }
            if !ts_files.is_empty() {
                ok &= ts::run(&p.root, &ts_files, verbose, fork);
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
            deploy::run(&p, out, conn, deploy::Opts { target, args, broadcast, verify, account, proxy, upgrade: None });
        }
        "upgrade" => {
            let p = project::Project::load();
            let network = flag(&mut args, "--network").unwrap_or_else(|| die("plain upgrade needs --network"));
            let account = flag(&mut args, "--account");
            let call = flag(&mut args, "--call");
            let broadcast = switch(&mut args, &["--broadcast"]);
            let verify = !switch(&mut args, &["--no-verify"]);
            let unchecked = switch(&mut args, &["--unsafe-skip-storage-check"]);
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
            deploy::run(&p, out, Some(conn), deploy::Opts { target: plan.target.clone(), args, broadcast, verify, account, proxy: false, upgrade: Some(plan) });
        }
        "verify" => {
            let p = project::Project::load();
            let export = switch(&mut args, &["--export"]);
            let network = flag(&mut args, "--network");
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
                die(&format!("no deploy recorded on chain {chain} matches {}", args.join(" ")));
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
        "init" => init(),
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
