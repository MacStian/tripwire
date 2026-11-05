//! NEP-297 events. Every log line is `EVENT_JSON:{"standard":"tripwire","version":"1.0.0",...}`.

use near_sdk::json_types::U128;
use near_sdk::{near, AccountId};

use crate::TokenKey;

#[near(event_json(standard = "tripwire"))]
pub enum Event {
    #[event_version("1.0.0")]
    PolicyUpdated { by: AccountId },

    #[event_version("1.0.0")]
    GuardFrozen { by: AccountId },

    #[event_version("1.0.0")]
    GuardUnfrozen { by: AccountId },

    #[event_version("1.0.0")]
    BudgetFunded { funder: AccountId, token_id: TokenKey, amount: U128, new_budget: U128 },

    #[event_version("1.0.0")]
    BudgetWithdrawn {
        receiver_id: AccountId,
        token_id: TokenKey,
        amount: U128,
        remaining_budget: U128,
    },

    #[event_version("1.0.0")]
    ReserveWithdrawn { receiver_id: AccountId, amount: U128 },

    /// The transfer of a withdrawal failed. A budget withdrawal is credited back, and for
    /// `unallocated` NEAR the refund returns it to the guard.
    #[event_version("1.0.0")]
    WithdrawalFailed { receiver_id: AccountId, token_id: TokenKey, amount: U128, unallocated: bool },
}
