//! Escalation to the DAO: the proposal, its description and the callback.

use near_sdk::Gas;

use super::*;

const VENDOR: &str = "vendor.test.near";
const HOSTING: &str = "hosting.test.near";
const BOND: NearToken = NearToken::from_millinear(100);

/// 500 USDC of budget and a 0.1 NEAR proposal bond. The next call comes from the agent.
fn funded_guard_with(policy: Policy) -> Guard {
    let mut guard = guard_with(policy);
    call_as(USDC);
    guard.ft_on_transfer(acc(DAO), U128(500_000_000), String::new());
    call_as(AGENT);
    guard
}

fn funded_guard() -> Guard {
    funded_guard_with(Policy { proposal_bond: BOND, ..policy() })
}

fn request(
    guard: &mut Guard,
    request_id: &str,
    recipient: &str,
    token_id: TokenKey,
    amount: u128,
    memo: Option<&str>,
) {
    let _ = guard.request_payment(
        request_id.to_string(),
        acc(recipient),
        token_id,
        U128(amount),
        memo.map(str::to_string),
    );
}

/// Escalates `amount` USDC to the vendor, who is not on the allowlist.
fn escalate(guard: &mut Guard, request_id: &str, amount: u128) {
    request(guard, request_id, VENDOR, Some(usdc()), amount, None);
}

fn complete(
    guard: &mut Guard,
    request_id: &str,
    amount: u128,
    period_start: u64,
) -> PaymentOutcome {
    guard.on_escalation_complete(
        request_id.to_string(),
        acc(AGENT),
        acc(VENDOR),
        Some(usdc()),
        U128(amount),
        EscalationReason::RecipientNotAllowlisted,
        period_start,
    )
}

fn escalations(guard: &Guard) -> u32 {
    guard.get_status().request_usage.escalations_in_period
}

#[test]
fn clean_memo_keeps_only_safe_characters() {
    assert_eq!(
        clean_memo("Invoice 7, hosting/Nov #2026_a.b-c"),
        "Invoice 7, hosting/Nov #2026_a.b-c"
    );
    assert_eq!(
        clean_memo("<b>hi</b> * key: value & more\nnext`tick`"),
        "bhi/b  key value  morenexttick"
    );
    assert_eq!(clean_memo("a＜b＞c：d＊e"), "abcde");
    assert_eq!(clean_memo("faktura ą"), "faktura");
    assert_eq!(clean_memo("  note  "), "note");
    assert_eq!(clean_memo(""), "");
    assert_eq!(clean_memo(&"x".repeat(200)), "x".repeat(MAX_NOTE_LEN));
}

#[test]
fn the_description_has_one_fixed_format() {
    assert_eq!(
        proposal_description(
            "inv-101",
            EscalationReason::RecipientNotAllowlisted,
            &acc(VENDOR),
            &Some(usdc()),
            U128(5_000_000),
            Some("Invoice 7"),
        ),
        "* Notes: [Guard Escalation] req=inv-101 reason=RECIPIENT_NOT_ALLOWLISTED \
         receiver=vendor.test.near amount=5000000 token=usdc.test.near note=Invoice 7 <br>"
    );
    assert_eq!(
        proposal_description(
            "inv-102",
            EscalationReason::TokenNotAllowed,
            &acc(HOSTING),
            &None,
            U128(NEAR),
            None,
        ),
        format!(
            "* Notes: [Guard Escalation] req=inv-102 reason=TOKEN_NOT_ALLOWED \
             receiver=hosting.test.near amount={NEAR} token=NEAR note= <br>"
        )
    );
}

#[test]
fn an_unknown_recipient_becomes_a_transfer_proposal() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    request(&mut guard, "inv-101", VENDOR, Some(usdc()), 5_000_000, Some("Invoice 7"));

    let receipts = get_created_receipts();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].receiver_id, acc(DAO));
    let (method, args, deposit, gas) = function_call(&receipts[0]);
    assert_eq!(method, "add_proposal");
    assert_eq!(
        args,
        serde_json::json!({
            "proposal": {
                "description": proposal_description(
                    "inv-101",
                    EscalationReason::RecipientNotAllowlisted,
                    &acc(VENDOR),
                    &Some(usdc()),
                    U128(5_000_000),
                    Some("Invoice 7"),
                ),
                "kind": {
                    "Transfer": {
                        "token_id": USDC,
                        "receiver_id": VENDOR,
                        "amount": "5000000",
                        "msg": null,
                    }
                }
            }
        })
    );
    assert_eq!(deposit, BOND);
    assert_eq!(gas, Gas::from_tgas(25));

    assert_eq!(receipts[1].receiver_id, acc(GUARD));
    assert_eq!(receipts[1].receipt_indices, vec![0]);
    let (method, args, deposit, gas) = function_call(&receipts[1]);
    assert_eq!(method, "on_escalation_complete");
    assert_eq!(
        args,
        serde_json::json!({
            "request_id": "inv-101",
            "agent": AGENT,
            "recipient": VENDOR,
            "token_id": USDC,
            "amount": "5000000",
            "reason": "RECIPIENT_NOT_ALLOWLISTED",
            "period_start": start,
        })
    );
    assert_eq!(deposit, NearToken::from_yoctonear(0));
    assert_eq!(gas, Gas::from_tgas(10));

    // The DAO pays from its treasury after the vote, so no budget or spend moves.
    let usdc_status = guard.get_token_status(Some(usdc())).unwrap();
    assert_eq!(usdc_status.budget, U128(500_000_000));
    assert_eq!(usdc_status.spent_in_period, U128(0));
    assert_eq!(escalations(&guard), 1);
    assert_eq!(guard.get_status().request_usage.payments_in_period, 0);
    assert!(guard.is_request_processed("inv-101".to_string()));
}

#[test]
fn native_near_outside_the_policy_uses_an_empty_token_id() {
    let mut guard = funded_guard();
    request(&mut guard, "inv-101", HOSTING, None, NEAR, None);

    let (_, args, _, _) = function_call(&get_created_receipts()[0]);
    assert_eq!(
        args["proposal"]["kind"],
        serde_json::json!({
            "Transfer": { "token_id": "", "receiver_id": HOSTING, "amount": NEAR.to_string(), "msg": null }
        })
    );
    let description = args["proposal"]["description"].as_str().unwrap();
    assert!(description.contains("reason=TOKEN_NOT_ALLOWED"), "{description}");
    assert!(description.contains("token=NEAR"), "{description}");
}

#[test]
fn a_large_invoice_to_a_known_recipient_goes_to_a_vote() {
    let mut guard = funded_guard();
    request(&mut guard, "inv-101", HOSTING, Some(usdc()), 100_000_001, None);

    let (method, args, _, _) = function_call(&get_created_receipts()[0]);
    assert_eq!(method, "add_proposal");
    let description = args["proposal"]["description"].as_str().unwrap();
    assert!(description.contains("reason=EXCEEDS_PER_PAYMENT_CAP"), "{description}");
    assert_eq!(guard.get_token_status(Some(usdc())).unwrap().budget, U128(500_000_000));
}

#[test]
fn the_memo_reaches_the_proposal_only_as_a_clean_note() {
    let mut guard = funded_guard();
    let memo = "<b>Invoice</b> #7: hosting & more";
    request(&mut guard, "inv-101", VENDOR, Some(usdc()), 5_000_000, Some(memo));

    let (_, args, _, _) = function_call(&get_created_receipts()[0]);
    let description = args["proposal"]["description"].as_str().unwrap();
    assert!(description.ends_with(" note=bInvoice/b #7 hosting  more <br>"), "{description}");
    assert_eq!(description.matches('*').count(), 1, "{description}");
    assert_eq!(description.matches(':').count(), 1, "{description}");
    assert!(!description.contains('&'), "{description}");
    assert_eq!(description.matches('<').count(), 1, "{description}");
}

#[test]
fn success_reports_the_proposal_id() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    escalate(&mut guard, "inv-101", 5_000_000);

    callback_with(PromiseResult::Successful(b"7".to_vec()));
    let outcome = complete(&mut guard, "inv-101", 5_000_000, start);
    assert_eq!(
        outcome,
        PaymentOutcome::Escalated { request_id: "inv-101".to_string(), proposal_id: Some(7) }
    );
    assert_eq!(escalations(&guard), 1);
    assert!(guard.is_request_processed("inv-101".to_string()));
    let event = only_event();
    assert_eq!(event["event"], "payment_escalated");
    assert_eq!(
        event["data"],
        serde_json::json!({
            "request_id": "inv-101",
            "agent": AGENT,
            "recipient": VENDOR,
            "token_id": USDC,
            "amount": "5000000",
            "reason": "RECIPIENT_NOT_ALLOWLISTED",
            "proposal_id": 7,
        })
    );
}

#[test]
fn an_answer_that_is_not_an_id_reports_null() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    escalate(&mut guard, "inv-101", 5_000_000);

    let expected =
        PaymentOutcome::Escalated { request_id: "inv-101".to_string(), proposal_id: None };
    for answer in [b"\"seven\"".to_vec(), b"-1".to_vec(), vec![b'1'; 33]] {
        callback_with(PromiseResult::Successful(answer));
        assert_eq!(complete(&mut guard, "inv-101", 5_000_000, start), expected);
        assert_eq!(only_event()["data"]["proposal_id"], serde_json::Value::Null);
    }
    assert!(guard.is_request_processed("inv-101".to_string()));
}

#[test]
fn failure_restores_the_escalation_count_and_the_request_id() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    escalate(&mut guard, "inv-101", 5_000_000);

    callback_with(PromiseResult::Failed);
    let outcome = complete(&mut guard, "inv-101", 5_000_000, start);
    assert_eq!(outcome, PaymentOutcome::EscalationFailed { request_id: "inv-101".to_string() });
    assert_eq!(escalations(&guard), 0);
    assert!(!guard.is_request_processed("inv-101".to_string()));
    let event = only_event();
    assert_eq!(event["event"], "escalation_failed");
    assert_eq!(
        event["data"],
        serde_json::json!({
            "request_id": "inv-101",
            "recipient": VENDOR,
            "token_id": USDC,
            "amount": "5000000",
            "reason": "RECIPIENT_NOT_ALLOWLISTED",
        })
    );

    // A wrong bond fails closed. Once the DAO fixes it, the agent retries the same id.
    call_as(AGENT);
    escalate(&mut guard, "inv-101", 5_000_000);
    assert_eq!(escalations(&guard), 1);
}

#[test]
fn failure_after_the_period_ended_leaves_the_new_period_alone() {
    let mut guard = funded_guard();
    let old_start = guard.current_period_start();
    escalate(&mut guard, "inv-101", 5_000_000);

    let new_start = old_start + policy().period_seconds;
    call_at(AGENT, new_start);
    escalate(&mut guard, "inv-102", 5_000_000);

    callback_at(PromiseResult::Failed, new_start);
    complete(&mut guard, "inv-101", 5_000_000, old_start);
    assert_eq!(escalations(&guard), 1);
    assert!(!guard.is_request_processed("inv-101".to_string()));
    assert!(guard.is_request_processed("inv-102".to_string()));
}

#[test]
#[should_panic(expected = "ERR_NOT_A_CALLBACK")]
fn a_direct_call_without_a_promise_result_panics() {
    let mut guard = funded_guard();
    let start = guard.current_period_start();
    escalate(&mut guard, "inv-101", 5_000_000);
    call_as(GUARD);
    complete(&mut guard, "inv-101", 5_000_000, start);
}

#[test]
#[should_panic(expected = "ERR_ESCALATION_LIMIT_REACHED")]
fn stops_at_the_escalation_limit() {
    let mut guard = funded_guard_with(Policy {
        max_escalations_per_period: 1,
        proposal_bond: BOND,
        ..policy()
    });
    escalate(&mut guard, "inv-1", 1);
    escalate(&mut guard, "inv-2", 1);
}

#[test]
fn payouts_and_escalations_have_separate_limits() {
    let mut guard = funded_guard_with(Policy {
        max_payments_per_period: 1,
        max_escalations_per_period: 1,
        proposal_bond: BOND,
        ..policy()
    });
    request(&mut guard, "inv-1", HOSTING, Some(usdc()), 1, None);
    escalate(&mut guard, "inv-2", 1);
    let usage = guard.get_status().request_usage;
    assert_eq!((usage.payments_in_period, usage.escalations_in_period), (1, 1));
}

#[test]
#[should_panic(expected = "ERR_INSUFFICIENT_NEAR_RESERVE")]
fn the_bond_never_comes_from_the_near_budget() {
    // 3 NEAR of budget, a 2 NEAR reserve and a 0.1 NEAR bond need 5.1 NEAR. The account
    // holds 5.05 NEAR, so the bond would have to come from the budget.
    let mut guard = funded_guard();
    call_with_deposit(DAO, NearToken::from_near(3));
    guard.fund();
    call_with_balance(AGENT, NearToken::from_millinear(5_050));
    escalate(&mut guard, "inv-101", 5_000_000);
}
