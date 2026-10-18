# plain-evm

[plain](https://getplain.org) compiles, tests, deploys, verifies and upgrades EVM smart contracts with one binary. This package installs that binary as `plain`:

```sh
npm install -g plain-evm   # then: plain init && plain test
npx plain-evm test         # or run it without installing
```

npm picks the binary for your machine from the optional `plain-evm-<os>-<cpu>` packages (macOS arm64 and x64, Linux x64 with glibc 2.34+), so don't install with `--omit=optional`. Docs and other ways to install: https://getplain.org and https://github.com/backmeupplz/plain.
