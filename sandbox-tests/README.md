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

`Deployment::create` builds this setup under one master account, which creates and funds the
accounts. `Harness::start` builds it for every test on a fresh local node: it creates the
accounts under `test.near` with a state patch, because the node runs its validator on
`test.near`, and then runs the same contract setup (`Deployment::set_up`):

| Account | Role |
| --- | --- |
| `council.test.near` | Only council member of the DAO, also the guard's guardian |
| `dao.test.near` | Sputnik DAO. Role `GuardRequestor` gives the guard `transfer:AddProposal` only |
| `usdc.test.near` | Mock NEP-141 token, 6 decimals. DAO, guard and vendor hold storage |
| `guard.test.near` | The guard, funded through approved DAO `FunctionCall` proposals. No access keys after `new` |
| `agent.test.near` | Signs with a function call key for `request_payment` on the guard only |
| `vendor.test.near` | The one allowlisted recipient |

`Options` sets the proposal bond, the NEAR and USDC budgets, edits to the default policy and the
NEAR each account starts with. Every account holds the master key, and the guard loses it
after `new`.

## Tests

| File | What it shows |
| --- | --- |
| `tests/harness.rs` | The setup above, including the empty key list of the guard |
| `tests/payments.rs` | USDC and NEAR paid inside the policy with the agent key, `payment_executed`, a repeated `request_id` rejected |
| `tests/escalation.rs` | Over the per-payment cap, over the period cap and to an unknown recipient: a Transfer proposal filed by the guard with a fixed description. Over the per-payment cap the council approves and the DAO pays |
| `tests/auth.rs` | The agent key refused by the runtime for every other guard method (`MethodNameMismatch`), for other contracts (`ReceiverMismatch`) and with a deposit (`DepositWithFunctionCall`). Full access keys of other accounts get `ERR_UNAUTHORIZED_AGENT`. The council cannot skip the vote (`ERR_NOT_DAO`) but as guardian may freeze. Approved proposals change the policy and withdraw. Callbacks and `new` are private |
| `tests/recovery.rs` | A payout to an allowlisted recipient without token storage, and a NEAR payout to a missing account, roll back budget, period spending, payout count and `request_id`, and the same invoice can be retried. A withdrawal that fails ends its DAO proposal as `Failed` with the budget restored. A bond the DAO returns is withdrawn as unallocated NEAR. A token outside the policy is refunded with no state or storage change. USDC sent with a Transfer proposal without `msg` is not credited and cannot be withdrawn |
| `tests/periods.rs` | Spending resets in a new period and the grant expires (`ERR_GRANT_EXPIRED`), both with sandbox fast forward. A grid change keeps the period spending. A frozen guard neither pays nor escalates (`ERR_GUARD_FROZEN`). The payout and escalation caps per period (`ERR_PAYMENT_LIMIT_REACHED`, `ERR_ESCALATION_LIMIT_REACHED`). A policy proposal filed before a freeze and approved after it leaves the guard frozen |
| `tests/gas.rs` | Worst case gas of every call and callback against the static gas the guard reserves |
| `tests/deploy.rs` | The testnet deploy on a local node, with a master holding exactly the NEAR the deploy asks for: the summary, the key files, room left on every account, a payment and an approved escalation signed with the written key files, the allowance they use. A second run, a master whose storage stake leaves it short of NEAR and an RPC that fails or stays silent stop the deploy before anything is sent |

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

## Testnet

`scripts/deploy-testnet.sh` runs the same setup on testnet under a master account you own, with
`Options::testnet()`: the default policy, a 1 NEAR and 1,000 USDC budget and these balances:

| Account | NEAR | Spendable after setup |
| --- | ---: | ---: |
| `council.<master>` | 1.0 | 0.99 |
| `dao.<master>` | 6.5 | 1.04 |
| `guard.<master>` | 3.8 | 2.61, of which 1 is the NEAR budget, 1 the reserve and 0.61 free for bonds |
| `usdc.<master>` | 2.0 | 0.47 |
| `agent.<master>` | 0.5 | 0.50 |
| `vendor.<master>` | 0.1 | 0.10 |

The agent key has a 0.25 NEAR allowance. Unused gas goes back to it, so a request costs it only
the gas burnt: two requests at 100 TGas, a payment and an escalation, take 0.0012 NEAR on the
local node.

With 0.1 NEAR for the gas of the account creations the master needs 14 NEAR it can spend.
`tests/deploy.rs` checks these numbers on a local node.

1. Create the master account and put its key in the near CLI legacy keychain, at
   `~/.near-credentials/testnet/<master>.json`.
2. Fund it with 14 testnet NEAR. A fresh account needs nothing more, because accounts up to 770
   bytes need no storage stake. An account with a contract or more state needs the stake for its
   storage on top, 1 NEAR per 100 kB, less any locked NEAR.
3. Run `scripts/deploy-testnet.sh <master>` on Linux or macOS (WSL on Windows), for example
   `scripts/deploy-testnet.sh tripwire-demo.testnet`. The deploy refuses to run elsewhere,
   because it cannot make the key files readable by the owner only there.

Before it sends anything the script builds both contracts and stops when a build fails, when one
of the six accounts already exists, when a key file it would write is already there or when the
master can spend too little NEAR. Only the RPC's `UNKNOWN_ACCOUNT` answer counts as a missing
account, so an RPC that fails or gives no answer within 15 seconds stops the deploy too. Then it writes both key files,
readable by the owner only and synced to disk, and only then creates the accounts. It writes:

- `~/.near-credentials/testnet/agent.<master>.json`: the agent's function call key, for the
  agent process. It reaches only `request_payment` on the guard.
- `~/.near-credentials/testnet/council.<master>.json`: a full access key of the council's own,
  for the votes in the demo. It reaches only the council account, not the master.
- `deployments/testnet.json`: accounts, code hashes, the agent's public key and its limits, the
  guard policy and explorer links. No secrets, safe to commit. The script writes it last.

`NEAR_RPC_URL` and `NEAR_CREDENTIALS_DIR` override the RPC endpoint and the key directory.

### When a deploy stops partway

The script does not resume. The accounts it already created stay on testnet and the two key
files stay in the keychain. To start over, delete `agent.<master>.json` and
`council.<master>.json` from the keychain and run the script under a new master account.

To get the NEAR back, in this order:

1. If the guard got its budget, move it back to the DAO first, while the DAO and the council
   still exist. The guard has no keys once `lock_guard` ran, so only DAO proposals can move its
   NEAR: a FunctionCall proposal for `withdraw_near` returns the budget, one for
   `withdraw_unallocated_near` the NEAR above `min_near_reserve`. The council approves them,
   signing with the master key, which it still holds. The reserve and the storage stake stay on
   the guard.
2. Delete the other subaccounts with the near CLI, signing with the master key, which every
   subaccount except the guard holds, and with `<master>` as the beneficiary. Deleting the DAO
   returns its NEAR, including what the guard sent back.

A deploy that finished but could not write its summary leaves a working setup: the accounts are
those in the table, under `<master>`.
