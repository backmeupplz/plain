// Runs the binary on tests/fixture: every testBad*/testFailLegacyPassesIsBad must fail, everything else pass.
use std::process::Command;

#[test]
fn fixture() {
    let out = Command::new(env!("CARGO_BIN_EXE_plain")).arg("test").arg("-v").current_dir("tests/fixture").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines().filter(|l| l.trim_start().starts_with("pass ") || l.trim_start().starts_with("FAIL ")) {
        let failed = line.trim_start().starts_with("FAIL");
        let should_fail = line.contains("testBad") || line.contains("PassesIsBad") || line.contains("invariantBad");
        assert_eq!(failed, should_fail, "{line}\n{text}");
    }
    assert!(text.contains("15 passed, 8 failed"), "{text}");
    assert!(text.contains("Breakable.poke(7)"), "{text}");
    assert!(text.contains("panic: arithmetic underflow"), "{text}");
    assert!(text.contains("log: value is 99"), "{text}");
    // viem-based TS test against the in-process chain (needs `bun install` in tests/fixture)
    assert!(text.contains("1 ts files passed, 0 failed"), "{text}");
}
