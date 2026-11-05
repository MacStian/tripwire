//! Step A preconditions and the Step B decision.

use near_sdk::Gas;

use super::*;

const HOSTING: &str = "hosting.test.near";

/// A guard with 500 USDC of budget. The policy caps USDC at 100 per payment and 300 per day.
fn funded_guard() -> Guard {
    let mut guard = guard_with(policy());
    call_as(USDC);
    guard.ft_on_transfer(acc(DAO), U128(500_000_000), String::new());
    call_as(AGENT);
    guard
}

fn spend(guard: &mut Guard, amount: u128) {
    let start = guard.current_period_start();
    guard
        .token_usage
        .insert(Some(usdc()), PeriodUsage { period_start: start, spent_in_period: U128(amount) });
}

fn decide(guard: &Guard, recipient: &str, token_id: TokenKey, amount: u128) -> Decision {
    guard.evaluate(&acc(recipient), &token_id, amount)
}

#[test]
fn request_id_charset() {
    for id in ["inv-101", "INV_2026.11", "a", &"x".repeat(MAX_REQUEST_ID_LEN)] {
        assert!(is_valid_request_id(id), "{id} should be valid");
    }
    let too_long = "x".repeat(MAX_REQUEST_ID_LEN + 1);
    for id in ["", &too_long, "a b", "a<b", "a*b", "a:b", "a&b", "a\nb", "faktura-ą", "a＜b"] {
        assert!(!is_valid_request_id(id), "{id:?} should be rejected");
    }
}

#[test]
fn allowed_request_passes_step_a() {
    let guard = funded_guard();
    guard.assert_request_allowed("inv-101", U128(1));
}

#[test]
#[should_panic(expected = "ERR_INSUFFICIENT_PREPAID_GAS")]
fn rejects_low_prepaid_gas() {
    let guard = funded_guard();
    testing_env!(context(AGENT).prepaid_gas(Gas::from_tgas(49)).build());
    guard.assert_request_allowed("inv-101", U128(1));
}

#[test]
#[should_panic(expected = "ERR_UNAUTHORIZED_AGENT")]
fn rejects_a_caller_outside_the_agent_list() {
    let guard = funded_guard();
    call_as(GUARDIAN);
    guard.assert_request_allowed("inv-101", U128(1));
}

#[test]
#[should_panic(expected = "ERR_GUARD_FROZEN")]
fn rejects_while_frozen() {
    let mut guard = funded_guard();
    call_as(GUARDIAN);
    guard.freeze();
    call_as(AGENT);
    guard.assert_request_allowed("inv-101", U128(1));
}

#[test]
fn accepts_one_second_before_expiry() {
    let guard = funded_guard();
    let expires_at = policy().expires_at.unwrap();
    call_at(AGENT, expires_at - 1);
    guard.assert_request_allowed("inv-101", U128(1));
}

#[test]
#[should_panic(expected = "ERR_GRANT_EXPIRED")]
fn rejects_at_expiry() {
    let guard = funded_guard();
    call_at(AGENT, policy().expires_at.unwrap());
    guard.assert_request_allowed("inv-101", U128(1));
}

#[test]
#[should_panic(expected = "ERR_ZERO_AMOUNT")]
fn rejects_zero_amount() {
    let guard = funded_guard();
    guard.assert_request_allowed("inv-101", U128(0));
}

#[test]
#[should_panic(expected = "ERR_DUPLICATE_OR_INVALID_REQUEST_ID")]
fn rejects_markup_in_the_request_id() {
    let guard = funded_guard();
    guard.assert_request_allowed("<b>inv</b>*101", U128(1));
}

#[test]
#[should_panic(expected = "ERR_DUPLICATE_OR_INVALID_REQUEST_ID")]
fn rejects_a_processed_request_id() {
    let mut guard = funded_guard();
    guard.processed_requests.insert("inv-101".to_string());
    guard.assert_request_allowed("inv-101", U128(1));
}

#[test]
fn pays_inside_the_policy() {
    let guard = funded_guard();
    assert_eq!(decide(&guard, HOSTING, Some(usdc()), 100_000_000), Decision::Pay);
}

#[test]
fn escalates_an_unknown_recipient() {
    let guard = funded_guard();
    assert_eq!(
        decide(&guard, "attacker.test.near", Some(usdc()), 1),
        Decision::Escalate(EscalationReason::RecipientNotAllowlisted)
    );
}

#[test]
fn escalates_a_token_outside_the_policy() {
    let guard = funded_guard();
    assert_eq!(
        decide(&guard, HOSTING, None, 1),
        Decision::Escalate(EscalationReason::TokenNotAllowed)
    );
}

#[test]
fn escalates_above_the_payment_cap() {
    let guard = funded_guard();
    assert_eq!(
        decide(&guard, HOSTING, Some(usdc()), 100_000_001),
        Decision::Escalate(EscalationReason::ExceedsPerPaymentCap)
    );
}

#[test]
fn escalates_above_the_period_cap() {
    let mut guard = funded_guard();
    spend(&mut guard, 250_000_000);
    assert_eq!(decide(&guard, HOSTING, Some(usdc()), 50_000_000), Decision::Pay);
    assert_eq!(
        decide(&guard, HOSTING, Some(usdc()), 50_000_001),
        Decision::Escalate(EscalationReason::ExceedsPeriodCap)
    );
}

#[test]
fn a_new_period_resets_the_period_cap() {
    let mut guard = funded_guard();
    spend(&mut guard, 300_000_000);
    let next_period = guard.current_period_start() + policy().period_seconds;

    call_at(AGENT, next_period - 1);
    assert_eq!(
        decide(&guard, HOSTING, Some(usdc()), 1),
        Decision::Escalate(EscalationReason::ExceedsPeriodCap)
    );
    call_at(AGENT, next_period);
    assert_eq!(decide(&guard, HOSTING, Some(usdc()), 100_000_000), Decision::Pay);
}

#[test]
fn escalates_above_the_guard_budget() {
    let mut guard = guard_with(policy());
    call_as(USDC);
    guard.ft_on_transfer(acc(DAO), U128(50_000_000), String::new());
    assert_eq!(
        decide(&guard, HOSTING, Some(usdc()), 50_000_001),
        Decision::Escalate(EscalationReason::ExceedsGuardBudget)
    );
}

#[test]
fn the_first_failing_condition_is_the_reason() {
    let guard = funded_guard();
    assert_eq!(
        decide(&guard, "attacker.test.near", Some(usdc()), 5_000_000_000),
        Decision::Escalate(EscalationReason::RecipientNotAllowlisted)
    );
}

#[test]
fn reserve_floor() {
    // 10 NEAR on the account, 0.01 NEAR locked for storage, 2 NEAR minimum reserve.
    let guard = guard_with(policy());
    let near = NearToken::from_near(1).as_yoctonear();
    guard.assert_reserve(7 * near);
    guard.assert_reserve(8 * near - NearToken::from_millinear(10).as_yoctonear());
}

#[test]
#[should_panic(expected = "ERR_INSUFFICIENT_NEAR_RESERVE")]
fn reserve_floor_rejects_one_yocto_more() {
    let guard = guard_with(policy());
    let near = NearToken::from_near(1).as_yoctonear();
    guard.assert_reserve(8 * near - NearToken::from_millinear(10).as_yoctonear() + 1);
}
