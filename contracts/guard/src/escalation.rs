//! Escalation to the DAO: a Transfer proposal with a fixed-format description, and its callback.

use near_sdk::json_types::U128;
use near_sdk::{env, near, require, serde_json, AccountId, Promise};

use crate::events::Event;
use crate::external::{ext_sputnik, ProposalInput, ProposalKind};
use crate::{
    promise_outcome, EscalationReason, Guard, GuardExt, PaymentOutcome, PromiseOutcome, TokenKey,
    GAS_ADD_PROPOSAL, GAS_ON_ESCALATION,
};

/// Longest note kept from the agent's memo.
pub const MAX_NOTE_LEN: usize = 120;

/// A u64 in JSON has at most 20 digits. Anything longer is not a proposal id.
const MAX_PROPOSAL_ID_LEN: usize = 32;

/// Keeps only `[A-Za-z0-9 .,_#/-]`, drops every other character, cuts to `MAX_NOTE_LEN`
/// and trims. An allowlist, so no markup, separator or lookalike character survives.
pub fn clean_memo(memo: &str) -> String {
    let kept: String = memo
        .chars()
        .filter(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | ',' | '_' | '#' | '/' | '-')
        })
        .take(MAX_NOTE_LEN)
        .collect();
    kept.trim().to_string()
}

/// One fixed line. The guard never puts markup or agent-supplied keys into proposal
/// descriptions: the request id is checked in Step A, account ids are protocol-safe, the
/// amount is a number, the reason is a constant and the memo goes through `clean_memo`.
pub fn proposal_description(
    request_id: &str,
    reason: EscalationReason,
    recipient: &AccountId,
    token_id: &TokenKey,
    amount: U128,
    memo: Option<&str>,
) -> String {
    // Account ids are lowercase, so "NEAR" cannot be a token contract.
    let token = token_id.as_ref().map_or("NEAR", |ft| ft.as_str());
    let note = memo.map(clean_memo).unwrap_or_default();
    format!(
        "* Notes: [Guard Escalation] req={request_id} reason={} receiver={recipient} amount={} \
         token={token} note={note} <br>",
        reason.as_str(),
        amount.0,
    )
}

#[near]
impl Guard {
    /// Never panics in a real callback. On success emits `payment_escalated` with the proposal
    /// id, or `null` if the DAO's answer is not one. On failure it restores the escalation
    /// count of the request's period and the request id. The bond comes back in its own
    /// refund receipt.
    #[private]
    #[allow(clippy::too_many_arguments)]
    pub fn on_escalation_complete(
        &mut self,
        request_id: String,
        agent: AccountId,
        recipient: AccountId,
        token_id: TokenKey,
        amount: U128,
        reason: EscalationReason,
        period_start: u64,
    ) -> PaymentOutcome {
        match promise_outcome(MAX_PROPOSAL_ID_LEN) {
            // Only a direct call has no result, and then there is nothing to restore.
            PromiseOutcome::Missing => env::panic_str("ERR_NOT_A_CALLBACK"),
            PromiseOutcome::Succeeded(value) => {
                let proposal_id = value.and_then(|v| serde_json::from_slice::<u64>(&v).ok());
                Event::PaymentEscalated {
                    request_id: request_id.clone(),
                    agent,
                    recipient,
                    token_id,
                    amount,
                    reason,
                    proposal_id,
                }
                .emit();
                PaymentOutcome::Escalated { request_id, proposal_id }
            }
            PromiseOutcome::Failed => {
                if self.request_usage.period_start == period_start {
                    self.request_usage.escalations_in_period =
                        self.request_usage.escalations_in_period.saturating_sub(1);
                }
                self.processed_requests.remove(&request_id);
                Event::EscalationFailed {
                    request_id: request_id.clone(),
                    recipient,
                    token_id,
                    amount,
                    reason,
                }
                .emit();
                PaymentOutcome::EscalationFailed { request_id }
            }
        }
    }
}

impl Guard {
    /// Section 4.2 of the spec. The proposal asks the DAO to pay from its own treasury, so no
    /// budget moves. The bond comes from the reserve, never from `near_budget`.
    pub(crate) fn escalate(
        &mut self,
        request_id: String,
        recipient: AccountId,
        token_id: TokenKey,
        amount: U128,
        reason: EscalationReason,
        memo: Option<String>,
    ) -> Promise {
        let period_start = self.current_period_start();
        let mut requests = self.request_usage.rolled(period_start);
        require!(
            requests.escalations_in_period < self.policy.max_escalations_per_period,
            "ERR_ESCALATION_LIMIT_REACHED"
        );
        requests.escalations_in_period += 1;
        self.request_usage = requests;
        // LookupSet writes through, so the new entry already counts in the reserve check.
        self.processed_requests.insert(request_id.clone());
        let bond = self.policy.proposal_bond;
        self.assert_reserve(bond.as_yoctonear());

        let description = proposal_description(
            &request_id,
            reason,
            &recipient,
            &token_id,
            amount,
            memo.as_deref(),
        );
        let proposal = ProposalInput {
            description,
            kind: ProposalKind::Transfer {
                token_id: token_id.as_ref().map(|ft| ft.to_string()).unwrap_or_default(),
                receiver_id: recipient.clone(),
                amount,
                msg: None,
            },
        };
        ext_sputnik::ext(self.dao_id.clone())
            .with_attached_deposit(bond)
            .with_static_gas(GAS_ADD_PROPOSAL)
            .add_proposal(proposal)
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_ON_ESCALATION)
                    .on_escalation_complete(
                        request_id,
                        env::predecessor_account_id(),
                        recipient,
                        token_id,
                        amount,
                        reason,
                        period_start,
                    ),
            )
    }
}
