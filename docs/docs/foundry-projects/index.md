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

<p class="lead">Run <code>plain test</code> in any Foundry repo and it picks up your existing <code>foundry.toml</code>, remappings, and <code>lib/</code> as they sit. On an untouched OpenZeppelin Contracts checkout, it ran all 383 tests in 34 seconds and passed 340. Two lines in <code>foundry.toml</code> (one of which forge needs too) pushed that to 367, leaving 16 tests that call cheatcodes plain 0.5.0 doesn't implement yet. On solmate, 290 tests passed and the 278 failures are all <code>testFail</code> tests that forge 1.8.6 rejects as well.</p>

I tested this on 4 November 2026 with plain 0.5.0 and forge 1.8.6 (6bad0a6) on Linux x86_64 (12 cores, Node 22.23.4), using fresh clones of [OpenZeppelin Contracts](https://github.com/OpenZeppelin/openzeppelin-contracts) at `efaa6f7` (solc 0.8.35) and [solmate](https://github.com/transmissions11/solmate) at `89365b8` (solc 0.8.15). Both tools ran side by side in the same checkout.

* TOC
{:toc}

## Try it without changing anything

```sh
curl -fsSL https://getplain.org/install | sh
cd your-foundry-project
plain test
echo ".plain/" >> .gitignore
```

The install script took 2 seconds here. Because plain only writes to `.plain/` and `~/.plain`, forge's `out/` and `cache/` stay untouched and you can run `forge test` right next to it. Any failing test returns a non-zero exit code just like forge, so your existing CI step works without edits.

## What I measured

| Project | Tests | forge 1.8.6 | plain 0.5.0 |
|---|---|---|---|
| OpenZeppelin, as cloned | 383 | 359 passed | 340 passed |
| OpenZeppelin, two fixes | 383 | 383 passed | 367 passed |
| solmate | 568 (forge ran 333) | 55 passed, 278 failed | 290 passed, 278 failed |
| Uniswap v4-core | 598 | 597 passed, 1 failed | 597 passed, 1 failed |

The Uniswap v4-core numbers come from the [CI page](/docs/ci/), run on 29 October with the same plain build, where both tools hit the exact same fuzz failure.

On OpenZeppelin Contracts, plain's cold run took 34 seconds including the solc download and build, while forge took 1m16s. Rerunning with nothing changed took 0.26 seconds in plain (reusing all 383 cached results) versus 43 seconds in forge, which re-executes every test. These are single runs on one box, so expect your own machine to vary.

## OpenZeppelin Contracts: from 340 passing to 367

Running `plain test` on a fresh clone gave 340 passes and 43 failures, while `forge test` got 359 passes and 1 failure. Both tools hit the first snag right away, and plain hit two more after that.

**A file the tests read from `node_modules`.** `ERC7579Utils` loads a bytecode file from `node_modules/hardhat-predeploy/`. That folder doesn't exist until you run `npm install`, and `foundry.toml` doesn't grant `fs_permissions` for it. Forge choked in the suite constructor with "not allowed to be accessed for read operations", and plain failed all 24 tests in that suite for the same reason. Both tools need `npm install --no-save hardhat-predeploy` plus this line under `[profile.default]`:

```toml
fs_permissions = [{ access = "read", path = "node_modules/hardhat-predeploy/bin" }]
```

**An inline setting plain doesn't read.** `Math.t.sol` puts `/// forge-config: default.allow_internal_expect_revert = true` above two modExp tests so `vm.expectRevert` catches reverts inside internal library calls. plain reads `allow_internal_expect_revert` from `foundry.toml`, just not from inline comments (unlike `fuzz.runs` and invariant settings). Three `Math.t.sol` tests failed until I moved `allow_internal_expect_revert = true` into `[profile.default]`.

With those two lines in place, forge passed all 383 tests and plain passed 367. The last 16 call cheatcodes plain doesn't have yet, and plain tells you exactly which one:

| Cheatcode | Tests failing | Where |
|---|---|---|
| `vm.publicKeyP256` | 9 | P256, WebAuthn |
| `vm.randomUint`, `randomAddress`, `randomBytes` | 4 | ERC2771Forwarder |
| `vm.toBase64`, `toBase64URL` | 2 | Base64 |
| `vm.setBlockhash` | 1 | Blockhash |

A missing cheatcode prints an explicit error (`plain: unsupported cheatcode vm.toBase64(bytes)`) rather than silently passing, so you never get a false green run. You can check the full list in the [supported cheatcodes docs](/docs/#writing-tests).

## solmate: what `testFail` tests do

solmate still has `testFail…` functions that pass whenever the body reverts for any reason. Forge 1.0 dropped them for being too loose, so both forge 1.8.6 and plain fail all 278 of them. plain prints: `testFail* isn't supported (forge 1.0 removed it…): name it test_… and use vm.expectRevert`.

Where the two tools split is the rest of the suite. Forge stopped short and reported only 333 tests (278 failures, 55 passes); inside `ERC1155Test`, it only ran the 50 `testFail` functions out of 76 total. Calling one of the skipped ones directly (`forge test --mc ERC1155Test --match-test testMintToEOA`) passed fine. plain ran all 568 tests anyway, which is why its 290 passes cover tests forge skipped.

Migrating a `testFail` test takes a minute and forces you to write down what you actually expect. `ReentrancyGuardTest.testFailUnprotectedCall` called a re-entrant function and asserted the counter was 1; the assertion failed, so the old test "passed". My first pass with `vm.expectRevert()` failed in both tools ("call did not revert as expected") because the call doesn't revert at all—it re-enters. Asserting the counter is 2 says what the test actually meant:

```solidity
function testUnprotectedCallReenters() public {
    riskyContract.unprotectedCall();

    assertEq(riskyContract.enterTimes(), 2);
}
```

After that, the suite passed 4 of 4 in both tools. For tests that genuinely expect a revert, rename them to `test_RevertIf_…` and add `vm.expectRevert(...)` right before the call.

## Questions

**Do I have to change `foundry.toml` to use plain?** No. On an untouched OpenZeppelin Contracts clone, plain ran all 383 tests and passed 340. The two config lines above were for a bytecode file read from disk (which forge needed too) and an inline comment setting.

**Can plain and forge run in the same project?** Yes. I ran both in the same checkouts here: plain keeps its files in `.plain/`, and forge keeps `out/` and `cache/`.

**Why do my `testFail` tests fail?** plain tracks forge 1.x, and forge 1.8.6 rejects them too. Rename them to `test_Revert…` with `vm.expectRevert`, or assert the actual outcome you wanted.

**What if a test uses a cheatcode plain doesn't have?** That test fails and prints the missing cheatcode while the rest of the suite keeps running. If you need it, [open an issue](https://github.com/backmeupplz/plain/issues); the [list of what plain doesn't do yet](/docs/#what-plain-doesnt-do-yet) is short.

**What did I not test?** Hardhat projects with a `hardhat.config`, live RPC forks, and gas snapshot files (plain doesn't write those). OpenZeppelin's 164 TypeScript tests were skipped since plain only runs them when `typescript = true` is set in `plain.toml`.
