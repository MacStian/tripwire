use near_sdk::test_utils::VMContextBuilder;
use near_sdk::{testing_env, NearToken};

use super::*;

const NOW_SEC: u64 = 1_762_300_000;

fn acc(id: &str) -> AccountId {
    id.parse().unwrap()
}

fn usdc() -> AccountId {
    acc("usdc.test.near")
}

fn setup() {
    let mut ctx = VMContextBuilder::new();
    ctx.current_account_id(acc("guard.test.near"))
        .predecessor_account_id(acc("guard.test.near"))
        .block_timestamp(NOW_SEC * 1_000_000_000)
        .account_balance(NearToken::from_near(10));
    testing_env!(ctx.build());
}

fn policy() -> Policy {
    Policy {
        agents: vec![acc("agent.test.near")],
        recipients: vec![acc("hosting.test.near")],
        token_limits: vec![TokenLimit {
            token_id: Some(usdc()),
            max_per_payment: U128(100_000_000),
            max_per_period: U128(300_000_000),
        }],
        period_seconds: 86_400,
        period_anchor: NOW_SEC - 3_600,
        expires_at: Some(NOW_SEC + 30 * 86_400),
        guardian: Some(acc("guardian.test.near")),
        max_payments_per_period: 20,
        max_escalations_per_period: 5,
        proposal_bond: NearToken::from_yoctonear(0),
        min_near_reserve: NearToken::from_near(2),
    }
}

fn guard_with(policy: Policy) -> Guard {
    setup();
    Guard::new(acc("dao.sputnik-dao.test.near"), policy)
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
    guard_with(Policy { recipients: vec![acc("guard.test.near")], ..policy() });
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: guard account")]
fn rejects_guard_as_agent() {
    guard_with(Policy { agents: vec![acc("guard.test.near")], ..policy() });
}
