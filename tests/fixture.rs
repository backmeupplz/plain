// Runs the binary on tests/fixture: every testBad*, invariantBad* and testFail* (gone since forge 1.0) must fail,
// everything else pass.
use std::io::{BufRead, Read, Write};
use std::process::Command;

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
                    // PUSH0 SLOAD PUSH0 MSTORE PUSH1 32 PUSH0 RETURN
                    "eth_getCode" => serde_json::json!(if beef { "0x5f545f5260205ff3" } else { "0x" }),
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
