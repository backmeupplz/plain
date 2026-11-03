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
    assert!(text.contains("29 passed, 10 failed, 4 skipped"), "{text}");
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
    // vm.skip(true) on some fuzz inputs skips the test (it used to fail with the skip marker as its reason), and
    // vm.skip(true, reason) skips with that reason in a unit test, a fuzz test and setUp
    let (ok, text) = plain(&["test", "--match-path", "Forge.t.sol", "--match-test", "skip", "-v", "--rerun"]);
    assert!(ok && text.contains("2 passed, 0 failed, 4 skipped") && text.contains("skip testFuzz_skipsSomeInputs(uint256): skipped"), "{text}");
    assert!(text.contains("pass testFuzz_skipFalseRuns(uint256)") && text.contains("pass test_skipFalseWithReasonRuns()"), "{text}");
    assert!(text.contains("skip test_skipWithReason(): needs mainnet") && text.contains("skip testFuzz_skipWithReason(uint256): even input"), "{text}");
    assert!(text.contains("skip test_skippedBySetUp(): skipped in setUp: no fork configured"), "{text}");
    assert!(!text.contains("FOUNDRY::") && !text.contains("never shown"), "{text}");
    let (ok, text) = plain(&["test", "--match-test", "nothing_by_this_name"]);
    assert!(!ok && text.contains("no test matches"), "{text}");
}

#[test]
fn invariant_skips_like_forge() {
    // forge's invariant skip check runs on the state setUp left: an invariant that vm.skip(true)s there is skipped
    // (even with nothing to fuzz) and the others still fuzz; a skip first reached mid-campaign breaks the invariant
    let dir = "tests/fixture/tmp/invariant-skip";
    std::fs::remove_dir_all(dir).ok();
    std::fs::create_dir_all(format!("{dir}/test")).unwrap();
    std::fs::write(format!("{dir}/foundry.toml"), "[profile.default]\n").unwrap();
    std::fs::write(format!("{dir}/test/Skip.t.sol"), "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\nimport {Test} from \"forge-std/Test.sol\";\ncontract Bump {\n    uint256 public n;\n    function bump() external { n += 1; }\n}\ncontract SkipTest is Test {\n    Bump b;\n    function setUp() public { b = new Bump(); }\n    function invariant_a_skips() public { vm.skip(true); }\n    function invariant_c_skips_why() public { vm.skip(true, \"no fork configured\"); }\n    function invariant_b_holds() public view { assertLe(b.n(), 1000); }\n}\ncontract NothingToFuzzSkipTest is Test {\n    function invariant_skips() public { vm.skip(true); }\n}\ncontract MidCampaignSkipTest is Test {\n    Bump b;\n    function setUp() public { b = new Bump(); }\n    function invariant_skips_later() public { vm.skip(b.n() != 0); }\n}\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_plain")).args(["test", "-v"]).current_dir(dir).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success() && text.contains("1 passed, 1 failed, 3 skipped"), "{text}");
    assert!(text.contains("skip invariant_a_skips(): skipped") && text.contains("skip invariant_skips(): skipped"), "{text}");
    assert!(text.contains("pass invariant_b_holds()") && text.contains("skip invariant_c_skips_why(): no fork configured"), "{text}");
    assert!(text.contains("FAIL invariant_skips_later(): invariant broken: FOUNDRY::SKIP"), "{text}");
}

#[test]
fn skip_reasons_like_forge() {
    // vm.skip(true, reason) skips with the reason shown (it used to fail as an unsupported cheatcode), in a test, a
    // fuzz test, setUp and an invariant's setUp-state check; checked against forge 1.7.1, where a skip first reached
    // mid-campaign breaks the invariant ("FOUNDRY::SKIP" then the reason) and a handler's skip is a reverted call,
    // which breaks it only with fail_on_revert; a revert carrying the marker that no vm.skip made, or a vm.skip in a
    // contract the test calls, fails
    let dir = "tests/fixture/tmp/skip-reasons";
    std::fs::remove_dir_all(dir).ok();
    std::fs::create_dir_all(format!("{dir}/test")).unwrap();
    std::fs::write(format!("{dir}/foundry.toml"), "[profile.default]\n").unwrap();
    std::fs::write(
        format!("{dir}/test/Reasons.t.sol"),
        r#"// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;
import {Test} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
contract Bump {
    uint256 public n;
    function bump() external { n += 1; }
}
contract SkipHandler {
    uint256 public n;
    function go() external {
        n += 1;
        if (n > 2) Vm(address(uint160(uint256(keccak256("hevm cheat code"))))).skip(true, "handler says skip");
    }
}
contract Spoof {
    function marker() external pure { assembly { mstore(0, "FOUNDRY::SKIP") revert(0, 13) } }
    function markerX() external pure { assembly { mstore(0, "FOUNDRY::SKIPx") revert(0, 14) } }
    function deep() external { Vm(address(uint160(uint256(keccak256("hevm cheat code"))))).skip(true, "deep"); }
}
contract EdgeTest is Test {
    Spoof s;
    function setUp() public { s = new Spoof(); }
    function test_spoofMarker() public view { s.marker(); }
    function test_spoofMarkerX() public view { s.markerX(); }
    function test_skipInHelper() public { s.deep(); }
    /// forge-config: default.isolate = true
    function test_isolatedSpoof() public view { s.markerX(); }
    /// forge-config: default.isolate = true
    function test_isolatedSkip() public { vm.skip(true, "iso"); }
}
contract ReasonTest is Test {
    function test_reason() public { vm.skip(true, "plain reason"); }
    function test_noReason() public { vm.skip(true); }
    function test_falseReason() public { vm.skip(false, "nope"); }
    function testFuzz_reason(uint256 x) public { vm.skip(x % 2 == 0, "even input"); }
}
contract SetUpReasonTest is Test {
    function setUp() public { vm.skip(true, "setup reason"); }
    function test_a() public {}
}
contract InvariantReasonTest is Test {
    Bump b;
    function setUp() public { b = new Bump(); }
    function invariant_pre() public { vm.skip(true, "pre reason"); }
}
contract MidCampaignReasonTest is Test {
    Bump b;
    function setUp() public { b = new Bump(); }
    function invariant_later() public { vm.skip(b.n() != 0, "later reason"); }
}
contract HandlerSkipTest is Test {
    SkipHandler h;
    function setUp() public { h = new SkipHandler(); }
    /// forge-config: default.invariant.runs = 16
    /// forge-config: default.invariant.depth = 16
    function invariant_holds() public view { assertLe(h.n(), 1000); }
}
contract HandlerSkipFailOnRevertTest is Test {
    SkipHandler h;
    function setUp() public { h = new SkipHandler(); }
    /// forge-config: default.invariant.fail-on-revert = true
    function invariant_holdsStrict() public view { assertLe(h.n(), 1000); }
}
"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_plain")).args(["test", "-v"]).current_dir(dir).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success() && text.contains("2 passed, 6 failed, 6 skipped"), "{text}");
    for line in [
        "skip test_reason(): plain reason",
        "skip test_noReason(): skipped",
        "pass test_falseReason()",
        "skip testFuzz_reason(uint256): even input",
        "skip test_a(): skipped in setUp: setup reason",
        "skip invariant_pre(): pre reason",
        "FAIL invariant_later(): invariant broken: FOUNDRY::SKIPlater reason",
        "pass invariant_holds()",
        "FAIL invariant_holdsStrict(): invariant broken: call reverted: FOUNDRY::SKIPhandler says skip",
        // a revert that merely carries the marker is no skip, nor is a skip from a contract the test calls
        "FAIL test_spoofMarker(): FOUNDRY::SKIP",
        "FAIL test_spoofMarkerX(): FOUNDRY::SKIPx",
        "FAIL test_isolatedSpoof(): FOUNDRY::SKIPx",
        "FAIL test_skipInHelper(): vm.skip: `skip` can only be used at test level",
        "skip test_isolatedSkip(): iso",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
    assert!(!text.contains("unsupported cheatcode"), "{text}");
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
    // forge's methodIdentifiers, and its ABI order (functions by name)
    assert_eq!(art["methodIdentifiers"]["number()"], "8381f58a", "{art}");
    let names: Vec<&str> = art["abi"].as_array().unwrap().iter().filter(|x| x["type"] == "function").map(|x| x["name"].as_str().unwrap()).collect();
    assert!(names.windows(2).all(|w| w[0] <= w[1]), "{names:?}");
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

#[test]
fn fmt_like_forge() {
    // forge fmt 1.7.1's output for this input with these [fmt] settings, byte for byte
    let dir = "tests/fixture/tmp/fmt";
    std::fs::create_dir_all(format!("{dir}/src")).unwrap();
    std::fs::write(format!("{dir}/foundry.toml"), "[fmt]\nline_length = 60\nint_types = \"short\"\nbracket_spacing = true\nsort_imports = true\n").unwrap();
    let input = "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\nimport {B} from \"./B.sol\";\nimport {A as Z} from './A.sol';\ncontract  Counter{\nuint256 public number;event Set(uint256 indexed value,address indexed by,uint256 when);\n    function set(uint256 n,address who,uint256 when) external { number=n; emit Set(n,who,when); }\n  // forgefmt: disable-next-line\n  function   keep( ) external {}\n}\n";
    let want = "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\nimport { A as Z } from \"./A.sol\";\nimport { B } from \"./B.sol\";\n\ncontract Counter {\n    uint public number;\n    event Set(\n        uint indexed value, address indexed by, uint when\n    );\n\n    function set(uint n, address who, uint when) external {\n        number = n;\n        emit Set(n, who, when);\n    }\n    // forgefmt: disable-next-line\n  function   keep( ) external {}\n}\n";
    std::fs::write(format!("{dir}/src/A.sol"), input).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_plain")).args(args).current_dir(dir).output().unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    };
    let (ok, text) = run(&["fmt", "--check"]);
    assert!(!ok && text.contains("src/A.sol"), "{text}");
    assert_eq!(std::fs::read_to_string(format!("{dir}/src/A.sol")).unwrap(), input, "--check wrote the file");
    let (ok, text) = run(&["fmt"]);
    assert!(ok, "{text}");
    assert_eq!(std::fs::read_to_string(format!("{dir}/src/A.sol")).unwrap(), want);
    let (ok, text) = run(&["fmt", "--check"]);
    assert!(ok, "{text}");
    // a key forge doesn't have is an error, not a silent difference
    std::fs::write(format!("{dir}/foundry.toml"), "[fmt]\nline_lenght = 60\n").unwrap();
    let (ok, text) = run(&["fmt", "--check"]);
    assert!(!ok && text.contains("line_lenght"), "{text}");
}

#[test]
fn node_like_anvil() {
    // plain node with anvil's flags: a geth-style genesis' alloc, the chain id, gas limit and base fee
    let dir = "tests/fixture/tmp/node";
    std::fs::create_dir_all(dir).unwrap();
    // runtime: return storage slot 0 (PUSH0 SLOAD PUSH0 MSTORE PUSH1 32 PUSH0 RETURN)
    std::fs::write(format!("{dir}/genesis.json"), r#"{"config":{"chainId":8453},"timestamp":"0x6553f100","alloc":{"0x000000000000000000000000000000000000bEEF":{"balance":"0x2a","nonce":"0x3","code":"0x5f545f5260205ff3","storage":{"0x0":"0x07"}}}}"#).unwrap();
    // port 0: the node takes a free port itself and names it in its first line, printed once it listens (a port
    // picked here and freed again could be taken by a concurrent test before the node binds it)
    let mut child = Command::new(env!("CARGO_BIN_EXE_plain"))
        .args(["node", "--host", "127.0.0.1", "--port", "0", "--init", "genesis.json", "--gas-limit", "16777216", "--base-fee", "100"])
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // (the pipe stays open until the node is killed: it prints two more lines)
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    let port = first.split("http://127.0.0.1:").nth(1).and_then(|r| r.split(' ').next()).unwrap_or_else(|| panic!("plain node: {first:?}")).to_string();
    let rpc = |method: &str, params: serde_json::Value| -> serde_json::Value {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let mut s = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        write!(s, "POST / HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut r = std::io::BufReader::new(s);
        let mut len = 0;
        loop {
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
        }
        let mut out = vec![0; len];
        r.read_exact(&mut out).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        v["result"].clone()
    };
    let beef = "0x000000000000000000000000000000000000beef";
    let checks = [
        (rpc("eth_chainId", serde_json::json!([])), "0x2105"),
        (rpc("eth_getBalance", serde_json::json!([beef, "latest"])), "0x2a"),
        (rpc("eth_getTransactionCount", serde_json::json!([beef, "latest"])), "0x3"),
        (rpc("eth_call", serde_json::json!([{"to": beef}, "latest"])), "0x0000000000000000000000000000000000000000000000000000000000000007"),
        (rpc("eth_getBlockByNumber", serde_json::json!(["latest", false]))["gasLimit"].clone(), "0x1000000"),
        (rpc("eth_getBlockByNumber", serde_json::json!(["latest", false]))["baseFeePerGas"].clone(), "0x64"),
        (rpc("eth_getBlockByNumber", serde_json::json!(["0x0", false]))["timestamp"].clone(), "0x6553f100"),
    ];
    child.kill().ok();
    drop(stdout);
    for (got, want) in checks {
        assert_eq!(got, want);
    }
}

#[test]
fn fuzz_seeds() {
    // a seed from foundry.toml, $PLAIN_SEED or --seed (in that order of precedence), or a random one per run
    let dir = "tests/fixture/tmp/seeds";
    std::fs::create_dir_all(format!("{dir}/test")).unwrap();
    std::fs::write(format!("{dir}/foundry.toml"), "[profile.default]\n\n[fuzz]\nseed = \"0x1234\"\n").unwrap();
    let head = "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\nimport {Test} from \"forge-std/Test.sol\";\n";
    std::fs::write(format!("{dir}/test/Plain.t.sol"), format!("{head}contract PlainTest is Test {{\n    function test_sum() public pure {{\n        assertEq(uint256(2) + 2, 4);\n    }}\n}}\n")).unwrap();
    std::fs::write(format!("{dir}/test/Fuzz.t.sol"), format!("{head}contract FuzzTest is Test {{\n    function testFuzz_xor(uint256 x) public pure {{\n        assertEq(x ^ x, 0);\n    }}\n}}\n")).unwrap();
    // fails for about one input in seven, so which input fails first depends on the seed
    std::fs::write(format!("{dir}/test/Bad.t.sol"), format!("{head}contract BadTest is Test {{\n    function testFuzz_bad(uint256 x) public pure {{\n        assertTrue(x % 7 != 3);\n    }}\n}}\n")).unwrap();
    let run = |args: &[&str], env: &[(&str, &str)]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_plain"));
        c.arg("test").args(args).current_dir(dir).env_remove("PLAIN_SEED").env_remove("FOUNDRY_PROFILE");
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    };
    let field = |text: &str, prefix: &str| text.lines().find_map(|l| l.trim().strip_prefix(prefix).map(|r| r.split_whitespace().next().unwrap().to_string()));
    let suite = |text: &str, id: &str| text.lines().find(|l| l.starts_with(id)).unwrap_or_else(|| panic!("no {id}: {text}")).to_string();

    // a new random seed each run reruns the fuzz suite, while the suite without fuzz tests stays cached
    let mp = ["--mp", "test/{Plain,Fuzz}.t.sol"];
    let (ok, text) = run(&mp, &[]);
    assert!(ok && text.contains("2 passed, 0 failed") && !text.contains("fuzz seed"), "{text}");
    let mut seeds = vec![];
    for _ in 0..2 {
        let (ok, text) = run(&[&mp[..], &["--seed", "random"]].concat(), &[]);
        assert!(ok && text.contains("2 passed, 0 failed"), "{text}");
        seeds.push(field(&text, "fuzz seed ").unwrap_or_else(|| panic!("no seed printed: {text}")));
        assert!(suite(&text, "test/Plain.t.sol:PlainTest").ends_with("(cached)"), "{text}");
        assert!(!suite(&text, "test/Fuzz.t.sol:FuzzTest").ends_with("(cached)"), "{text}");
    }
    assert_ne!(seeds[0], seeds[1]);
    // the same seed again is a cache hit for the fuzz suite too
    let (ok, text) = run(&[&mp[..], &["--seed", &seeds[1]]].concat(), &[]);
    assert!(ok && suite(&text, "test/Fuzz.t.sol:FuzzTest").ends_with("(cached)"), "{text}");

    // a failure prints its seed, and --seed with it replays the same failing input, however the runs are scheduled
    let bad = ["--mc", "BadTest", "--rerun"];
    let (ok, text) = run(&[&bad[..], &["--seed", "random"]].concat(), &[]);
    assert!(!ok && text.contains("FAIL testFuzz_bad"), "{text}");
    let (seed, args) = (field(&text, "seed: ").unwrap_or_else(|| panic!("{text}")), field(&text, "args: ").unwrap());
    assert!(text.contains(&format!("plain test --seed {seed} --mt testFuzz_bad replays it")), "{text}");
    for threads in ["1", "8"] {
        let (ok, again) = run(&[&bad[..], &["--seed", &seed, "--threads", threads]].concat(), &[]);
        assert!(!ok && field(&again, "args: ").as_ref() == Some(&args) && !again.contains("fuzz seed"), "{text}\n{again}");
    }
    // consecutive seeds (--seed $GITHUB_RUN_NUMBER) try different inputs: the first input a seed draws, as random bytes
    // never come from the code's constants (two seeds may pick the same constant)
    std::fs::write(format!("{dir}/test/Bytes.t.sol"), format!("{head}contract BytesTest is Test {{\n    function testFuzz_first(bytes memory b) public pure {{\n        assertEq(b.length, 1000);\n    }}\n}}\n")).unwrap();
    let failing = |s: &str| field(&run(&["--mc", "BytesTest", "--rerun", "--seed", s], &[]).1, "args: ");
    for s in [100u64, 102, 0x5eec] {
        let (a, b) = (failing(&s.to_string()), failing(&(s + 1).to_string()));
        assert!(a.is_some() && a != b, "{s}: {a:?} {b:?}");
    }
    // the replay command names what else decides the inputs
    let (ok, text) = run(&[&bad[..], &["--seed", "0x9", "--fuzz-runs", "300"]].concat(), &[]);
    assert!(!ok && text.contains("(plain test --fuzz-runs 300 --seed 0x9 --mt testFuzz_bad replays it)"), "{text}");
    // and how it runs, as typed (--block-gas-limit is --gas-limit), and that command replays it
    let (ok, text) = run(&[&bad[..], &["--isolate", "--seed", "0x9", "--block-gas-limit", "900000000"]].concat(), &[]);
    assert!(!ok && text.contains("(plain test --isolate --gas-limit 900000000 --seed 0x9 --mt testFuzz_bad replays it)"), "{text}");
    let (ok, again) = run(&["--rerun", "--isolate", "--gas-limit", "900000000", "--seed", "0x9", "--mt", "testFuzz_bad"], &[]);
    assert!(!ok && field(&again, "args: ") == field(&text, "args: "), "{text}\n{again}");

    // foundry.toml's fuzz.seed, $PLAIN_SEED over it, --seed over both
    for (args, env, want) in [(&[][..], &[][..], "0x1234"), (&[][..], &[("PLAIN_SEED", "99")][..], "0x63"), (&["--seed", "0x5"][..], &[("PLAIN_SEED", "99")][..], "0x5")] {
        let (ok, text) = run(&[&bad[..], args].concat(), env);
        assert!(!ok && field(&text, "seed: ").as_deref() == Some(want), "{want}: {text}");
    }
    let (ok, text) = run(&["--seed", "nope"], &[]);
    assert!(!ok && text.contains("--seed nope: the fuzz seed is"), "{text}");
    // an empty --seed (`--seed "$GITHUB_RUN_NUMBER"` with the variable unset) or a bare 0x is an error, not seed 0
    for s in ["", "0x"] {
        let (ok, text) = run(&["--seed", s], &[]);
        assert!(!ok && text.contains(&format!("--seed {s}: the fuzz seed is")), "{s:?}: {text}");
    }
    let (ok, text) = run(&[], &[("PLAIN_SEED", "0x")]);
    assert!(!ok && text.contains("PLAIN_SEED=0x: the fuzz seed is"), "{text}");
    let (ok, text) = run(&[], &[("PLAIN_SEED", "-1")]);
    assert!(!ok && text.contains("PLAIN_SEED=-1: the fuzz seed is"), "{text}");
    // an empty PLAIN_SEED is unset: foundry.toml's seed applies
    let (ok, text) = run(&bad, &[("PLAIN_SEED", " ")]);
    assert!(!ok && field(&text, "seed: ").as_deref() == Some("0x1234"), "{text}");
}
