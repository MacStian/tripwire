use near_sdk::PromiseOrValue;

use super::*;

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

/// The callback's plain value, or `None` when it returned a promise.
fn returned(outcome: PromiseOrValue<bool>) -> Option<bool> {
    match outcome {
        PromiseOrValue::Value(value) => Some(value),
        PromiseOrValue::Promise(_) => None,
    }
}

/// A failed withdrawal ends its chain in `fail_withdrawal` on the guard.
fn assert_chain_fails(outcome: PromiseOrValue<bool>) {
    assert_eq!(returned(outcome), None);
    let receipts = get_created_receipts();
    let [receipt] = receipts.as_slice() else {
        panic!("expected one receipt, got {receipts:?}");
    };
    assert_eq!(receipt.receiver_id, acc(GUARD));
    let [MockAction::FunctionCallWeight { method_name, attached_deposit, prepaid_gas, .. }] =
        receipt.actions.as_slice()
    else {
        panic!("expected one function call, got {:?}", receipt.actions);
    };
    assert_eq!(method_name, b"fail_withdrawal");
    assert_eq!(*attached_deposit, NearToken::from_yoctonear(0));
    assert_eq!(*prepaid_gas, GAS_FAIL_WITHDRAWAL);
}

/// JSON args of the callback a withdrawal chains after its transfer.
fn withdraw_callback(receipt: &Receipt) -> serde_json::Value {
    assert_eq!(receipt.receiver_id, acc(GUARD));
    let (method, args, deposit, gas) = function_call(receipt);
    assert_eq!(method, "on_withdraw_complete");
    assert_eq!(deposit, NearToken::from_yoctonear(0));
    assert_eq!(gas, GAS_ON_WITHDRAW);
    args
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
fn withdraw_near_works_while_the_reserve_is_short() {
    // 4 NEAR on the account cannot cover 2 NEAR reserve plus 3 NEAR budget. The DAO can
    // still take budget NEAR out, and the shortfall stays the same.
    let mut guard = funded_guard();
    call_with_balance(DAO, NearToken::from_near(4));
    let _ = guard.withdraw_near(U128(NEAR), None);
    assert_eq!(guard.get_status().near_budget, U128(2 * NEAR));
}

#[test]
#[should_panic(expected = "ERR_INSUFFICIENT_NEAR_RESERVE")]
fn withdraw_near_needs_the_near_it_sends() {
    let mut guard = funded_guard();
    call_with_balance(DAO, NearToken::from_millinear(500));
    let _ = guard.withdraw_near(U128(NEAR), None);
}

#[test]
fn withdraw_near_sends_a_transfer_and_chains_the_callback() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_near(U128(NEAR), None);

    let receipts = get_created_receipts();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].receiver_id, acc(DAO));
    let [MockAction::Transfer { deposit, .. }] = receipts[0].actions.as_slice() else {
        panic!("expected one transfer, got {:?}", receipts[0].actions);
    };
    assert_eq!(*deposit, NearToken::from_yoctonear(NEAR));
    assert_eq!(
        withdraw_callback(&receipts[1]),
        serde_json::json!({
            "withdrawal": { "Budget": null },
            "amount": NEAR.to_string(),
            "receiver_id": DAO,
        })
    );
}

#[test]
fn withdraw_near_debits_and_the_callback_reports_success() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_near(U128(NEAR), None);
    assert_eq!(guard.get_status().near_budget, U128(2 * NEAR));

    callback_with(PromiseResult::Successful(vec![]));
    let outcome = guard.on_withdraw_complete(Withdrawal::Budget(None), U128(NEAR), acc(DAO));
    assert_eq!(returned(outcome), Some(true));
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
fn failed_near_withdrawal_restores_the_budget_and_fails_the_chain() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_near(U128(NEAR), Some(acc("missing.test.near")));

    callback_with(PromiseResult::Failed);
    let missing = acc("missing.test.near");
    assert_chain_fails(guard.on_withdraw_complete(Withdrawal::Budget(None), U128(NEAR), missing));
    assert_eq!(guard.get_status().near_budget, U128(3 * NEAR));
    let event = only_event();
    assert_eq!(event["event"], "withdrawal_failed");
    assert_eq!(
        event["data"],
        serde_json::json!({
            "receiver_id": "missing.test.near",
            "token_id": null,
            "amount": NEAR.to_string(),
            "unallocated": false,
        })
    );
}

#[test]
#[should_panic(expected = "ERR_WITHDRAWAL_FAILED")]
fn fail_withdrawal_always_panics() {
    let guard = funded_guard();
    call_as(GUARD);
    guard.fail_withdrawal();
}

#[test]
fn callback_without_a_promise_result_changes_nothing() {
    let mut guard = funded_guard();
    call_as(GUARD);
    let outcome = guard.on_withdraw_complete(Withdrawal::Budget(None), U128(NEAR), acc(DAO));
    assert_eq!(returned(outcome), Some(false));
    assert_eq!(guard.get_status().near_budget, U128(3 * NEAR));
    assert!(events().is_empty());
    assert!(get_created_receipts().is_empty());
}

#[test]
fn withdraw_ft_sends_ft_transfer_and_chains_the_callback() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_ft(usdc(), U128(200_000_000), None);

    let receipts = get_created_receipts();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].receiver_id, usdc());
    let (method, args, deposit, gas) = function_call(&receipts[0]);
    assert_eq!(method, "ft_transfer");
    assert_eq!(
        args,
        serde_json::json!({ "receiver_id": DAO, "amount": "200000000", "memo": null })
    );
    assert_eq!(deposit, NearToken::from_yoctonear(1));
    assert_eq!(gas, GAS_FT_TRANSFER);
    assert_eq!(
        withdraw_callback(&receipts[1]),
        serde_json::json!({
            "withdrawal": { "Budget": USDC },
            "amount": "200000000",
            "receiver_id": DAO,
        })
    );
}

#[test]
fn withdraw_ft_works_while_the_reserve_is_short() {
    // 3 NEAR on the account is under the 2 NEAR reserve plus 3 NEAR budget.
    let mut guard = funded_guard();
    call_with_balance(DAO, NearToken::from_near(3));
    let _ = guard.withdraw_ft(usdc(), U128(200_000_000), None);
    assert_eq!(usdc_budget(&guard), U128(300_000_000));
}

#[test]
fn withdraw_ft_debits_and_a_failed_transfer_restores_the_budget() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_ft(usdc(), U128(200_000_000), None);
    assert_eq!(usdc_budget(&guard), U128(300_000_000));

    callback_with(PromiseResult::Failed);
    let budget = Withdrawal::Budget(Some(usdc()));
    assert_chain_fails(guard.on_withdraw_complete(budget, U128(200_000_000), acc(DAO)));
    assert_eq!(usdc_budget(&guard), U128(500_000_000));
    let event = only_event();
    assert_eq!(event["event"], "withdrawal_failed");
    assert_eq!(event["data"]["token_id"], USDC);
    assert_eq!(event["data"]["unallocated"], false);
}

#[test]
fn successful_ft_withdrawal_emits_the_remaining_budget() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_ft(usdc(), U128(200_000_000), None);

    callback_with(PromiseResult::Successful(vec![]));
    let budget = Withdrawal::Budget(Some(usdc()));
    let outcome = guard.on_withdraw_complete(budget, U128(200_000_000), acc(DAO));
    assert_eq!(returned(outcome), Some(true));
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
    let receipts = get_created_receipts();
    assert_eq!(receipts.len(), 2);
    assert_eq!(
        withdraw_callback(&receipts[1]),
        serde_json::json!({
            "withdrawal": "Unallocated",
            "amount": (4 * NEAR).to_string(),
            "receiver_id": DAO,
        })
    );

    callback_with(PromiseResult::Successful(vec![]));
    let outcome = guard.on_withdraw_complete(Withdrawal::Unallocated, U128(4 * NEAR), acc(DAO));
    assert_eq!(returned(outcome), Some(true));
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
fn failed_unallocated_withdrawal_changes_no_budget_and_fails_the_chain() {
    let mut guard = funded_guard();
    call_as(DAO);
    let _ = guard.withdraw_unallocated_near(U128(NEAR), None);

    callback_with(PromiseResult::Failed);
    assert_chain_fails(guard.on_withdraw_complete(Withdrawal::Unallocated, U128(NEAR), acc(DAO)));
    assert_eq!(guard.get_status().near_budget, U128(3 * NEAR));
    let event = only_event();
    assert_eq!(event["event"], "withdrawal_failed");
    assert_eq!(event["data"]["token_id"], serde_json::Value::Null);
    assert_eq!(event["data"]["unallocated"], true);
}
