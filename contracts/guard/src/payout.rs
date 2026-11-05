//! In-policy payout and its callback.

use near_sdk::json_types::U128;
use near_sdk::{env, near, require, AccountId, NearToken, Promise};

use crate::events::Event;
use crate::external::ext_ft;
use crate::{
    promise_succeeded, Guard, GuardExt, PaymentOutcome, TokenKey, GAS_FT_TRANSFER, GAS_ON_PAYOUT,
};

#[near]
impl Guard {
    /// Never panics in a real callback. On success emits `payment_executed`. On failure it
    /// restores the budget, the period counters of the request's period and the request id,
    /// so the agent can retry with the same id once the cause is fixed.
    #[private]
    #[allow(clippy::too_many_arguments)]
    pub fn on_payout_complete(
        &mut self,
        request_id: String,
        agent: AccountId,
        recipient: AccountId,
        token_id: TokenKey,
        amount: U128,
        spent_in_period: U128,
        period_start: u64,
    ) -> PaymentOutcome {
        // A real callback always has the one result of the transfer it is chained to.
        // Without it this is a direct call, and there is nothing to restore.
        let Some(succeeded) = promise_succeeded() else {
            env::panic_str("ERR_NOT_A_CALLBACK");
        };
        if succeeded {
            Event::PaymentExecuted {
                request_id: request_id.clone(),
                agent,
                recipient,
                token_id,
                amount,
                spent_in_period,
            }
            .emit();
            PaymentOutcome::Paid { request_id }
        } else {
            self.undo_payout(&request_id, &token_id, amount.0, period_start);
            Event::PaymentFailed { request_id: request_id.clone(), recipient, token_id, amount }
                .emit();
            PaymentOutcome::PaymentFailed { request_id }
        }
    }
}

impl Guard {
    /// Section 4.1 of the spec. Every check that can fail runs before the promise, so a
    /// panic here reverts the whole call and nothing is debited.
    pub(crate) fn pay(
        &mut self,
        request_id: String,
        recipient: AccountId,
        token_id: TokenKey,
        amount: U128,
    ) -> Promise {
        let period_start = self.current_period_start();
        let mut requests = self.request_usage.rolled(period_start);
        require!(
            requests.payments_in_period < self.policy.max_payments_per_period,
            "ERR_PAYMENT_LIMIT_REACHED"
        );
        requests.payments_in_period += 1;
        self.request_usage = requests;
        self.processed_requests.insert(request_id.clone());

        self.debit(&token_id, amount.0);
        let mut usage = self.token_usage_now(&token_id);
        usage.spent_in_period = U128(usage.spent_in_period.0.saturating_add(amount.0));
        // Taken now, so the event reports the period of the payment even when the callback
        // runs after the period has ended.
        let spent_in_period = usage.spent_in_period;
        self.token_usage.insert(token_id.clone(), usage);

        // New entries must count in the storage the reserve check sees. LookupSet writes
        // through, the maps cache until flushed.
        self.token_usage.flush();
        self.ft_budgets.flush();
        self.assert_reserve(if token_id.is_none() { amount.0 } else { 1 });

        let transfer = match &token_id {
            None => Promise::new(recipient.clone()).transfer(NearToken::from_yoctonear(amount.0)),
            // Plain ft_transfer, never ft_transfer_call, so the recipient runs no code.
            Some(ft) => ext_ft::ext(ft.clone())
                .with_attached_deposit(NearToken::from_yoctonear(1))
                .with_static_gas(GAS_FT_TRANSFER)
                .ft_transfer(recipient.clone(), amount, None),
        };
        transfer.then(
            Self::ext(env::current_account_id()).with_static_gas(GAS_ON_PAYOUT).on_payout_complete(
                request_id,
                env::predecessor_account_id(),
                recipient,
                token_id,
                amount,
                spent_in_period,
                period_start,
            ),
        )
    }

    /// Saturating only, so it can never panic inside the callback. Counters of a period that
    /// has already ended stay as they are, because a new period starts from zero anyway.
    fn undo_payout(&mut self, request_id: &str, token_id: &TokenKey, amount: u128, start: u64) {
        self.credit(token_id, amount);
        if let Some(usage) = self.token_usage.get_mut(token_id) {
            if usage.period_start == start {
                usage.spent_in_period = U128(usage.spent_in_period.0.saturating_sub(amount));
            }
        }
        if self.request_usage.period_start == start {
            self.request_usage.payments_in_period =
                self.request_usage.payments_in_period.saturating_sub(1);
        }
        self.processed_requests.remove(request_id);
    }
}
