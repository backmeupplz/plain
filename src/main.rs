mod decode;
mod deploy;
mod evm;
mod node;
mod project;
mod rpc;
mod solc;
mod stdlib;
mod test;
mod ts;

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
  plain build [--force]                      compile src/, test/, script/ (solc auto-selected from pragmas)
  plain test [filter] [-v] [--fork net]      run *.t.sol tests (forge-std compatible) and test/**/*.test.ts
             [--fuzz-runs N] [--seed N]
  plain deploy <Contract|script.s.sol> [args] [--network name|url] [--broadcast]
                                             simulate, then send with --broadcast (key: $PRIVATE_KEY)
  plain node [--port 8545] [--fork net]      local dev chain (anvil/hardhat compatible JSON-RPC)
  plain init                                 scaffold src/Counter.sol + test/Counter.t.sol
  plain <name> [args]                        run plugin `plain-<name>` from PATH

networks: mainnet sepolia base base-sepolia optimism optimism-sepolia arbitrum arbitrum-sepolia
          polygon polygon-amoy bsc avalanche gnosis local, or any URL; add more in plain.toml [networks]
";

fn init() {
    let files = [
        ("src/Counter.sol", "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.24;\n\ncontract Counter {\n    uint256 public number;\n\n    function increment() external {\n        number++;\n    }\n}\n"),
        ("test/Counter.t.sol", "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.24;\n\nimport {Test} from \"forge-std/Test.sol\";\nimport {Counter} from \"../src/Counter.sol\";\n\ncontract CounterTest is Test {\n    Counter counter;\n\n    function setUp() public {\n        counter = new Counter();\n    }\n\n    function test_Increment() public {\n        counter.increment();\n        assertEq(counter.number(), 1);\n    }\n}\n"),
        (".gitignore", ".plain/\nnode_modules/\n"),
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

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = if args.is_empty() { "help".to_string() } else { args.remove(0) };
    let t = Instant::now();
    match cmd.as_str() {
        "build" => {
            let p = project::Project::load();
            let out = solc::build(&p, switch(&mut args, &["--force"]));
            eprintln!("compiled {} files with solc {} in {:.2?} ({} contracts)", out.compiled, out.solc, t.elapsed(), out.artifacts.len());
        }
        "test" => {
            let p = project::Project::load();
            let verbose = switch(&mut args, &["-v", "--verbose"]);
            let force = switch(&mut args, &["--force"]);
            let fork = flag(&mut args, "--fork").map(|f| rpc::network_url(&p, &f).unwrap_or_else(|| die(&format!("unknown network {f}"))));
            let fuzz_runs = flag(&mut args, "--fuzz-runs").map(|v| v.parse().unwrap_or_else(|_| die("--fuzz-runs N"))).unwrap_or(p.fuzz_runs);
            let seed = flag(&mut args, "--seed").map(|v| v.parse().unwrap_or_else(|_| die("--seed N"))).unwrap_or(0x5eed);
            let out = solc::build(&p, force);
            if out.compiled > 0 {
                eprintln!("compiled {} files in {:.2?}", out.compiled, t.elapsed());
            }
            let filter = args.first().cloned();
            let ts_files = ts::files(&p.root, &p.dirs[1], filter.as_deref());
            let has_sol = out.artifacts.iter().any(test::is_test_contract);
            let mut ok = true;
            if has_sol || ts_files.is_empty() {
                let opts = test::Opts { filter: filter.clone(), verbose, fuzz_runs, fork: fork.clone(), seed, allow_internal_revert: p.allow_internal_revert, invariant: p.invariant, quiet_if_empty: !ts_files.is_empty() };
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
            let broadcast = switch(&mut args, &["--broadcast"]);
            if args.is_empty() {
                die("usage: plain deploy <Contract|script.s.sol> [constructor args...] [--network name|url] [--broadcast]");
            }
            let out = solc::build(&p, false);
            let target = args.remove(0);
            deploy::run(&p, out.artifacts, deploy::Opts { target, args, network, broadcast });
        }
        "node" => {
            let p = project::Project::load();
            let port = flag(&mut args, "--port").map(|v| v.parse().unwrap_or_else(|_| die("--port N"))).unwrap_or(8545);
            let fork = flag(&mut args, "--fork").map(|f| rpc::network_url(&p, &f).unwrap_or_else(|| die(&format!("unknown network {f}"))));
            let (port, _chain) = node::start(fork, port, false);
            println!("plain node on http://127.0.0.1:{port} (chain 31337); accounts are anvil/hardhat defaults, e.g.");
            println!("  0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 key 0x{}", node::KEYS[0]);
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
