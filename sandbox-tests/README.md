# Sandbox tests

End-to-end tests of the guard on a local NEAR node, next to a real Sputnik DAO v2 and a mock
USDC token. They check what unit tests cannot: `#[private]` methods, full promise chains with
callbacks, access key limits and real gas use.

## Running

```sh
cargo test -p sandbox-tests
```

The first run downloads the `near-sandbox` node (version 2.14.0) and builds the guard and
mock token wasm with `cargo near build`, so it needs `cargo-near` on the `PATH`. The node runs
on Linux and macOS only. On Windows run the tests in WSL.

Every test starts its own node, so tests never share state.

## Pinned Sputnik DAO v2

`res/sputnikdao2.wasm` is the code the mainnet factory `sputnik-dao.near` deploys for new DAOs:

| Field | Value |
| --- | --- |
| Source | [near-daos/sputnik-dao-contract](https://github.com/near-daos/sputnik-dao-contract) |
| Commit | `2b0f34a3860460f285307ec2ccf36345acbe05a2` |
| Version | 3.2 |
| Code hash | `DtHZA1mUmGYeScEaUoUZYQGGFHSMpnjMCvkr5U9hJXuF` |

To download it again and check it against the factory:

```sh
python3 scripts/fetch_sputnik_wasm.py
```

The script reads the code from the factory, checks that the factory metadata still maps the
hash to this commit and that the sha256 of the bytes, in base58, equals the hash.

## Setup

`Harness::start` builds this setup for every test:

| Account | Role |
| --- | --- |
| `council.test.near` | Only council member of the DAO, also the guard's guardian |
| `dao.test.near` | Sputnik DAO. Role `GuardRequestor` gives the guard `transfer:AddProposal` only |
| `usdc.test.near` | Mock NEP-141 token, 6 decimals. DAO, guard and vendor hold storage |
| `guard.test.near` | The guard, funded through approved DAO `FunctionCall` proposals |
| `agent.test.near` | Signs with a function call key for `request_payment` on the guard only |
| `vendor.test.near` | The one allowlisted recipient |

`Options` sets the proposal bond, the NEAR and USDC budgets and edits to the default policy.
