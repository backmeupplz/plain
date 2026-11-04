---
layout: docs
og_image: /og-foundry.png
og_image_alt: "Use plain in an existing Foundry project: OpenZeppelin Contracts, 340 of 383 tests pass as it comes and 367 after two config lines."
title: "Use plain in an existing Foundry project: what runs and what doesn't"
description: "Run plain test in a Foundry repository without changing it. Tested on OpenZeppelin Contracts and solmate against forge 1.8.6: 340 of 383 tests pass as it comes, 367 after two config lines, and every failure is explained."
jsonld: |
  {"@context":"https://schema.org","@graph":[{"@type":"TechArticle","headline":"Use plain in an existing Foundry project: what runs and what doesn't","description":"Run plain test in a Foundry repository without changing it, tested on OpenZeppelin Contracts and solmate against forge 1.8.6.","datePublished":"2026-11-04","dateModified":"2026-11-04","author":{"@type":"Person","name":"Nikita Kolmogorov","url":"https://borodutch.com"},"publisher":{"@type":"Organization","name":"plain","url":"https://getplain.org"},"mainEntityOfPage":"https://getplain.org/docs/foundry-projects/","image":"https://getplain.org/og-foundry.png"},{"@type":"FAQPage","mainEntity":[{"@type":"Question","name":"Do I have to change foundry.toml to use plain?","acceptedAnswer":{"@type":"Answer","text":"No. plain reads foundry.toml, remappings.txt, lib/ and .env as they are. On OpenZeppelin Contracts it ran all 383 tests without any change to the project, and 340 passed; two lines in foundry.toml brought that to 367."}},{"@type":"Question","name":"Why do my testFail tests fail in plain?","acceptedAnswer":{"@type":"Answer","text":"plain follows forge 1.x, which removed testFail. forge 1.8.6 fails them too, with the message that testFail has been removed. Rename each to test_Revert and use vm.expectRevert, or assert the behaviour you meant."}},{"@type":"Question","name":"Which cheatcodes does plain not support?","acceptedAnswer":{"@type":"Answer","text":"In plain 0.5.0, randomUint, randomAddress, randomBytes, toBase64, toBase64URL, setBlockhash and publicKeyP256 failed with an unsupported cheatcode error in the OpenZeppelin Contracts tests."}}]}]}
---

# Use plain in an existing Foundry project
{: .no_toc}

<p class="lead">Run <code>plain test</code> in the root of a Foundry repository and it reads your <code>foundry.toml</code>, remappings and <code>lib/</code> as they are. On OpenZeppelin Contracts it ran all 383 tests in 34 seconds with no change to the project, and 340 passed. Two lines in <code>foundry.toml</code> (one of them forge needs too) brought that to 367, and the last 16 call cheatcodes plain 0.5.0 doesn't have yet. On solmate, 290 tests passed, and the 278 that failed use <code>testFail</code>, which forge 1.8.6 fails as well.</p>

Tested on 4 November 2026 with plain 0.5.0 and forge 1.8.6 (6bad0a6) on Linux x86_64 (12 cores, Node 22.23.4), on [OpenZeppelin Contracts](https://github.com/OpenZeppelin/openzeppelin-contracts) at commit `efaa6f7` (solc 0.8.35) and [solmate](https://github.com/transmissions11/solmate) at `89365b8` (solc 0.8.15). Every number below is from a run I made for this page, with each project cloned fresh and both tools run in the same checkout.

* TOC
{:toc}

## Try it without changing anything

```sh
curl -fsSL https://getplain.org/install | sh
cd your-foundry-project
plain test
echo ".plain/" >> .gitignore
```

The installer took 2 seconds here. plain writes only to `.plain/` in the project and to `~/.plain`, so forge's `out/` and `cache/` stay as they were and you can keep running `forge test` next to it. If a test fails, the exit code is non-zero, as with forge, so the same CI step works with either.

## What I measured

| Project | Tests | forge 1.8.6 | plain 0.5.0 |
|---|---|---|---|
| OpenZeppelin, as cloned | 383 | 359 passed | 340 passed |
| OpenZeppelin, two fixes | 383 | 383 passed | 367 passed |
| solmate | 568 (forge ran 333) | 55 passed, 278 failed | 290 passed, 278 failed |
| Uniswap v4-core | 598 | 597 passed, 1 failed | 597 passed, 1 failed |

The third row is from the [CI page](/docs/ci/), run on 29 October with the same plain version. Both tools fail the same fuzz test there.

On OpenZeppelin Contracts, plain's first run took 34 seconds including the solc download and the compile, and forge's took 1m16s including its compile. Running again with nothing changed took 0.26 seconds with plain, which reused 383 stored results, and 43 seconds with forge, which runs every test again. These are single runs on one machine, so your times will differ.

## OpenZeppelin Contracts: from 340 passing to 367

The first `plain test` with the repository as it came gave 340 passed and 43 failed, and `forge test` gave 359 passed and 1 failed. The two tools stumbled over the same thing first, then plain over two more kinds of failure.

**A file the tests read from `node_modules`.** The `ERC7579Utils` suite reads a bytecode file from `node_modules/hardhat-predeploy/`. That folder only exists after `npm install`, and the project's `foundry.toml` has no `fs_permissions` entry for it, so forge failed the suite's constructor with "not allowed to be accessed for read operations" and plain failed all 24 tests of that suite with the same reason. The fix is for both tools: `npm install --no-save hardhat-predeploy` and this line under `[profile.default]`:

```toml
fs_permissions = [{ access = "read", path = "node_modules/hardhat-predeploy/bin" }]
```

**An inline setting plain doesn't read.** `Math.t.sol` has `/// forge-config: default.allow_internal_expect_revert = true` above the two modExp tests, which lets `vm.expectRevert` catch a revert from an internal library call. plain reads `allow_internal_expect_revert` from `foundry.toml` but not from that comment (it reads `fuzz.runs` and the invariant settings from comments). Three tests in `Math.t.sol` failed because of it, and they passed after I put `allow_internal_expect_revert = true` under `[profile.default]`.

After both changes, forge passed all 383 tests and plain passed 367. The remaining 16 are tests that call cheatcodes plain doesn't implement, and plain prints which one:

| Cheatcode | Tests failing | Where |
|---|---|---|
| `vm.publicKeyP256` | 9 | P256, WebAuthn |
| `vm.randomUint`, `randomAddress`, `randomBytes` | 4 | ERC2771Forwarder |
| `vm.toBase64`, `toBase64URL` | 2 | Base64 |
| `vm.setBlockhash` | 1 | Blockhash |

A failing test names the cheatcode (`plain: unsupported cheatcode vm.toBase64(bytes)`) instead of passing quietly, so a missing cheatcode never looks like a green run. The [docs list the supported cheatcodes](/docs/#writing-tests).

## solmate: what `testFail` tests do

solmate's suites use `testFail…` tests, which expect the body to revert. forge 1.0 removed them, because they pass whenever the body fails for any reason. In this run forge 1.8.6 and plain both fail all 278 of them with a message that says so, and plain's reads: `testFail* isn't supported (forge 1.0 removed it…): name it test_… and use vm.expectRevert`.

The tools differ in what they do with the rest of a suite that has such a test. forge reported only the 278 failures and 55 passing tests (333 in all), and in the `ERC1155Test` suite it ran just the 50 `testFail` functions, although the file has 76 tests. When I asked for one of the others by name, `forge test --mc ERC1155Test --match-test testMintToEOA`, it passed. plain ran all 568, so its 290 passes include the ones forge didn't run in that mode.

Rewriting a `testFail` test takes a minute, and it makes you say what it was checking. `ReentrancyGuardTest.testFailUnprotectedCall` called a function that re-enters itself and asserted the counter was 1, which is false, so the test "passed" because the assertion failed. My first rewrite, `vm.expectRevert()`, failed in both tools with "call did not revert as expected", since the call never reverts. What the test meant was that the unprotected call re-enters, so the final version asserts that the counter is 2:

```solidity
function testUnprotectedCallReenters() public {
    riskyContract.unprotectedCall();

    assertEq(riskyContract.enterTimes(), 2);
}
```

That suite then passed 4 of 4 in plain and in forge. For tests that really expect a revert, name them `test_RevertIf_…` and put `vm.expectRevert(...)` before the call.

## Questions

**Do I have to change `foundry.toml` to use plain?** No. On OpenZeppelin Contracts, plain ran all 383 tests against the repository as it was and 340 passed. The two changes above were for a file the tests read and for an inline setting, and forge needed the first one too.

**Can plain and forge run in the same project?** Yes. I ran both in the same checkouts for this page. plain keeps its state in `.plain/`, and forge keeps `out/` and `cache/`.

**Why do my `testFail` tests fail?** plain follows forge 1.x, and forge 1.8.6 fails them too. Rename each to `test_Revert…` with `vm.expectRevert`, or assert the behaviour you meant.

**What if a test uses a cheatcode plain doesn't have?** That test fails and names the cheatcode, and the rest of the suite runs. If you need one, [open an issue](https://github.com/backmeupplz/plain/issues); the [list of what plain doesn't do yet](/docs/#what-plain-doesnt-do-yet) is short.

**What did I not test?** Hardhat-style projects with their own `hardhat.config`, forks against a live RPC, and gas snapshot files, which plain doesn't write. The 164 TypeScript test files in OpenZeppelin Contracts were skipped, because plain runs them only with `typescript = true` in `plain.toml`.
