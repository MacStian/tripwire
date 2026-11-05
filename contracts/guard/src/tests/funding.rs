use super::*;

const NEAR: u128 = 1_000_000_000_000_000_000_000_000;

/// A guard with 3 NEAR in `near_budget` and 500 USDC in its token budget.
/// The mocked account balance stays at 10 NEAR, and `min_near_reserve` is 2 NEAR.
fn funded_guard() -> Guard {
    let mut guard = guard_with(policy());
    call_with_deposit(DAO, NearToken::from_near(3));
    guard.fund();
    call_as(USDC);
    guard.ft_on_transfer(acc(DAO), U128(500_000_000), String::new());
    guard
}

fn usdc_budget(guard: &Guard) -> U128 {
    guard.get_token_status(Some(usdc())).unwrap().budget
}

#[test]
fn fund_adds_the_deposit_to_the_near_budget() {
    let mut guard = guard_with(policy());
    call_with_deposit(DAO, NearToken::from_near(3));
    assert_eq!(guard.fund(), U128(3 * NEAR));
    assert_eq!(guard.get_status().near_budget, U128(3 * NEAR));

    let event = only_event();
    assert_eq!(event["event"], "budget_funded");
    assert_eq!(
        event["data"],
        serde_json::json!({
            "funder": DAO,
            "token_id": null,
            "amount": (3 * NEAR).to_string(),
            "new_budget": (3 * NEAR).to_string(),
        })
    );
}

#[test]
#[should_panic(expected = "ERR_ZERO_AMOUNT")]
fn fund_rejects_no_deposit() {
    let mut guard = guard_with(policy());
    call_as(DAO);
    guard.fund();
}

#[test]
fn ft_on_transfer_credits_a_token_in_the_policy() {
    let mut guard = guard_with(policy());
    call_as(USDC);
    let refund = guard.ft_on_transfer(acc(DAO), U128(500_000_000), "fund".to_string());

    assert_eq!(refund, U128(0));
    assert_eq!(usdc_budget(&guard), U128(500_000_000));
    let event = only_event();
    assert_eq!(event["event"], "budget_funded");
    assert_eq!(event["data"]["funder"], DAO);
    assert_eq!(event["data"]["token_id"], USDC);
    assert_eq!(event["data"]["new_budget"], "500000000");
}

#[test]
fn ft_on_transfer_refunds_an_unknown_token_and_writes_nothing() {
    let mut guard = guard_with(policy());
    call_as("fake-token.test.near");
    let storage_before = env::storage_usage();
    let refund = guard.ft_on_transfer(acc("attacker.test.near"), U128(1_000), String::new());
    guard.ft_budgets.flush();

    assert_eq!(refund, U128(1_000));
    assert_eq!(env::storage_usage(), storage_before);
    assert!(guard.ft_budgets.get(&acc("fake-token.test.near")).is_none());
    assert!(events().is_empty());
}

#[test]
#[should_panic(expected = "ERR_INSUFFICIENT_NEAR_RESERVE")]
fn ft_on_transfer_panics_when_the_reserve_cannot_pay_for_storage() {
    // A panic here makes the token contract refund the sender.
    let mut guard = guard_with(policy());
    call_with_balance(USDC, NearToken::from_near(2));
    guard.ft_on_transfer(acc(DAO), U128(500_000_000), String::new());
}

#[test]
#[should_panic(expected = "ERR_NOT_DAO")]
fn withdraw_near_rejects_a_non_dao_caller() {
    let mut guard = funded_guard();
    call_as(GUARDIAN);
    let _ = guard.withdraw_near(U128(NEAR), None);
}

#[test]
#[should_panic(expected = "ERR_INSUFFICIENT_BUDGET")]
fn withdraw_near_rejects_more_than_the_budget() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_near(U128(3 * NEAR + 1), None);
}

#[test]
#[should_panic(expected = "ERR_ZERO_AMOUNT")]
fn withdraw_near_rejects_zero() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_near(U128(0), None);
}

#[test]
#[should_panic(expected = "ERR_INSUFFICIENT_NEAR_RESERVE")]
fn withdraw_near_keeps_the_reserve_invariant() {
    // 4 NEAR on the account cannot cover 2 NEAR reserve plus 3 NEAR budget.
    let mut guard = funded_guard();
    call_with_balance(DAO, NearToken::from_near(4));
    let _ = guard.withdraw_near(U128(NEAR), None);
}

#[test]
fn withdraw_near_debits_and_the_callback_reports_success() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_near(U128(NEAR), None);
    assert_eq!(guard.get_status().near_budget, U128(2 * NEAR));

    callback_with(PromiseResult::Successful(vec![]));
    assert!(guard.on_withdraw_complete(Withdrawal::Budget(None), U128(NEAR), acc(DAO)));
    assert_eq!(guard.get_status().near_budget, U128(2 * NEAR));
    let event = only_event();
    assert_eq!(event["event"], "budget_withdrawn");
    assert_eq!(
        event["data"],
        serde_json::json!({
            "receiver_id": DAO,
            "token_id": null,
            "amount": NEAR.to_string(),
            "remaining_budget": (2 * NEAR).to_string(),
        })
    );
}

#[test]
fn failed_near_withdrawal_restores_the_budget() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_near(U128(NEAR), Some(acc("missing.test.near")));

    callback_with(PromiseResult::Failed);
    assert!(!guard.on_withdraw_complete(
        Withdrawal::Budget(None),
        U128(NEAR),
        acc("missing.test.near")
    ));
    assert_eq!(guard.get_status().near_budget, U128(3 * NEAR));
    assert!(events().is_empty());
}

#[test]
fn callback_without_a_promise_result_changes_nothing() {
    let mut guard = funded_guard();
    call_as(GUARD);
    assert!(!guard.on_withdraw_complete(Withdrawal::Budget(None), U128(NEAR), acc(DAO)));
    assert_eq!(guard.get_status().near_budget, U128(3 * NEAR));
    assert!(events().is_empty());
}

#[test]
fn withdraw_ft_debits_and_a_failed_transfer_restores_the_budget() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_ft(usdc(), U128(200_000_000), None);
    assert_eq!(usdc_budget(&guard), U128(300_000_000));

    callback_with(PromiseResult::Failed);
    assert!(!guard.on_withdraw_complete(
        Withdrawal::Budget(Some(usdc())),
        U128(200_000_000),
        acc(DAO)
    ));
    assert_eq!(usdc_budget(&guard), U128(500_000_000));
}

#[test]
fn successful_ft_withdrawal_emits_the_remaining_budget() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_ft(usdc(), U128(200_000_000), None);

    callback_with(PromiseResult::Successful(vec![]));
    assert!(guard.on_withdraw_complete(
        Withdrawal::Budget(Some(usdc())),
        U128(200_000_000),
        acc(DAO)
    ));
    let event = only_event();
    assert_eq!(event["event"], "budget_withdrawn");
    assert_eq!(event["data"]["token_id"], USDC);
    assert_eq!(event["data"]["remaining_budget"], "300000000");
}

#[test]
#[should_panic(expected = "ERR_INSUFFICIENT_BUDGET")]
fn withdraw_ft_rejects_a_token_without_budget() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_ft(acc("other-token.test.near"), U128(1), None);
}

#[test]
#[should_panic(expected = "ERR_NOT_DAO")]
fn withdraw_ft_rejects_a_non_dao_caller() {
    let mut guard = funded_guard();
    call_as(AGENT);
    let _ = guard.withdraw_ft(usdc(), U128(1), None);
}

#[test]
fn withdraw_unallocated_near_leaves_the_budget_alone() {
    // 10 NEAR on the account, 2 NEAR reserve, 3 NEAR budget: just under 5 NEAR is unallocated.
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_unallocated_near(U128(4 * NEAR), None);
    assert_eq!(guard.get_status().near_budget, U128(3 * NEAR));

    callback_with(PromiseResult::Successful(vec![]));
    assert!(guard.on_withdraw_complete(Withdrawal::Unallocated, U128(4 * NEAR), acc(DAO)));
    let event = only_event();
    assert_eq!(event["event"], "reserve_withdrawn");
    assert_eq!(
        event["data"],
        serde_json::json!({ "receiver_id": DAO, "amount": (4 * NEAR).to_string() })
    );
}

#[test]
#[should_panic(expected = "ERR_INSUFFICIENT_NEAR_RESERVE")]
fn withdraw_unallocated_near_cannot_touch_the_budget_or_reserve() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_unallocated_near(U128(5 * NEAR), None);
}

#[test]
#[should_panic(expected = "ERR_NOT_DAO")]
fn withdraw_unallocated_near_rejects_a_non_dao_caller() {
    let mut guard = funded_guard();
    call_as(GUARDIAN);
    let _ = guard.withdraw_unallocated_near(U128(NEAR), None);
}

#[test]
fn failed_unallocated_withdrawal_changes_no_budget() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_unallocated_near(U128(NEAR), None);

    callback_with(PromiseResult::Failed);
    assert!(!guard.on_withdraw_complete(Withdrawal::Unallocated, U128(NEAR), acc(DAO)));
    assert_eq!(guard.get_status().near_budget, U128(3 * NEAR));
    assert!(events().is_empty());
}
