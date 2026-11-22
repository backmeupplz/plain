---
layout: docs
og_image: /og-ci.png
og_image_alt: "Run Solidity tests in CI in seconds: Uniswap v4-core on 2 CPUs, 1m43s cold with plain, 8m57s with forge."
title: "Run Solidity tests in CI in seconds with plain"
description: "Run a Foundry project's tests in GitHub Actions with plain: a workflow, what to cache, and measured times on Uniswap v4-core (1m43s cold against 8m57s for forge, 4.9 s with a restored cache)."
jsonld: |
  {"@context":"https://schema.org","@type":"TechArticle","headline":"Run Solidity tests in CI in seconds with plain","description":"Run a Foundry project's tests in GitHub Actions with plain: a workflow, what to cache, and measured times on Uniswap v4-core.","datePublished":"2026-10-29","dateModified":"2026-11-04","author":{"@type":"Person","name":"Nikita Kolmogorov","url":"https://borodutch.com"},"publisher":{"@type":"Organization","name":"plain","url":"https://getplain.org"},"mainEntityOfPage":"https://getplain.org/docs/ci/","image":"https://getplain.org/og-ci.png"}
---

# Run Solidity tests in CI in seconds
{: .no_toc}

<p class="lead">Install plain in one line, swap <code>plain test</code> in for <code>forge test</code>, and cache three directories under <code>~/.plain</code>. Pinned to 2 CPUs on Uniswap v4-core, plain finished a cold run in 1m43s versus 8m57s for forge 1.8.6, and a fresh clone with a restored cache ran in 4.9 seconds.</p>

Tested on 29 October 2026 on Linux x86_64 with plain 0.5.0 and forge 1.8.6 against [Uniswap v4-core](https://github.com/Uniswap/v4-core) at commit `bea7f6d` (39 test suites, via-IR, solc 0.8.26). I ran every command below myself. The YAML workflow itself wasn't run on a hosted GitHub runner because I don't have one; instead I pinned everything to 2 CPUs with `taskset -c 0,1` on a single box to approximate a small CI runner.

* TOC
{:toc}

## The workflow

{% raw %}
```yaml
name: tests
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          submodules: recursive
      - uses: actions/cache@v4
        with:
          path: |
            ~/.plain/cache
            ~/.plain/solc
            ~/.plain/salt
          key: plain-${{ runner.os }}-${{ github.sha }}
          restore-keys: plain-${{ runner.os }}-
      - run: curl -fsSL https://getplain.org/install | sh
      - run: ~/.plain/bin/plain test
```
{% endraw %}

Pass `submodules: recursive` to checkout because plain reads `lib/` straight from disk like forge does; only forge-std ships built in. The installer finishes in about 3 seconds: it grabs the release binary for your OS, verifies it against `SHA256SUMS`, and drops an 11 MB binary into `~/.plain/bin`. `plain test` exits non-zero on any failure so CI goes red just like forge, and any `[profile.ci]` block in `foundry.toml` kicks in with `FOUNDRY_PROFILE=ci`.

## What the cache holds

plain stores three things outside the repo that you want restored between runs:

- `~/.plain/solc` holds downloaded compilers (29 MB here for solc 0.8.26).
- `~/.plain/cache` stores compiled artifacts and test results by content hash (7.9 MB here), shared across every checkout on the machine.
- `~/.plain/salt` is a per-machine random salt mixed into every cache key for builds and test runs. That stops a copied cache file from masquerading as a local build or test result, which also means a different salt invalidates the cache. plain 0.5.0 left the salt out of test-result keys, so a mismatched salt recompiled everything while still reusing test results whenever the bytecode matched; newer releases re-run every suite.

I learned about the salt the hard way. Copying just `cache` and `solc` into a fresh home directory still left a new clone taking 1m42s because nothing matched. Dropping `salt` in next to them cut that same run to 4.9 seconds, so the workflow caches all three.

Your checkout path doesn't need to match the machine that built the cache. Test results key off paths relative to the project root, so cloning into another folder (like GitLab's `builds/<runner>/<concurrency id>/…`, self-hosted runner workdirs or git worktrees) still gets cache hits. Only path-sensitive suites re-run: anything that calls `vm.projectRoot()`, passes a file cheatcode an absolute path, a `..` segment, a symlink or a path outside the repo, or hits a file-read error containing an absolute path. You need a release after 0.5.0 for that; 0.5.0 hashed the absolute path into every test key, so on 9 November 2026 the same fresh clone took 2.5 s at the original path versus 10.2 s in a different folder (contracts came from cache, tests re-ran). GitHub Actions reuses the same checkout path anyway, so the YAML above already hit the cache.

Since a restored cache is trusted like your own build output, only restore it in workflows you control.

## Measured times

I ran each command once on the same machine, pinned to 2 CPUs, running `plain` and `forge` back-to-back on the same checkout. These are single runs rather than averages, so your repo and runner will differ.

| Step | plain 0.5.0 | forge 1.8.6 |
|---|---|---|
| Cold, nothing cached | 1m43s | 8m57s |
| Rerun, nothing changed | 4.1 s | 29.6 s |
| Add one test, run its suite | 1.0 s | 27.2 s |
| Fresh clone, restored cache | 4.9 s | n/a |

The cold run includes downloading solc and compiling from scratch: forcing a build with `plain build --force` took 79.9 s and the tests took 19.4 s, while forge's tests took 30.0 s, meaning almost all of forge's 8m57s went to via-IR compilation. On the unchanged rerun, plain pulled 565 test results straight from cache and only ran the rest, whereas forge re-ran the whole suite.

For the single-test edit, I appended a one-line test to `test/types/Slot0.t.sol` and ran just that suite (`--mc TestSlot0`); running all 39 suites after the same edit took plain 3.8 s. The fresh-clone run was a brand-new `git clone --recurse-submodules` with `~/.plain/cache`, `solc` and `salt` copied into a clean home directory. On plain 0.5.0 that time holds only at the same checkout path (see [What the cache holds](#what-the-cache-holds)); newer releases drop the path from the key, though I haven't re-timed v4-core in a second directory, and I didn't time forge on this step.

## One v4-core test fails under both tools

Both tools hit the same single failure on this commit: plain printed `597 passed, 1 failed` and forge 1.8.6 printed `597 tests passed, 1 failed, 0 skipped (598 total tests)`. The failing test is `test_fuzz_getTickAtSqrtPrice_getSqrtPriceAtTick_relation`, which both trip on `-887273`. That input clamps to tick 887271, whose next tick sits right at `MAX_SQRT_PRICE`, causing `TickMath.getTickAtSqrtPrice` to revert with `InvalidSqrtPrice`.

I double-checked this on 4 November 2026 across forge 1.8.6, forge 1.5.1 and plain 0.5.0 on the same checkout. With the repo's `0x4444` seed, forge 1.8.6 hit that input on the second fuzz run and forge 1.5.1 hit it after 977 runs. Trying 30 seeds at 1,000 runs, 30 at 10,000 runs and 10 at 100,000 runs caught it every time. As written, that fuzz test turns CI red on either tool because both execute the exact same `TickMath` code.

An earlier draft of this section claimed forge passed all 598 tests because I misread the total in its summary line; I fixed that on 4 November 2026.

## What plain doesn't run

Like forge 1.x, plain rejects tests named `testFail…` and tells you to switch to `vm.expectRevert`. On [solmate](https://github.com/transmissions11/solmate) (commit `89365b8`), that accounted for all 278 failures out of 568 tests, and forge 1.8.6 failed the exact same 278 (see [Use plain in an existing Foundry project](/docs/foundry-projects/)). Any repo still on `testFail` needs those tests renamed before either tool will pass them.

plain doesn't generate gas snapshots or coverage reports yet either, so check [what plain doesn't do yet](/docs/#what-plain-doesnt-do-yet) before swapping out a CI job that relies on those.

## Questions

**Do I need Foundry installed in CI?** No. plain fetches the solc versions your project needs and bundles forge-std.

**Does it use my `foundry.toml`?** Yes. It reads your solc version, optimizer, via-IR, remappings, fuzz and invariant settings, and the rest of the [Foundry settings](/docs/#foundry-projects) it supports as-is.

**Do fuzz tests get new inputs on every run?** Not by default, since plain fixes its fuzz seed so passing fuzz and invariant suites stay cached until code changes. Pass `~/.plain/bin/plain test --seed random` (or `--seed {% raw %}${{ github.run_number }}{% endraw %}`) to test fresh inputs each run like forge does. It prints the chosen seed at startup, and any fuzz or invariant failure prints the seed plus a local replay command (carrying `FOUNDRY_PROFILE`, `--fuzz-runs`, `--fork`, `--isolate` and `--gas-limit` if set), while non-fuzz suites still come from cache.

**Can I run both?** Yes. plain only writes inside `.plain/`, leaving forge's `out/` and `cache/` untouched so you can run `forge test` alongside it while comparing.

**Where is the formatter check?** `plain fmt --check` lists unformatted files and exits 1 for CI gates. On v4-core it ran in 45 ms (`forge fmt --check` took 50 ms) and flagged 30 of 132 files.
