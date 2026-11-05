//! Interfaces of the contracts the guard calls.

use near_sdk::json_types::U128;
use near_sdk::{ext_contract, AccountId};

// Only the generated ext_ft module is used, not the trait itself.
#[ext_contract(ext_ft)]
#[allow(dead_code)]
trait FungibleToken {
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>);
}
