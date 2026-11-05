//! Tripwire: a budget guard for a Sputnik DAO.
//!
//! The DAO funds the guard with a small budget. An agent asks the guard to pay. Inside the
//! policy the guard pays from its budget. Outside the policy it files a Transfer proposal and
//! humans vote. Only the DAO can change the policy, unfreeze the guard or withdraw funds.
//! Specification: `docs/contract-spec.md` in the planning repo.

pub mod events;
mod funding;
mod governance;
mod policy;

pub use funding::Withdrawal;
pub use policy::*;

use near_sdk::json_types::U128;
use near_sdk::store::{LookupMap, LookupSet};
use near_sdk::{env, near, require, AccountId, BorshStorageKey, Gas, PanicOnDefault, PromiseError};

// Starting points. The sandbox gas tests (E2-S2) measure the worst case and set the final values.
pub(crate) const GAS_FT_TRANSFER: Gas = Gas::from_tgas(15);
pub(crate) const GAS_ON_WITHDRAW: Gas = Gas::from_tgas(15);
pub(crate) const GAS_FAIL_WITHDRAWAL: Gas = Gas::from_tgas(5);

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

/// NEAR that `bytes` of storage lock.
pub(crate) fn storage_cost(bytes: usize) -> u128 {
    (bytes as u128).saturating_mul(env::storage_byte_cost().as_yoctonear())
}

/// Borsh size of a value, for storage that is written only after the method returns.
pub(crate) fn borsh_len<T: near_sdk::borsh::BorshSerialize>(value: &T) -> usize {
    near_sdk::borsh::to_vec(value).map(|bytes| bytes.len()).unwrap_or(usize::MAX)
}

/// Storage of the root state record beyond its value: the `STATE` key and the record overhead.
const STATE_RECORD_OVERHEAD: usize = 5 + 40;

/// Outcome of the single promise a callback waits for. `None` when the method was not called
/// as a callback of exactly one promise. Never panics, so callbacks can always finish.
pub(crate) fn promise_succeeded() -> Option<bool> {
    if env::promise_results_count() != 1 {
        return None;
    }
    // A length limit of 0 avoids copying the result. A longer result still means success.
    Some(matches!(env::promise_result_checked(0, 0), Ok(_) | Err(PromiseError::TooLong(_))))
}

#[near]
impl Guard {
    #[init]
    #[private]
    pub fn new(dao_id: AccountId, policy: Policy) -> Self {
        let now = now_sec();
        validate_policy(&policy, now, &env::current_account_id());
        let period_start = current_period_start(now, policy.period_anchor, policy.period_seconds);
        let guard = Self {
            dao_id,
            policy,
            frozen: false,
            near_budget: 0,
            ft_budgets: LookupMap::new(StorageKey::FtBudgets),
            token_usage: LookupMap::new(StorageKey::TokenUsage),
            request_usage: RequestUsage { period_start, ..Default::default() },
            processed_requests: LookupSet::new(StorageKey::ProcessedRequests),
        };
        // I1 from the start. The state is written after `new` returns, so its storage is
        // counted here by hand.
        guard.assert_reserve(storage_cost(borsh_len(&guard) + STATE_RECORD_OVERHEAD));
        guard
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
        let spent = self
            .token_usage
            .get(&token_id)
            .map(|u| u.rolled(self.current_period_start()).spent_in_period.0)
            .unwrap_or(0);
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
    /// The root state is written after the method returns, so a method that grows it (`new`,
    /// `update_policy`) adds the cost of that growth to `outgoing_near`.
    pub(crate) fn assert_reserve(&self, outgoing_near: u128) {
        let required = self
            .policy
            .min_near_reserve
            .as_yoctonear()
            .saturating_add(self.near_budget)
            .saturating_add(outgoing_near);
        require!(free_near_balance() >= required, "ERR_INSUFFICIENT_NEAR_RESERVE");
    }

    /// For DAO withdrawals from a budget: the guard only needs the NEAR it sends. Moving funds
    /// out of a budget never makes an I1 shortfall worse (apart from the 1 yoctoNEAR an
    /// `ft_transfer` needs), and a guard that is already short must still return its funds.
    pub(crate) fn assert_can_send(&self, outgoing_near: u128) {
        require!(free_near_balance() >= outgoing_near, "ERR_INSUFFICIENT_NEAR_RESERVE");
    }

    fn current_period_start(&self) -> u64 {
        current_period_start(now_sec(), self.policy.period_anchor, self.policy.period_seconds)
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
