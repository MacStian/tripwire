use super::*;

const SPENT: u128 = 250_000_000;

/// A guard with USDC usage and request counters in the current period.
fn guard_with_usage() -> Guard {
    let mut guard = guard_with(policy());
    let start = guard.current_period_start();
    guard
        .token_usage
        .insert(Some(usdc()), PeriodUsage { period_start: start, spent_in_period: U128(SPENT) });
    guard.request_usage =
        RequestUsage { period_start: start, payments_in_period: 3, escalations_in_period: 2 };
    guard
}

fn request_counts(guard: &Guard) -> (u32, u32) {
    let usage = guard.get_status().request_usage;
    (usage.payments_in_period, usage.escalations_in_period)
}

fn usdc_spent(guard: &Guard) -> U128 {
    guard.get_token_status(Some(usdc())).unwrap().spent_in_period
}

#[test]
#[should_panic(expected = "ERR_NOT_DAO")]
fn update_policy_rejects_a_non_dao_caller() {
    let mut guard = guard_with(policy());
    call_as(AGENT);
    guard.update_policy(policy());
}

#[test]
fn update_policy_replaces_the_policy_and_emits_an_event() {
    let mut guard = guard_with(policy());
    call_as(DAO);
    let new_policy = Policy { max_payments_per_period: 7, ..policy() };
    guard.update_policy(new_policy.clone());

    assert_eq!(guard.get_policy(), new_policy);
    let event = only_event();
    assert_eq!(event["event"], "policy_updated");
    assert_eq!(event["data"]["by"], DAO);
}

#[test]
#[should_panic(expected = "ERR_INVALID_POLICY: agents")]
fn update_policy_validates_the_new_policy() {
    let mut guard = guard_with(policy());
    call_as(DAO);
    guard.update_policy(Policy { agents: vec![], ..policy() });
}

#[test]
fn update_policy_never_unfreezes() {
    let mut guard = guard_with(policy());
    call_as(GUARDIAN);
    guard.freeze();
    call_as(DAO);
    guard.update_policy(policy());
    assert!(guard.get_status().frozen);
}

#[test]
fn update_policy_keeps_usage_on_the_same_grid() {
    let mut guard = guard_with_usage();
    call_as(DAO);
    let mut new_policy = policy();
    new_policy.token_limits[0].max_per_period = U128(400_000_000);
    guard.update_policy(new_policy);

    let usdc_status = guard.get_token_status(Some(usdc())).unwrap();
    assert_eq!(usdc_status.spent_in_period, U128(SPENT));
    assert_eq!(usdc_status.remaining_in_period, U128(150_000_000));
    assert_eq!(request_counts(&guard), (3, 2));
}

#[test]
fn grid_change_moves_usage_to_the_new_grid() {
    let mut guard = guard_with_usage();
    call_as(DAO);
    // Old grid: anchor NOW - 1 h, 1 day periods. New grid: same anchor, 10 minute periods.
    guard.update_policy(Policy { period_seconds: 600, ..policy() });

    assert_eq!(usdc_spent(&guard), U128(SPENT));
    assert_eq!(request_counts(&guard), (3, 2));
    assert_eq!(guard.get_status().request_usage.period_start, NOW_SEC);
}

#[test]
fn grid_change_by_anchor_keeps_usage() {
    let mut guard = guard_with_usage();
    call_as(DAO);
    guard.update_policy(Policy { period_anchor: NOW_SEC - 60, ..policy() });

    assert_eq!(usdc_spent(&guard), U128(SPENT));
    assert_eq!(request_counts(&guard), (3, 2));
}

#[test]
fn moved_usage_resets_at_the_next_boundary_of_the_new_grid() {
    let mut guard = guard_with_usage();
    call_as(DAO);
    guard.update_policy(Policy { period_seconds: 600, ..policy() });

    call_at(AGENT, NOW_SEC + 599);
    assert_eq!(usdc_spent(&guard), U128(SPENT));
    call_at(AGENT, NOW_SEC + 600);
    assert_eq!(usdc_spent(&guard), U128(0));
    assert_eq!(request_counts(&guard), (0, 0));
}

#[test]
fn grid_change_drops_usage_from_a_finished_period() {
    let mut guard = guard_with(policy());
    let old_start = guard.current_period_start() - 86_400;
    guard.token_usage.insert(
        Some(usdc()),
        PeriodUsage { period_start: old_start, spent_in_period: U128(SPENT) },
    );
    call_as(DAO);
    guard.update_policy(Policy { period_seconds: 600, ..policy() });

    assert_eq!(usdc_spent(&guard), U128(0));
}

#[test]
fn dao_and_guardian_can_freeze() {
    let mut guard = guard_with(policy());
    call_as(DAO);
    guard.freeze();
    assert!(guard.get_status().frozen);
    let event = only_event();
    assert_eq!(event["event"], "guard_frozen");
    assert_eq!(event["data"]["by"], DAO);

    let mut guard = guard_with(policy());
    call_as(GUARDIAN);
    guard.freeze();
    assert!(guard.get_status().frozen);
    assert_eq!(only_event()["data"]["by"], GUARDIAN);
}

#[test]
#[should_panic(expected = "ERR_NOT_DAO_OR_GUARDIAN")]
fn agent_cannot_freeze() {
    let mut guard = guard_with(policy());
    call_as(AGENT);
    guard.freeze();
}

#[test]
#[should_panic(expected = "ERR_NOT_DAO_OR_GUARDIAN")]
fn nobody_else_can_freeze_without_a_guardian() {
    let mut guard = guard_with(Policy { guardian: None, ..policy() });
    call_as(GUARDIAN);
    guard.freeze();
}

#[test]
#[should_panic(expected = "ERR_NOT_DAO")]
fn guardian_cannot_unfreeze() {
    let mut guard = guard_with(policy());
    call_as(GUARDIAN);
    guard.freeze();
    guard.unfreeze();
}

#[test]
fn dao_can_unfreeze() {
    let mut guard = guard_with(policy());
    call_as(GUARDIAN);
    guard.freeze();
    call_as(DAO);
    guard.unfreeze();

    assert!(!guard.get_status().frozen);
    let event = only_event();
    assert_eq!(event["event"], "guard_unfrozen");
    assert_eq!(event["data"]["by"], DAO);
}
