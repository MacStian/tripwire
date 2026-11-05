//! Tripwire: a budget guard for a Sputnik DAO.
//!
//! The DAO funds the guard with a small budget. An agent asks the guard to pay. Inside the
//! policy the guard pays from its budget. Outside the policy it files a Transfer proposal and
//! humans vote. Only the DAO can change the policy, unfreeze the guard or withdraw funds.
//! Specification: `docs/contract-spec.md` in the planning repo.

mod policy;

pub use policy::*;

use near_sdk::json_types::U128;
use near_sdk::store::{LookupMap, LookupSet};
use near_sdk::{env, near, AccountId, BorshStorageKey, PanicOnDefault};

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
