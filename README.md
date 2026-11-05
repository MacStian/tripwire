# Tripwire

A budget guard for a Sputnik DAO on NEAR. The DAO funds the guard with a small budget. An AI agent asks the guard to pay an invoice. Inside the policy the guard pays from its budget. Outside the policy it files a Transfer proposal and the DAO votes.

Status: work in progress for the NEAR Hackathon (1 to 14 November 2026).

## Layout

- `contracts/guard`: the guard contract
- `contracts/mock-ft`: a NEP-141 test token with an open `mint`, for sandbox and testnet only

## Build and test

Rust 1.99.1 is pinned in `rust-toolchain.toml`. Wasm builds need cargo-near 0.22. Sandbox tests need Linux, macOS or WSL2.

```sh
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
cd contracts/guard && cargo near build non-reproducible-wasm
```

## Security and license

- `THREAT_MODEL.md`: assets, actors, threats, the check and the test behind each mitigation, and the residual risks
- MIT, see `LICENSE`
