//! Interfaces of the contracts the guard calls.

use near_sdk::json_types::U128;
use near_sdk::{ext_contract, near, AccountId};

/// The part of Sputnik's `ProposalInput` the guard sends.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposalInput {
    pub description: String,
    pub kind: ProposalKind,
}

/// The guard files only `Transfer`, so the DAO grants it only `transfer:AddProposal`.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalKind {
    /// `token_id` is `""` for NEAR, otherwise the NEP-141 contract.
    Transfer { token_id: String, receiver_id: AccountId, amount: U128, msg: Option<String> },
}

// Only the generated ext_* modules are used, not the traits themselves.
#[ext_contract(ext_ft)]
#[allow(dead_code)]
trait FungibleToken {
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>);
}

#[ext_contract(ext_sputnik)]
#[allow(dead_code)]
trait SputnikDao {
    /// Returns the new proposal id.
    fn add_proposal(&mut self, proposal: ProposalInput) -> u64;
}
