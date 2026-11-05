//! Tripwire: a budget guard for a Sputnik DAO.
//!
//! The DAO funds the guard with a small budget. An agent asks the guard to pay. Inside the
//! policy the guard pays from its budget. Outside the policy it files a Transfer proposal and
//! humans vote. Only the DAO can change the policy, unfreeze the guard or withdraw funds.
//! Specification: `docs/contract-spec.md` in the planning repo.

mod escalation;
pub mod events;
mod external;
mod funding;
mod governance;
mod payout;
mod policy;
mod request;

pub use escalation::{clean_memo, proposal_description, MAX_NOTE_LEN};
pub use external::{ProposalInput, ProposalKind};
pub use funding::Withdrawal;
pub use policy::*;
pub use request::*;

use near_sdk::json_types::U128;
use near_sdk::store::{LookupMap, LookupSet};
use near_sdk::{env, near, require, AccountId, BorshStorageKey, Gas, PanicOnDefault, PromiseError};

// Starting points. The sandbox gas tests (E2-S2) measure the worst case and set the final values.
pub(crate) const GAS_FT_TRANSFER: Gas = Gas::from_tgas(15);
pub(crate) const GAS_ON_WITHDRAW: Gas = Gas::from_tgas(10);
pub(crate) const GAS_ON_PAYOUT: Gas = Gas::from_tgas(10);
pub(crate) const GAS_ADD_PROPOSAL: Gas = Gas::from_tgas(25);
pub(crate) const GAS_ON_ESCALATION: Gas = Gas::from_tgas(10);

#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    FtBudgets,
    TokenUsage,
    ProcessedRequests,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Guard {
    dao_id: AccountId,
    policy: Policy,
    /// Changed only by `freeze` and `unfreeze`, never by `update_policy`.
    frozen: bool,
    /// Spendable NEAR, credited only by `fund`.
    near_budget: u128,
    /// Spendable NEP-141 balances, credited only through `ft_on_transfer` for allowed tokens.
    ft_budgets: LookupMap<AccountId, u128>,
    token_usage: LookupMap<TokenKey, PeriodUsage>,
    request_usage: RequestUsage,
    /// Request ids of payouts and escalations that are in flight or done.
    processed_requests: LookupSet<String>,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenStatus {
    pub token_id: TokenKey,
    pub budget: U128,
    pub max_per_payment: U128,
    pub max_per_period: U128,
    pub spent_in_period: U128,
    pub remaining_in_period: U128,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct GuardStatus {
    pub dao_id: AccountId,
    pub policy: Policy,
    pub frozen: bool,
    pub near_budget: U128,
    pub tokens: Vec<TokenStatus>,
    pub request_usage: RequestUsage,
    /// NEAR above `min_near_reserve + near_budget`, available for bonds and deposits.
    pub free_reserve: U128,
}

pub(crate) fn now_sec() -> u64 {
    env::block_timestamp_ms() / 1000
}

/// Account balance minus the NEAR locked for storage.
pub(crate) fn free_near_balance() -> u128 {
    let staked = u128::from(env::storage_usage()) * env::storage_byte_cost().as_yoctonear();
    env::account_balance().as_yoctonear().saturating_sub(staked)
}

/// Result of the single promise a callback waits for.
pub(crate) enum PromiseOutcome {
    /// The method was not called as a callback of exactly one promise.
    Missing,
    Failed,
    /// The value, or `None` when it is longer than the limit the caller asked for.
    Succeeded(Option<Vec<u8>>),
}

/// Reads the promise result with a length limit. Never panics, so callbacks can always finish.
pub(crate) fn promise_outcome(max_len: usize) -> PromiseOutcome {
    if env::promise_results_count() != 1 {
        return PromiseOutcome::Missing;
    }
    match env::promise_result_checked(0, max_len) {
        Ok(value) => PromiseOutcome::Succeeded(Some(value)),
        Err(PromiseError::TooLong(_)) => PromiseOutcome::Succeeded(None),
        Err(_) => PromiseOutcome::Failed,
    }
}

/// `None` when the method was not called as a callback of exactly one promise.
pub(crate) fn promise_succeeded() -> Option<bool> {
    // A length limit of 0 avoids copying the result. A longer result still means success.
    match promise_outcome(0) {
        PromiseOutcome::Missing => None,
        PromiseOutcome::Failed => Some(false),
        PromiseOutcome::Succeeded(_) => Some(true),
    }
}

#[near]
impl Guard {
    #[init]
    #[private]
    pub fn new(dao_id: AccountId, policy: Policy) -> Self {
        let now = now_sec();
        validate_policy(&policy, now, &env::current_account_id());
        let period_start = current_period_start(now, policy.period_anchor, policy.period_seconds);
        Self {
            dao_id,
            policy,
            frozen: false,
            near_budget: 0,
            ft_budgets: LookupMap::new(StorageKey::FtBudgets),
            token_usage: LookupMap::new(StorageKey::TokenUsage),
            request_usage: RequestUsage { period_start, ..Default::default() },
            processed_requests: LookupSet::new(StorageKey::ProcessedRequests),
        }
    }

    pub fn get_policy(&self) -> Policy {
        self.policy.clone()
    }

    pub fn get_status(&self) -> GuardStatus {
        let tokens = self
            .policy
            .token_limits
            .iter()
            .filter_map(|l| self.get_token_status(l.token_id.clone()))
            .collect();
        let locked = self.policy.min_near_reserve.as_yoctonear() + self.near_budget;
        GuardStatus {
            dao_id: self.dao_id.clone(),
            policy: self.policy.clone(),
            frozen: self.frozen,
            near_budget: U128(self.near_budget),
            tokens,
            request_usage: self.request_usage.rolled(self.current_period_start()),
            free_reserve: U128(free_near_balance().saturating_sub(locked)),
        }
    }

    /// `None` when the token is not in the policy.
    pub fn get_token_status(&self, token_id: TokenKey) -> Option<TokenStatus> {
        let limit = self.policy.token_limit(&token_id)?;
        let spent = self.spent_in_period(&token_id);
        Some(TokenStatus {
            budget: U128(self.budget_of(&token_id)),
            max_per_payment: limit.max_per_payment,
            max_per_period: limit.max_per_period,
            spent_in_period: U128(spent),
            remaining_in_period: U128(limit.max_per_period.0.saturating_sub(spent)),
            token_id,
        })
    }

    pub fn is_request_processed(&self, request_id: String) -> bool {
        self.processed_requests.contains(&request_id)
    }
}

impl Guard {
    pub(crate) fn assert_dao(&self) {
        require!(env::predecessor_account_id() == self.dao_id, "ERR_NOT_DAO");
    }

    /// Invariant I1: the NEAR left after `outgoing_near` covers `min_near_reserve` and the
    /// spendable `near_budget`. Call it after the method's own debits and before the promise.
    /// The contract root state is written after the method returns, so `min_near_reserve`
    /// must also cover small growth of the policy itself.
    pub(crate) fn assert_reserve(&self, outgoing_near: u128) {
        let required = self
            .policy
            .min_near_reserve
            .as_yoctonear()
            .saturating_add(self.near_budget)
            .saturating_add(outgoing_near);
        require!(free_near_balance() >= required, "ERR_INSUFFICIENT_NEAR_RESERVE");
    }

    fn current_period_start(&self) -> u64 {
        current_period_start(now_sec(), self.policy.period_anchor, self.policy.period_seconds)
    }

    /// Spending of `token_id` in the current period. A finished period reads as zero.
    fn token_usage_now(&self, token_id: &TokenKey) -> PeriodUsage {
        let start = self.current_period_start();
        self.token_usage
            .get(token_id)
            .map(|u| u.rolled(start))
            .unwrap_or(PeriodUsage { period_start: start, spent_in_period: U128(0) })
    }

    fn spent_in_period(&self, token_id: &TokenKey) -> u128 {
        self.token_usage_now(token_id).spent_in_period.0
    }

    fn budget_of(&self, token_id: &TokenKey) -> u128 {
        match token_id {
            None => self.near_budget,
            Some(ft) => self.ft_budgets.get(ft).copied().unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests;
