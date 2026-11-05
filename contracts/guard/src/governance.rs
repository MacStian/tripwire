//! DAO controls: policy updates, freeze and unfreeze.

use std::collections::BTreeSet;

use near_sdk::{env, near, require};

use crate::events::Event;
use crate::{
    borsh_len, current_period_start, now_sec, storage_cost, validate_policy, Guard, GuardExt,
    Policy, TokenKey,
};

#[near]
impl Guard {
    /// DAO only. Replaces the policy. Never touches `frozen` and never refills a period:
    /// usage counters are kept, also when the period grid changes.
    pub fn update_policy(&mut self, policy: Policy) {
        self.assert_dao();
        validate_policy(&policy, now_sec(), &env::current_account_id());
        let old = std::mem::replace(&mut self.policy, policy);
        self.rebase_usage(&old);
        // The larger policy is written after this method returns, so its growth counts here.
        let growth = borsh_len(&self.policy).saturating_sub(borsh_len(&old));
        self.assert_reserve(storage_cost(growth));
        Event::PolicyUpdated { by: env::predecessor_account_id() }.emit();
    }

    /// The DAO or the policy guardian. Stops every agent request until the DAO unfreezes.
    pub fn freeze(&mut self) {
        let caller = env::predecessor_account_id();
        require!(
            caller == self.dao_id || self.policy.guardian.as_ref() == Some(&caller),
            "ERR_NOT_DAO_OR_GUARDIAN"
        );
        self.frozen = true;
        Event::GuardFrozen { by: caller }.emit();
    }

    /// DAO only. The guardian can freeze but never unfreeze.
    pub fn unfreeze(&mut self) {
        self.assert_dao();
        self.frozen = false;
        Event::GuardUnfrozen { by: env::predecessor_account_id() }.emit();
    }
}

impl Guard {
    /// After a policy change, keeps the counters of the current period on the new grid.
    /// Without this, a changed `period_seconds` or `period_anchor` would make the next lazy
    /// read see a different period start and reset the counters to zero.
    fn rebase_usage(&mut self, old: &Policy) {
        let new = &self.policy;
        if old.period_seconds == new.period_seconds && old.period_anchor == new.period_anchor {
            return;
        }
        let now = now_sec();
        let old_start = current_period_start(now, old.period_anchor, old.period_seconds);
        let new_start = current_period_start(now, new.period_anchor, new.period_seconds);

        self.request_usage = self.request_usage.rebased(old_start, new_start);

        let tokens: BTreeSet<TokenKey> =
            old.token_limits.iter().chain(&new.token_limits).map(|l| l.token_id.clone()).collect();
        for token in tokens {
            if let Some(usage) = self.token_usage.get(&token) {
                let moved = usage.rebased(old_start, new_start);
                self.token_usage.insert(token, moved);
            }
        }
    }
}
