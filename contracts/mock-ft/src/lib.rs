//! Minimal NEP-141 + NEP-145 token for sandbox and testnet setups.
//!
//! `mint` is open to anyone. This contract is a test fixture and must never hold real value.

use near_contract_standards::fungible_token::events::FtMint;
use near_contract_standards::fungible_token::metadata::{
    FungibleTokenMetadata, FungibleTokenMetadataProvider, FT_METADATA_SPEC,
};
use near_contract_standards::fungible_token::{
    FungibleToken, FungibleTokenCore, FungibleTokenResolver,
};
use near_contract_standards::storage_management::{
    StorageBalance, StorageBalanceBounds, StorageManagement,
};
use near_sdk::json_types::U128;
use near_sdk::{log, near, AccountId, BorshStorageKey, NearToken, PanicOnDefault, PromiseOrValue};

#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    Token,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockFt {
    token: FungibleToken,
    symbol: String,
    decimals: u8,
}

#[near]
impl MockFt {
    #[init]
    pub fn new(symbol: String, decimals: u8) -> Self {
        Self { token: FungibleToken::new(StorageKey::Token), symbol, decimals }
    }

    /// Mints to an account that is already registered with `storage_deposit`.
    pub fn mint(&mut self, account_id: AccountId, amount: U128) {
        self.token.internal_deposit(&account_id, amount.0);
        FtMint { owner_id: &account_id, amount, memo: Some("mock mint") }.emit();
    }
}

#[near]
impl FungibleTokenCore for MockFt {
    #[payable]
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>) {
        self.token.ft_transfer(receiver_id, amount, memo)
    }

    #[payable]
    fn ft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<U128> {
        self.token.ft_transfer_call(receiver_id, amount, memo, msg)
    }

    fn ft_total_supply(&self) -> U128 {
        self.token.ft_total_supply()
    }

    fn ft_balance_of(&self, account_id: AccountId) -> U128 {
        self.token.ft_balance_of(account_id)
    }
}

#[near]
impl FungibleTokenResolver for MockFt {
    #[private]
    fn ft_resolve_transfer(
        &mut self,
        sender_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
    ) -> U128 {
        let (used, burned) = self.token.internal_ft_resolve_transfer(&sender_id, receiver_id, amount);
        if burned > 0 {
            log!("Account @{} burned {}", sender_id, burned);
        }
        used.into()
    }
}

#[near]
impl StorageManagement for MockFt {
    #[payable]
    fn storage_deposit(
        &mut self,
        account_id: Option<AccountId>,
        registration_only: Option<bool>,
    ) -> StorageBalance {
        self.token.storage_deposit(account_id, registration_only)
    }

    #[payable]
    fn storage_withdraw(&mut self, amount: Option<NearToken>) -> StorageBalance {
        self.token.storage_withdraw(amount)
    }

    #[payable]
    fn storage_unregister(&mut self, force: Option<bool>) -> bool {
        self.token.internal_storage_unregister(force).is_some()
    }

    fn storage_balance_bounds(&self) -> StorageBalanceBounds {
        self.token.storage_balance_bounds()
    }

    fn storage_balance_of(&self, account_id: AccountId) -> Option<StorageBalance> {
        self.token.storage_balance_of(account_id)
    }
}

#[near]
impl FungibleTokenMetadataProvider for MockFt {
    fn ft_metadata(&self) -> FungibleTokenMetadata {
        FungibleTokenMetadata {
            spec: FT_METADATA_SPEC.to_string(),
            name: format!("Mock {}", self.symbol),
            symbol: self.symbol.clone(),
            icon: None,
            reference: None,
            reference_hash: None,
            decimals: self.decimals,
        }
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    use super::*;

    fn acc(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    #[test]
    fn mint_to_registered_account() {
        let mut ctx = VMContextBuilder::new();
        ctx.predecessor_account_id(acc("alice.test.near"))
            .attached_deposit(NearToken::from_millinear(125));
        testing_env!(ctx.build());

        let mut ft = MockFt::new("USDC".to_string(), 6);
        ft.storage_deposit(None, None);
        ft.mint(acc("alice.test.near"), U128(25_000_000));
        assert_eq!(ft.ft_balance_of(acc("alice.test.near")), U128(25_000_000));
        assert_eq!(ft.ft_metadata().decimals, 6);
    }

    #[test]
    #[should_panic(expected = "is not registered")]
    fn mint_to_unregistered_account_panics() {
        testing_env!(VMContextBuilder::new().build());
        let mut ft = MockFt::new("USDC".to_string(), 6);
        ft.mint(acc("bob.test.near"), U128(1));
    }
}
