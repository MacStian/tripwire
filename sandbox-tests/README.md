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
| `guard.test.near` | The guard, funded through approved DAO `FunctionCall` proposals. No access keys after `new` |
| `agent.test.near` | Signs with a function call key for `request_payment` on the guard only |
| `vendor.test.near` | The one allowlisted recipient |

`Options` sets the proposal bond, the NEAR and USDC budgets and edits to the default policy.

## Tests

| File | What it shows |
| --- | --- |
| `tests/harness.rs` | The setup above, including the empty key list of the guard |
| `tests/payments.rs` | USDC and NEAR paid inside the policy with the agent key, `payment_executed`, a repeated `request_id` rejected |
| `tests/escalation.rs` | Over the per-payment cap, over the period cap and to an unknown recipient: a Transfer proposal filed by the guard with a fixed description. Over the per-payment cap the council approves and the DAO pays |
| `tests/gas.rs` | Worst case gas of every call and callback against the static gas the guard reserves |

## Gas

`tests/gas.rs` uses the largest valid policy (10 agents, 50 recipients, 10 tokens, the accounts
in use at the end of each list), 64 character request ids and a memo over the note limit. Every
request runs with exactly `MIN_PREPAID_GAS`. The test fails when a receipt burns more than half
of its reserved gas, or when the minimum does not cover twice the request plus the reserved gas
of its outgoing call and callback. Print the table with:

```sh
cargo test -p sandbox-tests --test gas -- --nocapture
```

Measured on near-sandbox 2.14.0, TGas:

| Receipt | Burnt | Reserved |
| --- | ---: | ---: |
| `request_payment`, FT payout | 2.96 | |
| `ft_transfer` (mock USDC) | 1.69 | 15 |
| `on_payout_complete`, paid | 1.85 | 10 |
| `on_payout_complete`, FT rolled back | 2.25 | 10 |
| `request_payment`, NEAR payout | 2.68 | |
| `on_payout_complete`, NEAR rolled back | 2.09 | 10 |
| `request_payment`, escalation | 2.70 | |
| `add_proposal` (Sputnik 3.2) | 1.96 | 25 |
| `on_escalation_complete`, escalated | 1.86 | 10 |
| `on_escalation_complete`, rolled back | 1.92 | 10 |
| `update_policy`, largest policy | 2.22 | |
| `withdraw_ft` | 2.62 | |
| `on_withdraw_complete`, withdrawn | 1.86 | 15 |
| `on_withdraw_complete`, failed | 2.28 | 15 |
| `fail_withdrawal` | 1.40 | 5 |

An escalation needs 40.4 of the 50 TGas minimum (twice the request plus 25 and 10 reserved).
The reserved values stay above the measurements on purpose: a real token or a DAO with a large
policy burns more than the mocks here.
