//! The in-policy payout and its callback.

use near_sdk::Gas;

use super::*;

const HOSTING: &str = "hosting.test.near";

/// The default policy plus native NEAR: 1 NEAR per payment and 2 NEAR per day.
fn near_policy() -> Policy {
    let mut p = policy();
    p.token_limits.push(TokenLimit {
        token_id: None,
        max_per_payment: U128(NEAR),
        max_per_period: U128(2 * NEAR),
    });
    p
}

/// 500 USDC and 3 NEAR of budget. The next call comes from the agent.
fn funded_guard_with(policy: Policy) -> Guard {
    let mut guard = guard_with(policy);
    call_as(USDC);
    guard.ft_on_transfer(acc(DAO), U128(500_000_000), String::new());
    call_with_deposit(DAO, NearToken::from_near(3));
    guard.fund();
    call_as(AGENT);
    guard
}

fn funded_guard() -> Guard {
    funded_guard_with(near_policy())
}

fn request(guard: &mut Guard, request_id: &str, token_id: TokenKey, amount: u128) {
    let _ =
        guard.request_payment(request_id.to_string(), acc(HOSTING), token_id, U128(amount), None);
}

fn complete(
    guard: &mut Guard,
    request_id: &str,
    token_id: TokenKey,
    amount: u128,
    spent_in_period: u128,
    period_start: u64,
) -> PaymentOutcome {
    guard.on_payout_complete(
        request_id.to_string(),
        acc(AGENT),
        acc(HOSTING),
        token_id,
        U128(amount),
        U128(spent_in_period),
        period_start,
    )
}

fn usdc_status(guard: &Guard) -> TokenStatus {
    guard.get_token_status(Some(usdc())).unwrap()
}

fn payments(guard: &Guard) -> u32 {
    guard.get_status().request_usage.payments_in_period
}

#[test]
fn pays_usdc_with_a_plain_ft_transfer_and_a_callback() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    request(&mut guard, "inv-101", Some(usdc()), 100_000_000);

    let status = usdc_status(&guard);
    assert_eq!(status.budget, U128(400_000_000));
    assert_eq!(status.spent_in_period, U128(100_000_000));
    assert_eq!(payments(&guard), 1);
    assert!(guard.is_request_processed("inv-101".to_string()));

    let receipts = get_created_receipts();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].receiver_id, usdc());
    let (method, args, deposit, gas) = function_call(&receipts[0]);
    assert_eq!(method, "ft_transfer");
    assert_eq!(
        args,
        serde_json::json!({ "receiver_id": HOSTING, "amount": "100000000", "memo": null })
    );
    assert_eq!(deposit, NearToken::from_yoctonear(1));
    assert_eq!(gas, Gas::from_tgas(15));

    assert_eq!(receipts[1].receiver_id, acc(GUARD));
    assert_eq!(receipts[1].receipt_indices, vec![0]);
    let (method, args, deposit, gas) = function_call(&receipts[1]);
    assert_eq!(method, "on_payout_complete");
    assert_eq!(
        args,
        serde_json::json!({
            "request_id": "inv-101",
            "agent": AGENT,
            "recipient": HOSTING,
            "token_id": USDC,
            "amount": "100000000",
            "spent_in_period": "100000000",
            "period_start": start,
        })
    );
    assert_eq!(deposit, NearToken::from_yoctonear(0));
    assert_eq!(gas, Gas::from_tgas(10));
}

#[test]
fn pays_near_with_a_transfer() {
    let mut guard = funded_guard();
    request(&mut guard, "inv-101", None, NEAR);

    assert_eq!(guard.get_status().near_budget, U128(2 * NEAR));
    let receipts = get_created_receipts();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].receiver_id, acc(HOSTING));
    let [MockAction::Transfer { deposit, .. }] = receipts[0].actions.as_slice() else {
        panic!("expected one transfer, got {:?}", receipts[0].actions);
    };
    assert_eq!(*deposit, NearToken::from_near(1));
    assert_eq!(function_call(&receipts[1]).0, "on_payout_complete");
}

#[test]
fn success_emits_payment_executed() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    request(&mut guard, "inv-101", Some(usdc()), 100_000_000);

    callback_with(PromiseResult::Successful(vec![]));
    let outcome = complete(&mut guard, "inv-101", Some(usdc()), 100_000_000, 100_000_000, start);
    assert_eq!(outcome, PaymentOutcome::Paid { request_id: "inv-101".to_string() });

    let status = usdc_status(&guard);
    assert_eq!(status.budget, U128(400_000_000));
    assert_eq!(status.spent_in_period, U128(100_000_000));
    assert!(guard.is_request_processed("inv-101".to_string()));
    let event = only_event();
    assert_eq!(event["event"], "payment_executed");
    assert_eq!(
        event["data"],
        serde_json::json!({
            "request_id": "inv-101",
            "agent": AGENT,
            "recipient": HOSTING,
            "token_id": USDC,
            "amount": "100000000",
            "spent_in_period": "100000000",
        })
    );
}

#[test]
fn failure_restores_the_budget_the_counters_and_the_request_id() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    request(&mut guard, "inv-101", Some(usdc()), 100_000_000);

    callback_with(PromiseResult::Failed);
    let outcome = complete(&mut guard, "inv-101", Some(usdc()), 100_000_000, 100_000_000, start);
    assert_eq!(outcome, PaymentOutcome::PaymentFailed { request_id: "inv-101".to_string() });

    let status = usdc_status(&guard);
    assert_eq!(status.budget, U128(500_000_000));
    assert_eq!(status.spent_in_period, U128(0));
    assert_eq!(payments(&guard), 0);
    assert!(!guard.is_request_processed("inv-101".to_string()));
    let event = only_event();
    assert_eq!(event["event"], "payment_failed");
    assert_eq!(
        event["data"],
        serde_json::json!({
            "request_id": "inv-101",
            "recipient": HOSTING,
            "token_id": USDC,
            "amount": "100000000",
        })
    );

    // Once the cause is fixed, the agent retries with the same id.
    call_as(AGENT);
    request(&mut guard, "inv-101", Some(usdc()), 100_000_000);
    assert_eq!(usdc_status(&guard).budget, U128(400_000_000));
}

#[test]
fn failed_near_payout_restores_the_near_budget() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    request(&mut guard, "inv-101", None, NEAR);

    callback_with(PromiseResult::Failed);
    complete(&mut guard, "inv-101", None, NEAR, NEAR, start);
    assert_eq!(guard.get_status().near_budget, U128(3 * NEAR));
    assert_eq!(guard.get_token_status(None).unwrap().spent_in_period, U128(0));
}

#[test]
fn failure_after_the_period_ended_leaves_the_new_period_alone() {
    let mut guard = funded_guard();
    let old_start = guard.current_period_start();
    request(&mut guard, "inv-101", Some(usdc()), 100_000_000);

    let new_start = old_start + policy().period_seconds;
    call_at(AGENT, new_start);
    request(&mut guard, "inv-102", Some(usdc()), 50_000_000);

    callback_at(PromiseResult::Failed, new_start);
    complete(&mut guard, "inv-101", Some(usdc()), 100_000_000, 100_000_000, old_start);
    let status = usdc_status(&guard);
    assert_eq!(status.budget, U128(450_000_000));
    assert_eq!(status.spent_in_period, U128(50_000_000));
    assert_eq!(payments(&guard), 1);
    assert!(!guard.is_request_processed("inv-101".to_string()));
}

#[test]
#[should_panic(expected = "ERR_NOT_A_CALLBACK")]
fn a_direct_call_without_a_promise_result_panics() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    request(&mut guard, "inv-101", Some(usdc()), 100_000_000);
    call_as(GUARD);
    complete(&mut guard, "inv-101", Some(usdc()), 100_000_000, 100_000_000, start);
}

#[test]
fn payment_executed_reports_the_period_of_the_payment() {
    // Paid just before the period ends, confirmed after it: the event still shows the total
    // of the period the payment counts in.
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    request(&mut guard, "inv-101", Some(usdc()), 100_000_000);

    callback_at(PromiseResult::Successful(vec![]), start + policy().period_seconds);
    complete(&mut guard, "inv-101", Some(usdc()), 100_000_000, 100_000_000, start);
    assert_eq!(usdc_status(&guard).spent_in_period, U128(0));
    assert_eq!(only_event()["data"]["spent_in_period"], "100000000");
}

#[test]
fn two_payouts_in_flight_one_fails_and_one_succeeds() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    request(&mut guard, "inv-1", Some(usdc()), 100_000_000);
    call_as(AGENT);
    request(&mut guard, "inv-2", Some(usdc()), 50_000_000);
    assert_eq!(usdc_status(&guard).budget, U128(350_000_000));
    assert_eq!(payments(&guard), 2);

    callback_with(PromiseResult::Failed);
    complete(&mut guard, "inv-1", Some(usdc()), 100_000_000, 100_000_000, start);
    callback_with(PromiseResult::Successful(vec![]));
    let outcome = complete(&mut guard, "inv-2", Some(usdc()), 50_000_000, 150_000_000, start);
    assert_eq!(outcome, PaymentOutcome::Paid { request_id: "inv-2".to_string() });

    // Only the failed payout is rolled back.
    let status = usdc_status(&guard);
    assert_eq!(status.budget, U128(450_000_000));
    assert_eq!(status.spent_in_period, U128(50_000_000));
    assert_eq!(payments(&guard), 1);
    assert!(!guard.is_request_processed("inv-1".to_string()));
    assert!(guard.is_request_processed("inv-2".to_string()));
    let event = only_event();
    assert_eq!(event["event"], "payment_executed");
    assert_eq!(event["data"]["request_id"], "inv-2");
}

#[test]
#[should_panic(expected = "ERR_DUPLICATE_OR_INVALID_REQUEST_ID")]
fn a_paid_request_id_cannot_be_used_again() {
    let mut guard = funded_guard();
    request(&mut guard, "inv-101", Some(usdc()), 1);
    request(&mut guard, "inv-101", Some(usdc()), 1);
}

#[test]
#[should_panic(expected = "ERR_PAYMENT_LIMIT_REACHED")]
fn stops_at_the_payment_limit() {
    let mut guard = funded_guard_with(Policy { max_payments_per_period: 2, ..near_policy() });
    request(&mut guard, "inv-1", Some(usdc()), 1);
    request(&mut guard, "inv-2", Some(usdc()), 1);
    request(&mut guard, "inv-3", Some(usdc()), 1);
}

#[test]
fn a_new_period_resets_the_payment_limit() {
    let mut guard = funded_guard_with(Policy { max_payments_per_period: 1, ..near_policy() });
    request(&mut guard, "inv-1", Some(usdc()), 1);
    call_at(AGENT, guard.current_period_start() + policy().period_seconds);
    request(&mut guard, "inv-2", Some(usdc()), 1);
    assert_eq!(payments(&guard), 1);
}

#[test]
fn payouts_in_flight_count_against_the_period_cap() {
    let mut guard = funded_guard();
    for id in ["inv-1", "inv-2", "inv-3"] {
        request(&mut guard, id, Some(usdc()), 100_000_000);
    }
    assert_eq!(usdc_status(&guard).remaining_in_period, U128(0));
    assert_eq!(
        guard.evaluate(&acc(HOSTING), &Some(usdc()), 1),
        Decision::Escalate(EscalationReason::ExceedsPeriodCap)
    );
}

#[test]
#[should_panic(expected = "ERR_INSUFFICIENT_NEAR_RESERVE")]
fn a_near_payout_never_takes_the_reserve() {
    // 3 NEAR of budget but only 4.9 NEAR on the account: after a 1 NEAR payout, 2 NEAR of
    // budget and the 2 NEAR reserve would need 5 NEAR.
    let mut guard = funded_guard();
    call_with_balance(AGENT, NearToken::from_millinear(4_900));
    request(&mut guard, "inv-101", None, NEAR);
}
