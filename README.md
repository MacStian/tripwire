# Tripwire

A budget guard for a Sputnik DAO on NEAR. An AI agent can pay routine invoices from a small budget the DAO controls. Everything outside the DAO's policy becomes a DAO proposal that humans vote on.

Built for the NEAR Hackathon (1 to 14 November 2026). Tested on a local NEAR sandbox node against a real Sputnik DAO, with a deploy script for testnet.

## The problem

A DAO that lets an agent handle its invoices has two options on NEAR today:

- Give the agent a proposal role. Every invoice, even a 20 USDC one, waits for a human vote.
- Let a reviewer agent vote on small payments. The limit is then a model's judgement, and the invoice it judges is attacker-controlled text.

An invoice is untrusted input. A poisoned invoice can tell the agent to pay another account, to pay more, or to put misleading text in front of voters. The limits have to hold even when the agent does what such an invoice says.

## How it works

The DAO moves a small budget to the guard contract and sets its policy by proposal: the agents that may ask, the recipients that may be paid directly, per-payment and per-period caps for each token, an expiry and an optional guardian. The agent holds a NEAR function call key that can call `request_payment` on the guard and nothing else.

On every `request_payment` the guard does one of three things:

1. **Reject.** The caller is not a listed agent, the guard is frozen, the grant expired, the `request_id` was used before, the amount is zero, or this period's count of payouts or escalations is used up.
2. **Pay** from its budget. The recipient is allowlisted, the token is in the policy and the amount fits the per-payment cap, the period cap and the budget.
3. **Escalate.** Anything else becomes a Sputnik `Transfer` proposal with the reason (`RECIPIENT_NOT_ALLOWLISTED`, `TOKEN_NOT_ALLOWED`, `EXCEEDS_PER_PAYMENT_CAP`, `EXCEEDS_PERIOD_CAP`, `EXCEEDS_GUARD_BUDGET`). The council votes on it. The guard writes the description itself as one fixed line, and the agent's memo passes a character allowlist.

```mermaid
flowchart LR
    I[Invoice] --> A[AI agent]
    A -- "request_payment<br/>(function call key)" --> G[Tripwire guard]
    G -- "inside the policy:<br/>pays from its budget" --> V[Vendor]
    G -- "outside the policy:<br/>Transfer proposal" --> D[Sputnik DAO]
    C[Council] -- votes --> D
    D -- "approved: pays<br/>from the treasury" --> V
    D -- "proposals: fund, update_policy,<br/>unfreeze, withdraw" --> G
```

The worst case with a stolen agent key, or an agent that follows a poisoned invoice, is payments to allowlisted recipients up to the period cap, never more than the guard's budget. The treasury stays out of reach. The guard has no vote in the DAO, only a role that can add Transfer proposals, and the guard account has no access keys after deployment.

## The agent key

The agent signs every request with one NEAR function call access key, never a full access key. The deploy adds it to the agent account with these limits, and the NEAR runtime enforces them before the guard runs:

| Limit | Value |
| --- | --- |
| Receiver | The guard account only |
| Methods | `request_payment` only |
| Deposit | None. A function call key cannot attach NEAR |
| Gas allowance | 0.25 NEAR |

A function call key signs only function calls, so it cannot transfer NEAR or add and delete keys. `tests/auth.rs` shows the runtime refusing the key for `update_policy`, `freeze`, `unfreeze` and the three withdrawals on the guard, for `ft_transfer` on the token and with a deposit. All it can do is ask the guard to pay, which leads to the worst case above.

Unused gas goes back to the allowance, so a request costs it only the gas burnt: about 0.0006 NEAR on the local node, so 0.25 NEAR covers about 400 requests. When the allowance runs out the runtime refuses the key and the owner of the agent account adds a new one. That owner is the deploying master, whose full access key the agent account also holds. The agent process never gets it, because the deploy writes only the function call key to `agent.<master>.json`.

## Where it fits

| | Limits enforced by | Governed by | Request over the limit |
| --- | --- | --- | --- |
| Reviewer agent that votes on small payments ([Trezu #624](https://github.com/NEAR-DevHub/trezu/issues/624)) | Model judgement | Treasury members | The agent does not vote |
| [OutLayer](https://github.com/out-layer/outlayer/blob/main/docs/NON_CUSTODIAL.md) agent custody | TEE. Period caps are counted outside the enclave | Wallet owner | Multisig approval in OutLayer |
| [Safe Allowance Module](https://docs.safe.global/home/ai-agent-quickstarts/agent-with-spending-limit), [Zodiac Roles v2](https://github.com/zodiaceco/zodiac-modifier-roles) (Ethereum) | Contract | Safe owners | Rejected |
| **Tripwire** | Contract | Sputnik DAO proposals | Becomes a Sputnik proposal |

An invoice agent such as [`denbite/trezu-skills`](https://github.com/denbite/trezu-skills) files every invoice as a proposal. With Tripwire it calls the guard instead and needs no DAO role. Only requests outside the policy reach a vote.

## Try it

Needs Linux, macOS or WSL2 (the sandbox node does not run on Windows), [rustup](https://rustup.rs) and [cargo-near](https://github.com/near/cargo-near) 0.22. `rust-toolchain.toml` pins Rust 1.99.1 with the wasm target, so rustup installs it on the first build.

```sh
git clone https://github.com/MacStian/tripwire
cd tripwire
cargo test --workspace
```

You see one `test result: ok` line per test binary: 114 guard unit tests, 2 mock token tests and 29 sandbox tests in 8 files. The first run downloads the `near-sandbox` node 2.14.0 and builds both contracts with `cargo near`, so it takes a few minutes. Every end-to-end test starts its own local node with a real Sputnik DAO v2 (`sandbox-tests/res/sputnikdao2.wasm`, pinned and checked against the mainnet factory), a mock USDC token and the guard.

One scenario at a time:

| Do | See |
| --- | --- |
| `cargo test -p sandbox-tests --test payments` | `pays_usdc_inside_the_policy ... ok`: the agent key pays an 80 USDC invoice to the allowlisted vendor at once. The same `request_id` again is rejected |
| `cargo test -p sandbox-tests --test escalation` | `over_the_per_payment_cap_the_council_decides ... ok`: a 450 USDC invoice, over the 100 USDC cap, becomes a Transfer proposal, the council approves and the DAO pays. `an_unknown_recipient_escalates_with_a_clean_note ... ok`: a request to an account outside the allowlist, with markup and a line break in the memo, becomes a proposal with a clean one-line description |
| `cargo test -p sandbox-tests --test auth` | The NEAR runtime refuses the agent key for any other guard method, any other contract and any deposit. Only the DAO changes the policy |
| `cargo test -p sandbox-tests --test recovery` | A payout that fails rolls back the budget, the period spending and the `request_id`, and the same invoice can be retried |
| `cargo test -p sandbox-tests --test gas -- --nocapture` | Worst case gas of every call and callback next to the gas the guard reserves for it |

The default policy in these tests: one agent, one allowlisted vendor, USDC up to 100 per payment and 300 per day, NEAR up to 1 per payment and 3 per day, at most 10 payouts and 5 escalations a day. `sandbox-tests/README.md` lists every test file and the measured gas.

## Demo

The invoice agent and a one-command demo with three invoices are in progress: a routine invoice paid at once, a large one escalated to the DAO, and a poisoned one that cannot redirect the payment and reaches voters only as a clean note.

## Testnet

The setup runs on testnet under `macstian-tripwire.testnet`:

| Account | Role | Code hash |
| --- | --- | --- |
| [`guard.macstian-tripwire.testnet`](https://testnet.nearblocks.io/address/guard.macstian-tripwire.testnet) | The guard. No access keys, funded with 1 NEAR and 1,000 USDC | `E9uSivdvoVBBsKsqJG1LhfrxxpZCGCCtsRXqc8csTFtq` |
| [`dao.macstian-tripwire.testnet`](https://testnet.nearblocks.io/address/dao.macstian-tripwire.testnet) | Sputnik DAO v2 3.2, the code the mainnet factory deploys | `DtHZA1mUmGYeScEaUoUZYQGGFHSMpnjMCvkr5U9hJXuF` |
| [`usdc.macstian-tripwire.testnet`](https://testnet.nearblocks.io/address/usdc.macstian-tripwire.testnet) | Mock USDC, 6 decimals | `BS9RxQ4HKy5yW9xVnHTKwzoqbdSBqiAwSRARaa9ypSTw` |
| [`council.macstian-tripwire.testnet`](https://testnet.nearblocks.io/address/council.macstian-tripwire.testnet) | Only council member, also the guardian | |
| [`agent.macstian-tripwire.testnet`](https://testnet.nearblocks.io/address/agent.macstian-tripwire.testnet) | Holds the agent's function call key | |
| [`vendor.macstian-tripwire.testnet`](https://testnet.nearblocks.io/address/vendor.macstian-tripwire.testnet) | The one allowlisted recipient | |

`deployments/testnet.json` holds the same accounts, the agent key, the policy and the budgets. Check them with [near CLI](https://github.com/near/near-cli-rs), no account needed:

| Do | See |
| --- | --- |
| `near account list-keys guard.macstian-tripwire.testnet network-config testnet now` | An empty table: nobody holds a key to the guard |
| `near account list-keys agent.macstian-tripwire.testnet network-config testnet now` | The agent key: `only do ["request_payment"] function calls on guard.macstian-tripwire.testnet with an allowance of 0.25 NEAR` |
| `near contract call-function as-read-only guard.macstian-tripwire.testnet get_status json-args '{}' network-config testnet now` | The policy, the budgets, `"frozen": false` and this period's spending |
| `near contract call-function as-read-only dao.macstian-tripwire.testnet get_policy json-args '{}' network-config testnet now` | The `GuardRequestor` role with only `transfer:AddProposal` for the guard |
| `near contract call-function as-read-only dao.macstian-tripwire.testnet get_proposal json-args '{"id":2}' network-config testnet now` | The proposal the guard filed for the 450 USDC invoice below, a Transfer to the vendor, `Approved` |

Sample transactions from 7.11. The agent signed the first two with its function call key:

| Step | Transaction | Result |
| --- | --- | --- |
| The agent asks for 80 USDC, inside the policy | [`59AYNpYd…`](https://testnet.nearblocks.io/txns/59AYNpYdXF6cavFp4zG9YWT4TdpGjwVkry8XWnPtaNG7) | `Paid`. The guard pays the vendor at once from its budget. The `payment_executed` event shows 80 of the 300 USDC daily cap spent |
| The agent asks for 450 USDC, over the 100 USDC cap | [`AdMYZk9P…`](https://testnet.nearblocks.io/txns/AdMYZk9PZ8oRnBSLUq4ogSqW2peYWgubJ43CuQS6g9HF) | `Escalated`. The guard pays nothing and files Transfer proposal 2 in the DAO with the reason `EXCEEDS_PER_PAYMENT_CAP` |
| The council approves proposal 2 | [`6jNypsob…`](https://testnet.nearblocks.io/txns/6jNypsobHeUCwmd76R6CGLDvV78efAfVHbdbiDvDixYn) | The DAO pays the 450 USDC from its own treasury |

To build the same setup under your own account, move the committed `deployments/testnet.json` away and run `scripts/deploy-testnet.sh <master account>`. It checks everything it can before the first transaction and writes a new `deployments/testnet.json`. Steps and balances: `sandbox-tests/README.md`, section Testnet.

## Design decisions

| Question | Choice | Why |
| --- | --- | --- |
| Where the money sits | The guard holds a small budget, the treasury stays in the DAO | A bug in the guard can lose at most its budget. A guard that votes could approve any transfer from the treasury |
| Limit model | Per token, per payment and per fixed period, shared by all agents. The period rolls over lazily on the next call | The Safe Allowance model, with no cron job. A policy update keeps the period's spending |
| Request outside the policy | A Transfer proposal with a reason | The invoice can still be paid if the council agrees |
| Duplicate invoices | A `request_id` is required and stored before any cross-contract call. It is released only when the payout or the proposal fails | Retries are safe, and a paid invoice is never paid twice |
| Proposal text | One fixed line written by the guard. Agent text passes a character allowlist | Voters see the real recipient and amount, never raw invoice text. The vote is on the Transfer itself |
| Agent key | A function call key for `request_payment` on the guard only | The runtime refuses every other call, so a leaked key can only ask |
| Guard account | No access keys after deployment. Changes only through DAO proposals | Nobody can redeploy or drain the guard outside a vote |
| Emergency stop | `freeze` by the DAO or the guardian, `unfreeze` only by the DAO. The flag is not part of the policy | A fast stop and a deliberate restart. A policy update cannot unfreeze the guard |
| Failed calls | Every payout and escalation has a callback that rolls back the budget, the period spending and the `request_id` | No lost budget and no stuck invoices |
| Reserve | NEAR for storage, 1 yoctoNEAR deposits and proposal bonds is kept apart from the budget and checked before every outgoing NEAR | Bonds and storage never eat into the budget. When the reserve runs low the guard refuses requests instead of failing halfway |

## Security

`THREAT_MODEL.md` covers the assets, the actors and threats T1 to T9: a changed recipient, an inflated amount, spoofed text in proposals, a stolen agent key, failed callbacks, replayed invoices, policy changes outside a vote, fake tokens and keys on the guard account. Each mitigation names the check in the contract and the test that shows it. The residual risks are listed there too.

## Repository layout

| Path | What |
| --- | --- |
| `contracts/guard` | The guard contract |
| `contracts/mock-ft` | A NEP-141 test token with an open `mint`, for sandbox and testnet only |
| `sandbox-tests` | End-to-end tests on a local node and the testnet deploy (`src/deploy.rs`, `src/bin/deploy.rs`) |
| `sandbox-tests/res/sputnikdao2.wasm` | Sputnik DAO v2 3.2, the code the mainnet factory deploys |
| `scripts/deploy-testnet.sh` | The testnet deploy |
| `deployments/testnet.json` | What the testnet deploy created. No secrets |
| `scripts/fetch_sputnik_wasm.py` | Downloads the Sputnik wasm again and checks it against the factory |

Checks run before every change:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
cd contracts/guard && cargo near build non-reproducible-wasm
```

## Credits

- [near-sdk](https://github.com/near/near-sdk-rs) and near-contract-standards 5.30 for the contracts, [cargo-near](https://github.com/near/cargo-near) 0.22 for the wasm builds.
- [near-api](https://github.com/near/near-api-rs) 0.8 and [near-sandbox](https://github.com/near/near-sandbox-rs) 0.4 (node 2.14.0) for the tests and the deploy.
- [Sputnik DAO v2](https://github.com/near-daos/sputnik-dao-contract) 3.2, commit `2b0f34a`, used unmodified as the DAO.
- Reference designs: [Safe Allowance Module](https://github.com/safe-global/safe-modules/tree/main/modules/allowances) (per-token allowance with a reset period) and [Zodiac Roles Modifier v2](https://github.com/zodiaceco/zodiac-modifier-roles) (scoped permissions with allowances).
- Standards: NEP-141 (fungible tokens), NEP-145 (storage management), NEP-297 (events).
- tokio, anyhow, serde_json, reqwest and base64 in the test crate.

## License

MIT, see `LICENSE`.
