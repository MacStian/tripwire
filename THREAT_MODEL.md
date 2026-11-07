# Threat model

Scope: the Tripwire guard contract (`contracts/guard`), the AI agent that calls it and the Sputnik DAO it answers to. For each threat this file names the check in the contract and the test that shows it. Unit tests are in `contracts/guard/src/tests`, sandbox tests against a real Sputnik DAO v2 are in `sandbox-tests/tests`.

## 1. Assets

1. **DAO treasury.** Holds the main NEAR and token funds. The guard has no vote in the DAO and cannot move treasury funds. Its DAO role can only add Transfer proposals.
2. **Guard budget** (`near_budget`, `ft_budgets`). The capped allowance the DAO moves to the guard for routine payouts.
3. **Guard NEAR reserve.** Pays for storage, the 1 yoctoNEAR of each token transfer and the bond of each escalation.
4. **What voters see.** An escalated proposal must show the real recipient and amount.

## 2. Actors

| Actor | Trust | Can do |
| --- | --- | --- |
| Sputnik DAO (`dao_id`) | Trusted, the governance root | Through approved proposals: change the policy, unfreeze, fund and withdraw |
| Guard contract | Enforces the policy on chain | Checks agents, recipients, caps per payment and per period, expiry, freeze, rate limits and duplicate requests. Writes a fixed proposal description |
| Guardian (optional) | Semi-trusted responder | Can call `freeze`. Cannot unfreeze, change the policy or move funds |
| AI agent | **Untrusted**, may be prompt injected or lose its key | Holds only a function call key limited to `request_payment` on the guard |
| Invoice sender | **Adversarial** | Writes invoice text meant to steer the agent or mislead voters |

## 3. Threats and mitigations

### T1. Prompt injection changes the recipient

- **Attack.** A poisoned invoice tells the agent to pay `attacker.near` instead of the vendor.
- **Mitigation.** `request_payment` checks `policy.recipients` on chain. A recipient outside the list is never paid by the guard. The request becomes a Transfer proposal with reason `RECIPIENT_NOT_ALLOWLISTED` and the council votes on it.
- **Shown by.** `escalation.rs`: `an_unknown_recipient_escalates_with_a_clean_note`.

### T2. Prompt injection inflates the amount

- **Attack.** An invoice makes the agent ask for 5,000 USDC instead of 50 to an allowlisted vendor.
- **Mitigation.** A payment must fit `max_per_payment`, and `spent_in_period + amount` must fit `max_per_period`. Anything above escalates with `EXCEEDS_PER_PAYMENT_CAP` or `EXCEEDS_PERIOD_CAP`. Split requests still cannot pass the period cap, and the guard can never pay more than its funded budget.
- **Shown by.** `escalation.rs`: `over_the_per_payment_cap_the_council_decides`, `over_the_period_cap_the_next_payment_escalates`. Unit tests: `payouts_in_flight_count_against_the_period_cap`.

### T3. Spoofed fields in the proposal description

- **Attack.** Invoice text reaches the description of an escalated proposal and misleads voters, for example with markup or a line that claims a different recipient.
- **Mitigation.** The guard never copies raw agent text into the description. It writes a fixed `[Guard Escalation] ...` line with the real `receiver_id`, amount and token. Every value the agent controls is limited to a safe character set: `request_id` to `[A-Za-z0-9._-]` and at most 64 characters, account ids by protocol rules, and the memo through an allowlist `[A-Za-z0-9 .,_#/-]` that drops everything else, including line breaks, markup, HTML entities and Unicode lookalikes. The vote itself is always on the Transfer in the proposal kind, never on the text.
- **Shown by.** Unit tests: `clean_memo_keeps_only_safe_characters`, `the_memo_reaches_the_proposal_only_as_a_clean_note`, `request_id_charset`, `rejects_markup_in_the_request_id`. Sandbox: `an_unknown_recipient_escalates_with_a_clean_note` checks the full description.

### T4. Stolen agent key and escalation flooding

- **Attack.** An attacker with the agent key sends many requests outside the policy. Each escalation pays a proposal bond. If the council removes them as spam, the DAO keeps the bonds.
- **Mitigation.**
  1. The agent key is a function call key for `request_payment` on the guard only, with a bounded gas allowance. It cannot call any other method, any other contract, or attach a deposit.
  2. `max_escalations_per_period` and `max_payments_per_period` stop the agent within a period.
  3. Bonds come from the reserve above `min_near_reserve + near_budget`, never from the budget.
  4. The DAO or the guardian can `freeze` the guard at once. A frozen guard neither pays nor escalates.
  5. Policy validation rejects an account that is both an agent and a recipient.
- **Shown by.** `auth.rs`: `the_agent_key_only_reaches_request_payment` (`MethodNameMismatch`, `ReceiverMismatch`), `the_agent_key_cannot_attach_a_deposit` (`DepositWithFunctionCall`), `full_access_keys_of_other_accounts_are_not_agents`. `periods.rs`: `the_rate_limits_stop_the_agent`, `a_frozen_guard_neither_pays_nor_escalates`. Unit test: `rejects_an_agent_that_is_also_a_recipient`.

### T5. Asynchronous calls, failed callbacks and storage griefing

- **Attack.**
  1. Cross-contract calls on NEAR finish in later blocks. A second request could arrive before the first callback and spend the same allowance twice.
  2. A callback runs out of gas or panics, so its rollback never runs.
  3. A compromised agent sends many small payments to grow `processed_requests` until the guard cannot pay for its own storage.
- **Mitigation.**
  1. Budget, period spending, counters and `request_id` are written before the promise is created. A payout in flight already counts.
  2. Each callback's static gas is measured in the sandbox in its worst case and has at least 2x margin. `request_payment` requires a minimum prepaid gas that covers the call, the outgoing call and the callback. Callbacks are private, never unwrap promise results and use saturating arithmetic.
  3. One reserve invariant: after every method the free balance (balance minus storage stake) covers `min_near_reserve + near_budget`. Every outgoing NEAR amount is checked against it before the promise. `new` and `update_policy` also count the state they write. DAO withdrawals of the budget only need the NEAR they send, so the DAO can always take its funds back.
  4. `max_payments_per_period` and the key's gas allowance bound how fast storage can grow (residual risk 4).
  5. Payouts use plain `ft_transfer`, never `ft_transfer_call`, so a recipient contract runs no code during a payout.
  6. The guard's DAO role has only `transfer:AddProposal`, so it can only ever propose a Transfer.
- **Shown by.** `gas.rs`: `worst_case_gas_fits_the_reserved_gas`. Unit tests: `two_payouts_in_flight_one_fails_and_one_succeeds`, `failure_restores_the_budget_the_counters_and_the_request_id`, `failure_restores_the_escalation_count_and_the_request_id`, `reserve_floor`, `reserve_floor_rejects_one_yocto_more`, `init_needs_the_reserve`, `a_near_payout_never_takes_the_reserve`. `recovery.rs`: `a_failed_payout_is_rolled_back_and_can_be_retried`. `auth.rs`: `callbacks_and_init_are_private`.

### T6. Replayed or duplicate invoices

- **Attack.** The agent retries a timed out call or reads the same invoice twice and pays it twice.
- **Mitigation.** Every request carries a `request_id`, and the guard refuses an id it has already processed. A payout that fails frees its id again, so the same invoice can be retried. The agent derives the id itself from the parsed invoice and never copies an id from invoice text, so a poisoned invoice cannot use up the id of a future one.
- **Shown by.** `payments.rs`: `pays_usdc_inside_the_policy` (the repeat fails with `ERR_DUPLICATE_OR_INVALID_REQUEST_ID`). Unit tests: `a_paid_request_id_cannot_be_used_again`, `rejects_a_processed_request_id`. `recovery.rs`: `a_failed_payout_is_rolled_back_and_can_be_retried`.

### T7. Policy changes outside a vote, a mid-period reset or a silent unfreeze

- **Attack.**
  1. A single council member or the agent calls `update_policy` directly.
  2. A routine policy update resets the period spending to zero. This includes an update that moves the period grid.
  3. A policy proposal filed before an emergency freeze is approved after it and switches the guard back on.
- **Mitigation.** `update_policy`, `unfreeze` and all withdrawals require the DAO as caller, so only an approved proposal reaches them. `update_policy` keeps all usage counters and moves them to the new grid when the grid changes. `frozen` lives outside the policy, so only `freeze` and `unfreeze` change it.
- **Shown by.** `auth.rs`: `council_members_cannot_skip_the_vote`, `approved_dao_proposals_change_the_policy_and_withdraw`. `periods.rs`: `a_grid_change_keeps_the_spending`, `a_policy_proposal_filed_before_a_freeze_leaves_the_guard_frozen`.

### T8. A fake token creates budget entries

- **Attack.** Anyone deploys a contract that calls `ft_on_transfer` on the guard. If the guard credited every caller, each fake token would add a budget entry paid from the reserve.
- **Mitigation.** `ft_on_transfer` credits only tokens in `policy.token_limits`. For any other caller it returns the full amount, so a real token contract refunds the sender, and writes nothing.
- **Shown by.** `recovery.rs`: `tokens_outside_the_policy_are_refused` (guard state and storage usage unchanged). Unit test: `ft_on_transfer_refunds_an_unknown_token_and_writes_nothing`.

### T9. Access keys on the guard account

- **Attack.** The guard account keeps the full access key it was deployed with. Whoever holds it can sign transfers as the guard or deploy other code and bypass every check.
- **Mitigation.** After deploy and `new`, every access key of the guard account is deleted. The guard has no upgrade method. If one is added later, it goes through a DAO-only method, never through a key.
- **Shown by.** `harness.rs`: the setup checks that the guard's key list is empty.

## 4. Residual risks and non-goals

1. **Drain to an allowlisted recipient up to the period cap.** If the agent is compromised and an allowlisted vendor colludes, the agent can pay that vendor up to `max_per_period`, bounded by the guard's budget. Keep the cap and the budget small, like petty cash, and use `freeze` and the withdrawals when something looks wrong.
2. **Burst at a period boundary.** Fixed windows allow `max_per_period` right before a boundary and again right after it. `max_per_payment` limits each payout, and the funded budget is a hard ceiling.
3. **A broad DAO role for the guard.** If the guard joins a general requestor role instead of its own role, other members of that role may be able to remove its proposals. Give the guard a dedicated role with only `transfer:AddProposal`.
4. **Reserve exhaustion stops the guard.** A compromised agent that stays inside every cap can still use up the reserve over several periods through storage for `processed_requests`. The guard then refuses every request with `ERR_INSUFFICIENT_NEAR_RESERVE`. No funds are lost. Limits: `max_payments_per_period`, the key allowance, monitoring of `payment_executed` events, and `freeze`.
5. **Tokens sent with a plain `ft_transfer` are locked.** Only `ft_transfer_call` reaches `ft_on_transfer`. A Sputnik Transfer without `msg`, or any app that sends a plain `ft_transfer`, puts tokens on the guard that are never credited and cannot be withdrawn in this version. Fund tokens only with `ft_transfer_call`. A DAO-only sweep is future work. Shown by `recovery.rs`: `a_transfer_without_msg_is_not_credited`.
6. **Proposal bond drift.** `policy.proposal_bond` must equal the DAO's bond. If the DAO changes its bond without updating the guard, every escalation fails closed with `escalation_failed` until both match.
7. **Bonds of expired proposals.** An expired proposal returns its bond only after someone with `transfer:Finalize` finalizes it. With a bond of 0 this does not matter. Returned bonds land outside the budget and the DAO recovers them with `withdraw_unallocated_near` (`recovery.rs`: `a_returned_bond_goes_back_to_the_dao_as_unallocated_near`).
8. **Bare links in the memo.** The memo allowlist keeps `.` and `/`, so text such as `www.example.com/pay` can reach a description, and a renderer that turns plain text into links would show it as one. It cannot change the recipient or any other field. The agent builds the memo from the invoice number and vendor name and never copies links, and voters check the on-chain receiver.
9. **A grid change while a call is in flight.** If `update_policy` moves the period grid while a payout or escalation is in flight and that call then fails, its rollback no longer matches the stored period start. The budget and the `request_id` come back, the period counters stay. This fails closed: the amount stays counted until the period ends.
