// Runs the binary on tests/fixture: every testBad*, invariantBad* and testFail* (gone since forge 1.0) must fail,
// everything else pass.
use std::io::{BufRead, Read, Write};
use std::process::Command;

const CREATE2_DEPLOYER: &str = "0x4e59b44847b379578588920ca78fbf26c0b4956c";
const CREATE2_DEPLOYER_CODE: &str = "0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3";

/// A JSON-RPC chain for test/Forge.t.sol's fork test: chain 8453 at block 100, where 0x…beef's code returns its
/// slot 0, which holds 42.
fn mock_rpc() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = std::io::BufReader::new(s);
            let mut len = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let answer = |c: &serde_json::Value| {
                let p = &c["params"];
                let beef = p[0].as_str().is_some_and(|a| a.to_lowercase() == "0x000000000000000000000000000000000000beef");
                let word = |n: u8| format!("0x{:064x}", n);
                let result = match c["method"].as_str().unwrap() {
                    "eth_chainId" => serde_json::json!("0x2105"),
                    "eth_blockNumber" => serde_json::json!("0x64"),
                    "eth_getBlockByNumber" => serde_json::json!({"number": "0x64", "timestamp": "0x6553f100", "baseFeePerGas": "0x7", "gasLimit": "0x1c9c380", "miner": "0x4200000000000000000000000000000000000011", "mixHash": word(1), "hash": word(2)}),
                    "eth_getBalance" | "eth_getTransactionCount" => serde_json::json!("0x0"),
                    "eth_gasPrice" | "eth_maxPriorityFeePerGas" => serde_json::json!("0x1"),
                    // PUSH0 SLOAD PUSH0 MSTORE PUSH1 32 PUSH0 RETURN; and the CREATE2 deployer, as on Base
                    "eth_getCode" if beef => serde_json::json!("0x5f545f5260205ff3"),
                    "eth_getCode" if p[0].as_str().is_some_and(|a| a.eq_ignore_ascii_case(CREATE2_DEPLOYER)) => serde_json::json!(CREATE2_DEPLOYER_CODE),
                    "eth_getCode" => serde_json::json!("0x"),
                    "eth_getStorageAt" => serde_json::json!(word(if beef { 42 } else { 0 })),
                    m => panic!("mock RPC: {m}"),
                };
                serde_json::json!({"jsonrpc": "2.0", "id": c["id"], "result": result})
            };
            let out = match req.as_array() {
                Some(calls) => serde_json::Value::Array(calls.iter().map(answer).collect()),
                None => answer(&req),
            }
            .to_string();
            let mut s = r.into_inner();
            write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}", out.len()).unwrap();
        }
    });
    url
}

fn plain(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_plain")).args(args).current_dir("tests/fixture").output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
fn fixture() {
    std::fs::create_dir_all("tests/fixture/tmp").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_plain")).arg("test").arg("-v").env("PLAIN_FIXTURE_RPC", mock_rpc()).current_dir("tests/fixture").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines().filter(|l| l.trim_start().starts_with("pass ") || l.trim_start().starts_with("FAIL ")) {
        let failed = line.trim_start().starts_with("FAIL");
        let should_fail = line.contains("testBad") || line.contains("testFail") || line.contains("invariantBad");
        assert_eq!(failed, should_fail, "{line}\n{text}");
    }
    assert!(text.contains("27 passed, 10 failed"), "{text}");
    assert!(text.contains("Breakable.poke(7)"), "{text}");
    assert!(text.contains("panic: arithmetic underflow"), "{text}");
    assert!(text.contains("log: value is 99"), "{text}");
    // viem-based TS test against the in-process chain (needs `bun install` in tests/fixture)
    assert!(text.contains("1 ts files passed, 0 failed"), "{text}");
}

#[test]
fn forge_options() {
    // forge's filters together, and --isolate for every test: a test that needs calls to share a transaction fails
    let (ok, text) = plain(&["test", "--match-path", "test/{Forge,Rewrite}.t.sol", "--match-contract", "Forge", "--match-test", "Share", "--isolate", "--gas-limit", "2000000000", "--threads", "1", "--rerun"]);
    assert!(!ok && text.contains("0 passed, 1 failed") && text.contains("warm and dirty"), "{text}");
    let (ok, text) = plain(&["test", "--match-path", "Forge.t.sol", "--match-test", "isolated", "--rerun"]);
    assert!(ok && text.contains("1 passed, 0 failed"), "{text}");
    let (ok, text) = plain(&["test", "--match-test", "nothing_by_this_name"]);
    assert!(!ok && text.contains("no test matches"), "{text}");
}

#[test]
fn build_outputs() {
    std::fs::create_dir_all("tests/fixture/tmp").unwrap();
    // forge's --sizes table and EIP-170 check, its artifact layout with storage layouts, and inspect
    let (ok, text) = plain(&["build", "src/Counter.sol", "--sizes", "--skip", "test", "--out", "tmp/out", "--extra-output", "storageLayout"]);
    assert!(ok && text.contains("Runtime Size (B)") && text.lines().any(|l| l.starts_with("Counter ")), "{text}");
    let art: serde_json::Value = serde_json::from_slice(&std::fs::read("tests/fixture/tmp/out/Counter.sol/Counter.json").unwrap()).unwrap();
    assert!(art["deployedBytecode"]["object"].as_str().unwrap().starts_with("0x60"), "{art}");
    assert_eq!(art["storageLayout"]["storage"][0]["label"], "number", "{art}");
    let (ok, text) = plain(&["inspect", "Counter", "storage-layout", "--json"]);
    let layout: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert!(ok && layout["storage"][1]["label"] == "owner", "{text}");
}

#[test]
fn forge_out_with_profiles() {
    // forge's out/ when a test compiles with another profile: what it imports (forge-std included) is built with
    // that profile too, and its src contracts are written as <Contract>.<profile>.json
    let dir = "tests/fixture/tmp/profiles";
    std::fs::create_dir_all(format!("{dir}/src")).unwrap();
    std::fs::create_dir_all(format!("{dir}/test")).unwrap();
    std::fs::copy("tests/fixture/src/Counter.sol", format!("{dir}/src/Counter.sol")).unwrap();
    std::fs::write(format!("{dir}/foundry.toml"), "[profile.default]\noptimizer = true\nadditional_compiler_profiles = [{ name = \"tests\", optimizer = false, optimizer_runs = 0 }]\ncompilation_restrictions = [{ paths = \"test/Slow.t.sol\", max_optimizer_runs = 0 }]\n").unwrap();
    std::fs::write(format!("{dir}/test/Slow.t.sol"), "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\nimport {Test} from \"forge-std/Test.sol\";\nimport {Counter} from \"../src/Counter.sol\";\ncontract SlowTest is Test {\n    function test_counter() public {\n        new Counter();\n    }\n}\n").unwrap();
    std::fs::remove_dir_all(format!("{dir}/out")).ok();
    let out = Command::new(env!("CARGO_BIN_EXE_plain")).args(["build", "--out", "out"]).current_dir(dir).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{text}");
    // a file's own profile keeps the plain name; a lib/ file (forge-std here) has none
    for f in ["Counter.sol/Counter.json", "Counter.sol/Counter.tests.json", "Slow.t.sol/SlowTest.json", "Test.sol/Test.default.json", "Test.sol/Test.tests.json"] {
        assert!(std::path::Path::new(&format!("{dir}/out/{f}")).exists(), "no out/{f}: {text}");
    }
    assert!(!std::path::Path::new(&format!("{dir}/out/Slow.t.sol/SlowTest.tests.json")).exists());
}

#[test]
fn storage_layouts_like_forge() {
    // forge's storage layouts: each type's fields in forge's order, and `types` {} rather than null
    let dir = "tests/fixture/tmp/layouts";
    std::fs::remove_dir_all(dir).ok();
    std::fs::create_dir_all(format!("{dir}/src")).unwrap();
    let write = |f: &str, body: &str| std::fs::write(format!("{dir}/{f}"), format!("// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\n{body}\n")).unwrap();
    write("src/A.sol", "contract A {\n    struct Slot {\n        uint128 a;\n        uint128 b;\n    }\n    mapping(address => Slot) public slots;\n    uint256[] public list;\n}");
    write("src/Empty.sol", "contract Empty {}");
    std::fs::write(format!("{dir}/plain.toml"), "").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_plain")).args(["build", "--out", "out", "--extra-output", "storageLayout"]).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let layout = |f: &str| -> serde_json::Value { serde_json::from_slice::<serde_json::Value>(&std::fs::read(format!("{dir}/out/{f}")).unwrap()).unwrap()["storageLayout"].clone() };
    assert_eq!(layout("Empty.sol/Empty.json").to_string(), r#"{"storage":[],"types":{}}"#);
    let a = layout("A.sol/A.json");
    let keys: Vec<Vec<&str>> = a["types"].as_object().unwrap().values().map(|t| t.as_object().unwrap().keys().map(|k| k.as_str()).collect()).collect();
    for want in [vec!["encoding", "key", "label", "numberOfBytes", "value"], vec!["encoding", "label", "numberOfBytes", "members"], vec!["encoding", "label", "numberOfBytes", "base"]] {
        assert!(keys.contains(&want), "{want:?} not in {keys:?}");
    }
}

#[test]
fn script_journal() {
    // forge script's flags; the simulation is also written as forge's dry-run journal, at the nonces the chain has
    let dir = "tests/fixture/tmp/script";
    std::fs::create_dir_all(format!("{dir}/src")).unwrap();
    std::fs::create_dir_all(format!("{dir}/script")).unwrap();
    std::fs::copy("tests/fixture/src/Counter.sol", format!("{dir}/src/Counter.sol")).unwrap();
    std::fs::write(format!("{dir}/plain.toml"), "").unwrap();
    std::fs::write(format!("{dir}/script/Deploy.s.sol"), "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\nimport {Script} from \"forge-std/Script.sol\";\nimport {Counter} from \"../src/Counter.sol\";\ncontract Deploy is Script {\n    function run() external {\n        vm.startBroadcast(address(0xc2ae));\n        new Counter().increment();\n        vm.stopBroadcast();\n    }\n}\n").unwrap();
    std::fs::remove_dir_all(format!("{dir}/broadcast")).ok();
    let rpc = mock_rpc();
    let script = |chain: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_plain")).args(["script", "script/Deploy.s.sol:Deploy", "--rpc-url", &rpc, "--chain", chain]).current_dir(dir).output().unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    };
    let (ok, text) = script("1");
    assert!(!ok && text.contains("--chain 1, but"), "{text}");
    let (ok, text) = script("8453");
    assert!(ok && text.contains("plan written to broadcast/Deploy.s.sol/8453/dry-run/run-latest.json"), "{text}");
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(format!("{dir}/broadcast/Deploy.s.sol/8453/dry-run/run-latest.json")).unwrap()).unwrap();
    let txs = j["transactions"].as_array().unwrap();
    // the mock chain has every nonce at 0: the create is nonce 0, so the counter lands at 0xc2ae's first address
    let counter = "0xbff07e966fed2f2dfac9a5a6f8daf9682e625ae5";
    let sender = "0x000000000000000000000000000000000000c2ae";
    assert_eq!(txs.len(), 2, "{j}");
    assert_eq!((txs[0]["transactionType"].as_str(), txs[0]["contractName"].as_str(), txs[0]["transaction"]["to"].is_null()), (Some("CREATE"), Some("Counter"), true), "{j}");
    assert_eq!((txs[0]["transaction"]["from"].as_str(), txs[0]["transaction"]["nonce"].as_str(), txs[0]["transaction"]["chainId"].as_str()), (Some(sender), Some("0x0"), Some("0x2105")), "{j}");
    assert_eq!(txs[0]["contractAddress"], counter, "{j}");
    assert_eq!((txs[1]["transactionType"].as_str(), txs[1]["function"].as_str(), txs[1]["transaction"]["to"].as_str()), (Some("CALL"), Some("increment()"), Some(counter)), "{j}");
    assert_eq!((txs[1]["transaction"]["input"].as_str(), txs[1]["transaction"]["nonce"].as_str()), (Some("0xd09de08a"), Some("0x1")), "{j}");
}

#[test]
fn script_libraries_create2() {
    // as forge script links them on a chain with the CREATE2 deployer: at their CREATE2 addresses (salt 0), deployed
    // through it by the one account that creates contracts in the script (here a keyless vm.startBroadcast(address))
    let dir = "tests/fixture/tmp/script-libraries";
    std::fs::remove_dir_all(dir).ok();
    std::fs::create_dir_all(format!("{dir}/src")).unwrap();
    std::fs::create_dir_all(format!("{dir}/script")).unwrap();
    std::fs::copy("tests/fixture/src/Lib.sol", format!("{dir}/src/Lib.sol")).unwrap();
    std::fs::write(format!("{dir}/plain.toml"), "").unwrap();
    std::fs::write(format!("{dir}/script/UseLib.s.sol"), "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\nimport {Script} from \"forge-std/Script.sol\";\nimport {UsesLib} from \"../src/Lib.sol\";\ncontract UseLib is Script {\n    function run() external {\n        vm.startBroadcast(address(0xc2ae));\n        new UsesLib();\n        vm.stopBroadcast();\n    }\n}\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_plain")).args(["script", "script/UseLib.s.sol", "--rpc-url", &mock_rpc()]).current_dir(dir).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{text}");
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(format!("{dir}/broadcast/UseLib.s.sol/8453/dry-run/run-latest.json")).unwrap()).unwrap();
    let txs = j["transactions"].as_array().unwrap();
    assert_eq!(txs.len(), 2, "{text}");
    let (lib, uses) = (&txs[0], &txs[1]);
    let sender = "0x000000000000000000000000000000000000c2ae";
    assert_eq!((lib["transactionType"].as_str(), lib["contractName"].as_str(), lib["transaction"]["to"].as_str()), (Some("CREATE2"), Some("Adder"), Some(CREATE2_DEPLOYER)), "{j}");
    assert_eq!((lib["transaction"]["from"].as_str(), lib["transaction"]["nonce"].as_str()), (Some(sender), Some("0x0")), "{j}");
    // salt 0, then the library's code, which decides where it lands
    let input = lib["transaction"]["input"].as_str().unwrap();
    let code = hex::decode(input.strip_prefix(&format!("0x{}", "0".repeat(64))).unwrap()).unwrap();
    let at: alloy_primitives::Address = CREATE2_DEPLOYER.parse::<alloy_primitives::Address>().unwrap().create2_from_code(alloy_primitives::B256::ZERO, &code);
    assert_eq!(lib["contractAddress"].as_str(), Some(format!("{at:#x}").as_str()), "{j}");
    assert_eq!((uses["transactionType"].as_str(), uses["contractName"].as_str(), uses["transaction"]["nonce"].as_str()), (Some("CREATE"), Some("UsesLib"), Some("0x1")), "{j}");
    assert!(uses["transaction"]["input"].as_str().unwrap().contains(&format!("{at:x}")), "UsesLib isn't linked against {at}: {j}");
}

#[test]
fn configured_libraries() {
    // a library the config says is on chain already is linked against, not deployed again
    let dir = "tests/fixture/tmp/libraries";
    std::fs::create_dir_all(format!("{dir}/src")).unwrap();
    std::fs::copy("tests/fixture/src/Lib.sol", format!("{dir}/src/Lib.sol")).unwrap();
    let deploy = |config: &str| {
        std::fs::write(format!("{dir}/plain.toml"), config).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_plain")).args(["deploy", "UsesLib"]).current_dir(dir).output().unwrap();
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    };
    let text = deploy("");
    assert!(text.contains("1. create Adder") && text.contains("2. create UsesLib"), "{text}");
    let text = deploy(r#"libraries = ["src/Lib.sol:Adder:0x00000000000000000000000000000000000000Ad"]"#);
    assert!(text.contains("1. create UsesLib") && !text.contains("Adder"), "{text}");
}
