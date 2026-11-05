//! Funding the budgets and DAO withdrawals.

use near_sdk::json_types::U128;
use near_sdk::{env, near, require, AccountId, NearToken, Promise};

use crate::events::Event;
use crate::external::ext_ft;
use crate::{promise_succeeded, Guard, GuardExt, TokenKey, GAS_FT_TRANSFER, GAS_ON_WITHDRAW};

/// Where a withdrawal takes funds from. Passed to `on_withdraw_complete`.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Withdrawal {
    /// `near_budget` (`None`) or `ft_budgets[token]` (`Some`).
    Budget(TokenKey),
    /// NEAR outside every budget: returned bonds, plain transfers, leftovers.
    Unallocated,
}

#[near]
impl Guard {
    /// Adds the attached NEAR to the spendable budget. A plain transfer runs no code
    /// and stays outside the budget.
    #[payable]
    pub fn fund(&mut self) -> U128 {
        let amount = env::attached_deposit().as_yoctonear();
        require!(amount > 0, "ERR_ZERO_AMOUNT");
        let new_budget = self.credit(&None, amount);
        Event::BudgetFunded {
            funder: env::predecessor_account_id(),
            token_id: None,
            amount: U128(amount),
            new_budget: U128(new_budget),
        }
        .emit();
        U128(new_budget)
    }

    /// NEP-141 receiver. Credits only tokens in the policy. For any other token it returns
    /// the full amount, so the token contract refunds the sender, and writes nothing.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> U128 {
        let _ = msg;
        let token_id = Some(env::predecessor_account_id());
        if amount.0 == 0 || self.policy.token_limit(&token_id).is_none() {
            return amount;
        }
        let new_budget = self.credit(&token_id, amount.0);
        // The first deposit of a token adds a storage entry. If the reserve cannot pay for it,
        // this panics and the token contract refunds the sender.
        self.ft_budgets.flush();
        self.assert_reserve(0);
        Event::BudgetFunded { funder: sender_id, token_id, amount, new_budget: U128(new_budget) }
            .emit();
        U128(0)
    }

    /// DAO only. Sends NEAR from `near_budget` to `receiver_id` (default: the DAO).
    pub fn withdraw_near(&mut self, amount: U128, receiver_id: Option<AccountId>) -> Promise {
        self.assert_dao();
        self.debit(&None, amount.0);
        self.assert_reserve(amount.0);
        let receiver_id = receiver_id.unwrap_or_else(|| self.dao_id.clone());
        Promise::new(receiver_id.clone())
            .transfer(NearToken::from_yoctonear(amount.0))
            .then(Self::withdraw_callback(Withdrawal::Budget(None), amount, receiver_id))
    }

    /// DAO only. Sends tokens from `ft_budgets[token_id]` with a plain `ft_transfer`.
    /// The receiver must be registered on the token contract, otherwise the transfer fails
    /// and the callback restores the budget.
    pub fn withdraw_ft(
        &mut self,
        token_id: AccountId,
        amount: U128,
        receiver_id: Option<AccountId>,
    ) -> Promise {
        self.assert_dao();
        let key = Some(token_id.clone());
        self.debit(&key, amount.0);
        self.assert_reserve(1);
        let receiver_id = receiver_id.unwrap_or_else(|| self.dao_id.clone());
        ext_ft::ext(token_id)
            .with_attached_deposit(NearToken::from_yoctonear(1))
            .with_static_gas(GAS_FT_TRANSFER)
            .ft_transfer(receiver_id.clone(), amount, None)
            .then(Self::withdraw_callback(Withdrawal::Budget(key), amount, receiver_id))
    }

    /// DAO only. Sends NEAR that is outside `near_budget` and above `min_near_reserve`.
    /// Changes no budget.
    pub fn withdraw_unallocated_near(
        &mut self,
        amount: U128,
        receiver_id: Option<AccountId>,
    ) -> Promise {
        self.assert_dao();
        require!(amount.0 > 0, "ERR_ZERO_AMOUNT");
        self.assert_reserve(amount.0);
        let receiver_id = receiver_id.unwrap_or_else(|| self.dao_id.clone());
        Promise::new(receiver_id.clone())
            .transfer(NearToken::from_yoctonear(amount.0))
            .then(Self::withdraw_callback(Withdrawal::Unallocated, amount, receiver_id))
    }

    /// Never panics. On success emits the withdrawal event. On failure restores the budget.
    /// A failed NEAR transfer comes back to the guard in its own refund receipt.
    #[private]
    pub fn on_withdraw_complete(
        &mut self,
        withdrawal: Withdrawal,
        amount: U128,
        receiver_id: AccountId,
    ) -> bool {
        let Some(succeeded) = promise_succeeded() else {
            return false;
        };
        match (withdrawal, succeeded) {
            (Withdrawal::Budget(token_id), true) => {
                let remaining_budget = U128(self.budget_of(&token_id));
                Event::BudgetWithdrawn { receiver_id, token_id, amount, remaining_budget }.emit();
            }
            (Withdrawal::Budget(token_id), false) => {
                self.credit(&token_id, amount.0);
            }
            (Withdrawal::Unallocated, true) => {
                Event::ReserveWithdrawn { receiver_id, amount }.emit();
            }
            (Withdrawal::Unallocated, false) => {}
        }
        succeeded
    }
}

impl Guard {
    /// Adds to a budget and returns the new value. Saturates, so it is safe in callbacks.
    pub(crate) fn credit(&mut self, token_id: &TokenKey, amount: u128) -> u128 {
        let new_budget = self.budget_of(token_id).saturating_add(amount);
        match token_id {
            None => self.near_budget = new_budget,
            Some(ft) => {
                self.ft_budgets.insert(ft.clone(), new_budget);
            }
        }
        new_budget
    }

    /// Takes from a budget or panics. Never creates a storage entry for an unknown token.
    pub(crate) fn debit(&mut self, token_id: &TokenKey, amount: u128) {
        require!(amount > 0, "ERR_ZERO_AMOUNT");
        let budget = self.budget_of(token_id);
        require!(amount <= budget, "ERR_INSUFFICIENT_BUDGET");
        match token_id {
            None => self.near_budget = budget - amount,
            Some(ft) => {
                self.ft_budgets.insert(ft.clone(), budget - amount);
            }
        }
    }

    fn withdraw_callback(withdrawal: Withdrawal, amount: U128, receiver_id: AccountId) -> Promise {
        Self::ext(env::current_account_id()).with_static_gas(GAS_ON_WITHDRAW).on_withdraw_complete(
            withdrawal,
            amount,
            receiver_id,
        )
    }
}
