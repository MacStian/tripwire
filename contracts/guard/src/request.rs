//! Checks for `request_payment`: hard preconditions (Step A) and the policy decision (Step B).

use near_sdk::json_types::U128;
use near_sdk::{env, near, require, AccountId, Gas};

use crate::{now_sec, Guard, TokenKey};

pub const MAX_REQUEST_ID_LEN: usize = 64;

/// Starting point until the sandbox gas tests (E2-S2) set the final value. Covers the outgoing
/// call (`ft_transfer` or `add_proposal`) plus its callback, with a clear error when too low.
pub const MIN_PREPAID_GAS: Gas = Gas::from_tgas(50);

/// 1 to 64 characters from `[A-Za-z0-9._-]`. The id is written into proposal descriptions,
/// so it must never carry markup or separators.
pub fn is_valid_request_id(request_id: &str) -> bool {
    (1..=MAX_REQUEST_ID_LEN).contains(&request_id.len())
        && request_id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Why a request goes to a DAO vote instead of being paid. Checked in this order.
#[near(serializers = [json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EscalationReason {
    RecipientNotAllowlisted,
    TokenNotAllowed,
    ExceedsPerPaymentCap,
    ExceedsPeriodCap,
    ExceedsGuardBudget,
}

impl EscalationReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RecipientNotAllowlisted => "RECIPIENT_NOT_ALLOWLISTED",
            Self::TokenNotAllowed => "TOKEN_NOT_ALLOWED",
            Self::ExceedsPerPaymentCap => "EXCEEDS_PER_PAYMENT_CAP",
            Self::ExceedsPeriodCap => "EXCEEDS_PERIOD_CAP",
            Self::ExceedsGuardBudget => "EXCEEDS_GUARD_BUDGET",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Pay,
    Escalate(EscalationReason),
}

impl Guard {
    /// Step A. Panics on the first failed precondition and changes nothing.
    pub fn assert_request_allowed(&self, request_id: &str, amount: U128) {
        require!(env::prepaid_gas() >= MIN_PREPAID_GAS, "ERR_INSUFFICIENT_PREPAID_GAS");
        require!(self.policy.is_agent(&env::predecessor_account_id()), "ERR_UNAUTHORIZED_AGENT");
        require!(!self.frozen, "ERR_GUARD_FROZEN");
        if let Some(expires_at) = self.policy.expires_at {
            require!(now_sec() < expires_at, "ERR_GRANT_EXPIRED");
        }
        require!(amount.0 > 0, "ERR_ZERO_AMOUNT");
        require!(
            is_valid_request_id(request_id) && !self.processed_requests.contains(request_id),
            "ERR_DUPLICATE_OR_INVALID_REQUEST_ID"
        );
    }

    /// Step B. `Pay` when all five conditions hold, otherwise the first one that fails.
    /// Usage is read on the current period, so a finished period counts as zero.
    pub fn evaluate(&self, recipient: &AccountId, token_id: &TokenKey, amount: u128) -> Decision {
        use EscalationReason::*;
        if !self.policy.is_recipient(recipient) {
            return Decision::Escalate(RecipientNotAllowlisted);
        }
        let Some(limit) = self.policy.token_limit(token_id) else {
            return Decision::Escalate(TokenNotAllowed);
        };
        if amount > limit.max_per_payment.0 {
            return Decision::Escalate(ExceedsPerPaymentCap);
        }
        if self.spent_in_period(token_id).saturating_add(amount) > limit.max_per_period.0 {
            return Decision::Escalate(ExceedsPeriodCap);
        }
        if amount > self.budget_of(token_id) {
            return Decision::Escalate(ExceedsGuardBudget);
        }
        Decision::Pay
    }
}
