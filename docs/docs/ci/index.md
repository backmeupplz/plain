---
layout: docs
og_image: /og-ci.png
og_image_alt: "Run Solidity tests in CI in seconds: Uniswap v4-core on 2 CPUs, 1m43s cold with plain, 8m57s with forge."
title: "Run Solidity tests in CI in seconds with plain"
description: "Run a Foundry project's tests in GitHub Actions with plain: a workflow, what to cache, and measured times on Uniswap v4-core (1m43s cold against 8m57s for forge, 4 s with a restored cache)."
jsonld: |
  {"@context":"https://schema.org","@type":"TechArticle","headline":"Run Solidity tests in CI in seconds with plain","description":"Run a Foundry project's tests in GitHub Actions with plain: a workflow, what to cache, and measured times on Uniswap v4-core.","datePublished":"2026-10-29","dateModified":"2026-10-29","author":{"@type":"Person","name":"Nikita Kolmogorov","url":"https://borodutch.com"},"publisher":{"@type":"Organization","name":"plain","url":"https://getplain.org"},"mainEntityOfPage":"https://getplain.org/docs/ci/","image":"https://getplain.org/og-ci.png"}
---

# Run Solidity tests in CI in seconds
{: .no_toc}

<p class="lead">Install plain with one command, run <code>plain test</code> in place of <code>forge test</code>, and keep three entries of <code>~/.plain</code> in your CI cache. On Uniswap v4-core, pinned to 2 CPUs, a cold run took 1m43s with plain against 8m57s with forge 1.8.6, and a fresh clone with a restored cache took 4.9 seconds.</p>

Tested on 29 October 2026 with plain 0.5.0 and forge 1.8.6 on Linux x86_64, on [Uniswap v4-core](https://github.com/Uniswap/v4-core) at commit `bea7f6d` (39 test suites, via-IR, solc 0.8.26). Every command below was run for this page. The workflow file itself was not: I don't have a hosted GitHub runner, so the commands ran on one machine whose processes were pinned to 2 CPUs (`taskset -c 0,1`) to come closer to a small CI runner.

* TOC
{:toc}

## The workflow

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

The checkout needs `submodules: recursive` because plain, like forge, reads `lib/` as it is; only forge-std is built in. The installer takes about 3 seconds: it downloads the release for your platform, checks it against the release's `SHA256SUMS` and unpacks one 11 MB binary into `~/.plain/bin`. `plain test` exits with a non-zero code when any test fails, so the job fails as it would with forge. A `[profile.ci]` in `foundry.toml` is picked up with `FOUNDRY_PROFILE=ci`.

## What the cache holds

plain keeps three things outside the project that are worth restoring between runs:

- `~/.plain/solc` has the compilers it downloaded (29 MB here, for solc 0.8.26).
- `~/.plain/cache` has compiled contracts and test results by content hash (7.9 MB here), shared by every checkout of every project.
- `~/.plain/salt` is a random value of the machine that goes into every cache key. It exists so that a cache file copied from somewhere else can never pass for a compile you made, and it means the cache is ignored on a machine with a different salt.

I checked the salt the hard way. Copying `cache` and `solc` into a new home directory and running a fresh clone still took 1m42s, because the cache didn't match. Adding `salt` brought the same run down to 4.9 seconds. So the three entries go together, and the workflow above caches all of them.

Because a restored cache is trusted as if you had built it, restore it only in workflows you control.

## Measured times

Each command was run once on the same machine, pinned to 2 CPUs, with `plain` and `forge` run one after the other on the same checkout. These are single runs, not averages, and your project and runner will differ.

| Step | plain 0.5.0 | forge 1.8.6 |
|---|---|---|
| Cold, nothing cached | 1m43s | 8m57s |
| Rerun, nothing changed | 4.1 s | 29.6 s |
| Add one test, run its suite | 1.0 s | 27.2 s |
| Fresh clone, restored cache | 4.9 s | n/a |

- **Cold** includes downloading solc and compiling everything. plain's compile alone, forced with `plain build --force`, took 79.9 s, and its tests ran in 19.4 s. forge's tests ran in 30.0 s, so almost all of its 8m57s was compiling the project with via-IR.
- **Rerun** is where plain reuses test results: 565 of the results came from its cache and the rest ran again. forge ran every test again.
- **Add one test** appended a one-line test to `test/types/Slot0.t.sol`; both times are for running that suite (`--mc TestSlot0`). Running all 39 suites after the same edit took plain 3.8 s.
- **Fresh clone** is a new `git clone --recurse-submodules` in a new directory with `~/.plain/cache`, `solc` and `salt` copied from the earlier run into a new home directory. I did not time forge on this step.

## One test plain failed that forge passed

plain's run ended with `597 passed, 1 failed`, and forge's with `598 tests passed`. The failing test is `test_fuzz_getTickAtSqrtPrice_getSqrtPriceAtTick_relation`, and plain fails it every time, with the input `-887273`. That input bounds to tick 887271, whose next tick's price is exactly `MAX_SQRT_PRICE`, and `TickMath.getTickAtSqrtPrice` reverts with `InvalidSqrtPrice` on that value.

I wrote a three-line test that calls it with that tick, and forge fails it the same way. So the project's fuzz test has an edge case that forge's fuzzer, with the project's seed of `0x4444` and 1,000 runs, didn't reach and plain's did. The times above are for a suite that goes red in CI under plain, because forge would fail that input too.

## What plain doesn't run

plain follows forge 1.x, so tests named `testFail…` are rejected with a message that says to use `vm.expectRevert`. On [solmate](https://github.com/transmissions11/solmate) (commit `89365b8`) 278 of 568 tests failed for this reason in my run, and nothing else failed. A project that still uses `testFail` has to rename those tests before plain can run them.

plain also doesn't write gas snapshot files or run coverage, so check [what plain doesn't do yet](/docs/#what-plain-doesnt-do-yet) before replacing a CI step that depends on either.

## Questions

**Do I need Foundry installed in CI?** No. plain downloads the solc versions your project needs, and forge-std is built in.

**Does it use my `foundry.toml`?** Yes: the solc version, optimizer, via-IR, remappings, fuzz and invariant settings and the rest of the [Foundry settings](/docs/#foundry-projects) it reads are used as they are.

**Can I run both?** Yes. plain writes only to `.plain/` in the project, so forge's `out/` and `cache/` are untouched and a job can run `forge test` next to it while you compare.

**Where is the formatter check?** `plain fmt --check` lists the files that aren't formatted and exits 1, which is a gate for CI. On v4-core it took 45 ms in my run (`forge fmt --check` took 50 ms) and listed 30 of 132 files as not formatted.
