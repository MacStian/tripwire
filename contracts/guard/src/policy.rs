//! Policy types, validation and the lazy period grid.

use std::collections::BTreeSet;

use near_sdk::json_types::U128;
use near_sdk::{env, near, AccountId, NearToken};

/// `None` is native NEAR, `Some(token_id)` is a NEP-141 token contract.
pub type TokenKey = Option<AccountId>;

pub const MAX_AGENTS: usize = 10;
pub const MAX_RECIPIENTS: usize = 50;
pub const MAX_TOKENS: usize = 10;

/// Spending caps for one token.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenLimit {
    pub token_id: TokenKey,
    pub max_per_payment: U128,
    pub max_per_period: U128,
}

/// Everything the DAO controls through `update_policy`.
/// The freeze flag is deliberately not here, so a policy update can never unfreeze the guard.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Accounts allowed to call `request_payment`.
    pub agents: Vec<AccountId>,
    /// Recipients that may be paid without a DAO vote.
    pub recipients: Vec<AccountId>,
    /// Per-token caps. A token missing here is never paid directly.
    pub token_limits: Vec<TokenLimit>,
    /// Fixed window length in seconds.
    pub period_seconds: u64,
    /// Unix timestamp in seconds that anchors the period grid.
    pub period_anchor: u64,
    /// At or after this Unix timestamp (seconds) every request is rejected.
    pub expires_at: Option<u64>,
    /// May call `freeze`, never `unfreeze`.
    pub guardian: Option<AccountId>,
    pub max_payments_per_period: u32,
    pub max_escalations_per_period: u32,
    /// Must equal the DAO's own proposal bond exactly.
    pub proposal_bond: NearToken,
    /// NEAR that always stays on the guard for storage, deposits and bonds.
    pub min_near_reserve: NearToken,
}

impl Policy {
    pub fn token_limit(&self, token_id: &TokenKey) -> Option<&TokenLimit> {
        self.token_limits.iter().find(|l| &l.token_id == token_id)
    }

    pub fn is_agent(&self, account_id: &AccountId) -> bool {
        self.agents.contains(account_id)
    }

    pub fn is_recipient(&self, account_id: &AccountId) -> bool {
        self.recipients.contains(account_id)
    }
}

fn invalid(reason: &str) -> ! {
    env::panic_str(&format!("ERR_INVALID_POLICY: {reason}"))
}

fn has_duplicates<T: Ord>(items: impl IntoIterator<Item = T>) -> bool {
    let mut seen = BTreeSet::new();
    items.into_iter().any(|item| !seen.insert(item))
}

/// Rules shared by `new` and `update_policy`. Panics with `ERR_INVALID_POLICY: <rule>`.
pub fn validate_policy(policy: &Policy, now_sec: u64, guard_id: &AccountId) {
    if policy.agents.is_empty() || policy.agents.len() > MAX_AGENTS {
        invalid("agents");
    }
    if policy.recipients.len() > MAX_RECIPIENTS {
        invalid("recipients");
    }
    if policy.token_limits.is_empty() || policy.token_limits.len() > MAX_TOKENS {
        invalid("token_limits");
    }
    if has_duplicates(&policy.agents)
        || has_duplicates(&policy.recipients)
        || has_duplicates(policy.token_limits.iter().map(|l| &l.token_id))
    {
        invalid("duplicates");
    }
    for limit in &policy.token_limits {
        if limit.max_per_payment.0 == 0 || limit.max_per_payment.0 > limit.max_per_period.0 {
            invalid("token caps");
        }
    }
    if policy.period_seconds == 0 {
        invalid("period_seconds");
    }
    if policy.period_anchor > now_sec {
        invalid("period_anchor");
    }
    if matches!(policy.expires_at, Some(exp) if exp <= now_sec) {
        invalid("expires_at");
    }
    if policy.is_agent(guard_id) || policy.is_recipient(guard_id) {
        invalid("guard account in agents or recipients");
    }
}

/// Start of the period that contains `now_sec`, on the grid set by `anchor` and `period_seconds`.
pub fn current_period_start(now_sec: u64, anchor: u64, period_seconds: u64) -> u64 {
    if now_sec <= anchor {
        anchor
    } else {
        anchor + ((now_sec - anchor) / period_seconds) * period_seconds
    }
}

/// Spending of one token in the current period.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeriodUsage {
    pub period_start: u64,
    pub spent_in_period: U128,
}

impl PeriodUsage {
    /// The usage as seen at `period_start`. A stale period reads as zero.
    pub fn rolled(&self, period_start: u64) -> Self {
        if self.period_start == period_start {
            self.clone()
        } else {
            Self { period_start, spent_in_period: U128(0) }
        }
    }
}

/// Payouts and escalations in the current period.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestUsage {
    pub period_start: u64,
    pub payments_in_period: u32,
    pub escalations_in_period: u32,
}

impl RequestUsage {
    pub fn rolled(&self, period_start: u64) -> Self {
        if self.period_start == period_start {
            self.clone()
        } else {
            Self { period_start, ..Default::default() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_start_before_and_at_anchor() {
        assert_eq!(current_period_start(50, 100, 10), 100);
        assert_eq!(current_period_start(100, 100, 10), 100);
    }

    #[test]
    fn period_start_on_the_grid() {
        assert_eq!(current_period_start(109, 100, 10), 100);
        assert_eq!(current_period_start(110, 100, 10), 110);
        assert_eq!(current_period_start(125, 100, 10), 120);
    }

    #[test]
    fn stale_usage_reads_as_zero() {
        let usage = PeriodUsage { period_start: 100, spent_in_period: U128(7) };
        assert_eq!(usage.rolled(100).spent_in_period, U128(7));
        assert_eq!(usage.rolled(110).spent_in_period, U128(0));
        assert_eq!(usage.rolled(110).period_start, 110);
    }

    #[test]
    fn duplicates_are_found() {
        assert!(has_duplicates([1, 2, 1]));
        assert!(!has_duplicates([1, 2, 3]));
    }
}
