use near_sdk::mock::{MockAction, Receipt};
use near_sdk::test_utils::{get_created_receipts, get_logs, VMContextBuilder};
use near_sdk::{serde_json, testing_env, NearToken, PromiseResult};

use super::*;

mod escalation;
mod funding;
mod governance;
mod payout;
mod request;

const NEAR: u128 = 1_000_000_000_000_000_000_000_000;
const NOW_SEC: u64 = 1_762_300_000;
const GUARD: &str = "guard.test.near";
const DAO: &str = "dao.sputnik-dao.test.near";
const AGENT: &str = "agent.test.near";
const GUARDIAN: &str = "guardian.test.near";
const USDC: &str = "usdc.test.near";

fn acc(id: &str) -> AccountId {
    id.parse().unwrap()
}

fn usdc() -> AccountId {
    acc(USDC)
}

/// 10 NEAR on the account and 1 000 bytes of storage (0.01 NEAR). The near-sdk default is
/// 300 KiB, which would lock about 3 NEAR and blur the reserve checks.
fn context(predecessor: &str) -> VMContextBuilder {
    let mut ctx = VMContextBuilder::new();
    ctx.current_account_id(acc(GUARD))
        .predecessor_account_id(acc(predecessor))
        .block_timestamp(NOW_SEC * 1_000_000_000)
        .account_balance(NearToken::from_near(10))
        .storage_usage(1_000);
    ctx
}

fn setup() {
    testing_env!(context(GUARD).build());
}

/// The next call comes from `predecessor`. Contract storage carries over, logs start empty.
fn call_as(predecessor: &str) {
    testing_env!(context(predecessor).build());
}

fn call_at(predecessor: &str, now_sec: u64) {
    testing_env!(context(predecessor).block_timestamp(now_sec * 1_000_000_000).build());
}

fn call_with_deposit(predecessor: &str, deposit: NearToken) {
    testing_env!(context(predecessor).attached_deposit(deposit).build());
}

fn call_with_balance(predecessor: &str, balance: NearToken) {
    testing_env!(context(predecessor).account_balance(balance).build());
}

/// The next call is a callback of one promise that ended with `result`.
fn callback_with(result: PromiseResult) {
    callback_at(result, NOW_SEC);
}

fn callback_at(result: PromiseResult, now_sec: u64) {
    testing_env!(
        context(GUARD).block_timestamp(now_sec * 1_000_000_000).build(),
        near_sdk::test_vm_config(),
        near_sdk::RuntimeFeesConfig::test(),
        Default::default(),
        vec![result],
    );
}

/// NEP-297 events logged by the last call.
fn events() -> Vec<serde_json::Value> {
    get_logs()
        .iter()
        .filter_map(|line| line.strip_prefix("EVENT_JSON:"))
        .map(|json| serde_json::from_str(json).unwrap())
        .collect()
}

/// Method, JSON args, deposit and static gas of a receipt with one function call.
fn function_call(receipt: &Receipt) -> (String, serde_json::Value, NearToken, Gas) {
    let [MockAction::FunctionCallWeight { method_name, args, attached_deposit, prepaid_gas, .. }] =
        receipt.actions.as_slice()
    else {
        panic!("expected one function call, got {:?}", receipt.actions);
    };
    (
        String::from_utf8(method_name.clone()).unwrap(),
        serde_json::from_slice(args).unwrap(),
        *attached_deposit,
        *prepaid_gas,
    )
}

fn only_event() -> serde_json::Value {
    let mut events = events();
    assert_eq!(events.len(), 1, "expected one event, got {events:?}");
    let event = events.remove(0);
    assert_eq!(event["standard"], "tripwire");
    assert_eq!(event["version"], "1.0.0");
    event
}

fn policy() -> Policy {
    Policy {
        agents: vec![acc(AGENT)],
        recipients: vec![acc("hosting.test.near")],
        token_limits: vec![TokenLimit {
            token_id: Some(usdc()),
            max_per_payment: U128(100_000_000),
            max_per_period: U128(300_000_000),
        }],
        period_seconds: 86_400,
        period_anchor: NOW_SEC - 3_600,
        expires_at: Some(NOW_SEC + 30 * 86_400),
        guardian: Some(acc(GUARDIAN)),
        max_payments_per_period: 20,
        max_escalations_per_period: 5,
        proposal_bond: NearToken::from_yoctonear(0),
        min_near_reserve: NearToken::from_near(2),
    }
}

fn guard_with(policy: Policy) -> Guard {
    setup();
    Guard::new(acc(DAO), policy)
}

#[test]
fn init_reports_policy_and_empty_budgets() {
    let guard = guard_with(policy());
    let status = guard.get_status();
    assert_eq!(status.policy, policy());
    assert!(!status.frozen);
    assert_eq!(status.near_budget, U128(0));
    assert_eq!(status.request_usage.period_start, NOW_SEC - 3_600);

    let usdc_status = guard.get_token_status(Some(usdc())).unwrap();
    assert_eq!(usdc_status.budget, U128(0));
    assert_eq!(usdc_status.spent_in_period, U128(0));
    assert_eq!(usdc_status.remaining_in_period, U128(300_000_000));
}

#[test]
fn token_outside_policy_has_no_status() {
    let guard = guard_with(policy());
    assert!(guard.get_token_status(None).is_none());
}

#[test]
fn unknown_request_is_not_processed() {
    let guard = guard_with(policy());
    assert!(!guard.is_request_processed("inv-101".to_string()));
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: agents")]
fn rejects_no_agents() {
    guard_with(Policy { agents: vec![], ..policy() });
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: agents")]
fn rejects_too_many_agents() {
    let agents = (0..=MAX_AGENTS).map(|i| acc(&format!("agent{i}.test.near"))).collect();
    guard_with(Policy { agents, ..policy() });
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: recipients")]
fn rejects_too_many_recipients() {
    let recipients = (0..=MAX_RECIPIENTS).map(|i| acc(&format!("vendor{i}.test.near"))).collect();
    guard_with(Policy { recipients, ..policy() });
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: token_limits")]
fn rejects_no_tokens() {
    guard_with(Policy { token_limits: vec![], ..policy() });
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: duplicates")]
fn rejects_duplicate_agents() {
    guard_with(Policy { agents: vec![acc("agent.test.near"), acc("agent.test.near")], ..policy() });
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: duplicates")]
fn rejects_duplicate_tokens() {
    let mut p = policy();
    p.token_limits.push(p.token_limits[0].clone());
    guard_with(p);
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: token caps")]
fn rejects_payment_cap_above_period_cap() {
    let mut p = policy();
    p.token_limits[0].max_per_payment = U128(400_000_000);
    guard_with(p);
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: token caps")]
fn rejects_zero_payment_cap() {
    let mut p = policy();
    p.token_limits[0].max_per_payment = U128(0);
    guard_with(p);
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: period_seconds")]
fn rejects_zero_period() {
    guard_with(Policy { period_seconds: 0, ..policy() });
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: period_anchor")]
fn rejects_anchor_in_the_future() {
    guard_with(Policy { period_anchor: NOW_SEC + 1, ..policy() });
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: expires_at")]
fn rejects_expiry_in_the_past() {
    guard_with(Policy { expires_at: Some(NOW_SEC), ..policy() });
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: guard account")]
fn rejects_guard_as_recipient() {
    guard_with(Policy { recipients: vec![acc(GUARD)], ..policy() });
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: guard account")]
fn rejects_guard_as_agent() {
    guard_with(Policy { agents: vec![acc(GUARD)], ..policy() });
}
