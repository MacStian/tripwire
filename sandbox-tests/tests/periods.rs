//! E2-S4: period rollover, grant expiry, a period grid change, freeze and the per-period rate
//! limits. The chain clock moves with sandbox fast forward.

use anyhow::Result;
use near_api::{NearGas, NearToken};
use sandbox_tests::{check, check_panic, events, returned, Harness, Options, USDC};
use serde_json::{json, Value};

const GAS: NearGas = NearGas::from_tgas(100);
const NO_DEPOSIT: NearToken = NearToken::from_yoctonear(0);
/// Long enough for the setup and a few payments to land in the first period.
const SHORT_PERIOD: u64 = 120;

fn short_period(policy: &mut Value) {
    policy["period_seconds"] = json!(SHORT_PERIOD);
}

fn expires_after_short_period(policy: &mut Value) {
    let anchor = policy["period_anchor"].as_u64().unwrap();
    policy["expires_at"] = json!(anchor + SHORT_PERIOD);
}

fn tight_rate_limits(policy: &mut Value) {
    policy["max_payments_per_period"] = json!(2);
    policy["max_escalations_per_period"] = json!(1);
}

async fn pay(h: &Harness, request_id: &str, amount: u128) -> Result<Value> {
    let outcome = h.request_payment(request_id, &h.vendor, Some(&h.usdc), amount, None).await?;
    check(&outcome, request_id)?;
    returned(&outcome)
}

async fn assert_paid(h: &Harness, request_id: &str, amount: u128) -> Result<()> {
    assert_eq!(pay(h, request_id, amount).await?, json!({ "Paid": { "request_id": request_id } }));
    Ok(())
}

async fn usdc_spent(h: &Harness) -> Result<Value> {
    Ok(h.token_status(Some(&h.usdc)).await?["spent_in_period"].clone())
}

/// The council freezes the guard as its guardian, without a vote.
async fn freeze(h: &Harness) -> Result<()> {
    let outcome = h.call_ok(&h.council, &h.guard, "freeze", json!({}), NO_DEPOSIT, GAS).await?;
    assert_eq!(events(&outcome)[0]["event"], "guard_frozen");
    Ok(())
}

/// Start of the period after the current one, on the guard's grid.
async fn next_period_start(h: &Harness) -> Result<u64> {
    let policy: Value = h.view(&h.guard, "get_policy", json!({})).await?;
    let anchor = policy["period_anchor"].as_u64().unwrap();
    let period = policy["period_seconds"].as_u64().unwrap();
    let now = h.now_sec().await?;
    Ok(anchor + period * ((now - anchor) / period + 1))
}

#[tokio::test]
async fn a_new_period_resets_the_spending() -> Result<()> {
    let h = Harness::start(Options { policy: short_period, ..Options::default() }).await?;

    for id in ["inv-1", "inv-2", "inv-3"] {
        assert_paid(&h, id, 100 * USDC).await?;
    }
    assert_eq!(usdc_spent(&h).await?, (300 * USDC).to_string());
    assert_eq!(h.guard_status().await?["request_usage"]["payments_in_period"], 3);

    let next = next_period_start(&h).await?;
    h.advance_to(next).await?;
    let status = h.token_status(Some(&h.usdc)).await?;
    assert_eq!(status["spent_in_period"], "0");
    assert_eq!(status["remaining_in_period"], (300 * USDC).to_string());
    assert_eq!(status["budget"], (700 * USDC).to_string());
    let usage = &h.guard_status().await?["request_usage"];
    assert_eq!(usage["payments_in_period"], 0);
    assert!(usage["period_start"].as_u64().unwrap() >= next);

    assert_paid(&h, "inv-4", 100 * USDC).await?;
    assert_eq!(usdc_spent(&h).await?, (100 * USDC).to_string());
    Ok(())
}

#[tokio::test]
async fn an_expired_grant_refuses_every_request() -> Result<()> {
    let h = Harness::start(Options { policy: expires_after_short_period, ..Options::default() })
        .await?;
    assert_paid(&h, "inv-1", 10 * USDC).await?;

    let expires_at = h.guard_status().await?["policy"]["expires_at"].as_u64().unwrap();
    h.advance_to(expires_at).await?;
    let next_proposal = h.next_proposal_id().await?;

    // Inside the policy or not, nothing is paid and nothing goes to a vote.
    for (id, amount) in [("inv-2", 10 * USDC), ("inv-3", 450 * USDC)] {
        let outcome = h.request_payment(id, &h.vendor, Some(&h.usdc), amount, None).await?;
        check_panic(&outcome, "ERR_GRANT_EXPIRED")?;
    }
    assert_eq!(h.next_proposal_id().await?, next_proposal);
    assert_eq!(h.usdc_balance(&h.vendor).await?, 10 * USDC);

    // The DAO still takes the budget back.
    let args =
        json!({ "token_id": h.usdc, "amount": (990 * USDC).to_string(), "receiver_id": null });
    h.dao_function_call(&h.guard, "withdraw_ft", &args, 0, GAS).await?;
    assert_eq!(h.token_status(Some(&h.usdc)).await?["budget"], "0");
    Ok(())
}

#[tokio::test]
async fn a_grid_change_keeps_the_spending() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    for (id, amount) in [("inv-1", 100), ("inv-2", 100), ("inv-3", 50)] {
        assert_paid(&h, id, amount * USDC).await?;
    }
    assert_eq!(usdc_spent(&h).await?, (250 * USDC).to_string());

    // One hour periods on an anchor half an hour back: the current period now starts at a
    // different time, and the counters move to it instead of starting from zero.
    let mut policy: Value = h.view(&h.guard, "get_policy", json!({})).await?;
    let old_start = h.guard_status().await?["request_usage"]["period_start"].as_u64().unwrap();
    policy["period_seconds"] = json!(3_600);
    policy["period_anchor"] = json!(policy["period_anchor"].as_u64().unwrap() - 1_800);
    h.dao_function_call(&h.guard, "update_policy", &json!({ "policy": policy }), 0, GAS).await?;

    let status = h.guard_status().await?;
    assert_ne!(status["request_usage"]["period_start"].as_u64().unwrap(), old_start);
    assert_eq!(status["request_usage"]["payments_in_period"], 3);
    assert_eq!(usdc_spent(&h).await?, (250 * USDC).to_string());

    // 100 more would pass 300 in the period, so it escalates, while 50 still fits.
    let value = pay(&h, "inv-4", 100 * USDC).await?;
    assert!(value["Escalated"]["proposal_id"].is_u64(), "{value}");
    assert_paid(&h, "inv-5", 50 * USDC).await?;
    assert_eq!(usdc_spent(&h).await?, (300 * USDC).to_string());
    Ok(())
}

#[tokio::test]
async fn a_frozen_guard_neither_pays_nor_escalates() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    freeze(&h).await?;
    let next_proposal = h.next_proposal_id().await?;

    let stranger = sandbox_tests::account("stranger.test.near")?;
    for (id, recipient, amount) in [
        ("inv-1", &h.vendor, 50 * USDC),
        ("inv-2", &h.vendor, 450 * USDC),
        ("inv-3", &stranger, 50 * USDC),
    ] {
        let outcome = h.request_payment(id, recipient, Some(&h.usdc), amount, None).await?;
        check_panic(&outcome, "ERR_GUARD_FROZEN")?;
    }
    assert_eq!(h.next_proposal_id().await?, next_proposal);
    assert_eq!(h.usdc_balance(&h.vendor).await?, 0);
    assert_eq!(h.token_status(Some(&h.usdc)).await?["budget"], (1_000 * USDC).to_string());

    h.dao_function_call(&h.guard, "unfreeze", &json!({}), 0, GAS).await?;
    assert_paid(&h, "inv-1", 50 * USDC).await?;
    Ok(())
}

#[tokio::test]
async fn the_rate_limits_stop_the_agent() -> Result<()> {
    let h = Harness::start(Options { policy: tight_rate_limits, ..Options::default() }).await?;

    assert_paid(&h, "inv-1", 10 * USDC).await?;
    assert_paid(&h, "inv-2", 10 * USDC).await?;
    // At the payout cap the request is refused, it does not turn into a proposal.
    let outcome = h.request_payment("inv-3", &h.vendor, Some(&h.usdc), 10 * USDC, None).await?;
    check_panic(&outcome, "ERR_PAYMENT_LIMIT_REACHED")?;

    let value = pay(&h, "esc-1", 450 * USDC).await?;
    assert!(value["Escalated"]["proposal_id"].is_u64(), "{value}");
    let next_proposal = h.next_proposal_id().await?;
    let outcome = h.request_payment("esc-2", &h.vendor, Some(&h.usdc), 450 * USDC, None).await?;
    check_panic(&outcome, "ERR_ESCALATION_LIMIT_REACHED")?;
    assert_eq!(h.next_proposal_id().await?, next_proposal);

    let usage = &h.guard_status().await?["request_usage"];
    assert_eq!(usage["payments_in_period"], 2);
    assert_eq!(usage["escalations_in_period"], 1);
    assert_eq!(h.usdc_balance(&h.vendor).await?, 20 * USDC);
    for id in ["inv-3", "esc-2"] {
        let processed: bool =
            h.view(&h.guard, "is_request_processed", json!({ "request_id": id })).await?;
        assert!(!processed, "{id}");
    }
    Ok(())
}

#[tokio::test]
async fn a_policy_proposal_filed_before_a_freeze_leaves_the_guard_frozen() -> Result<()> {
    let h = Harness::start(Options::default()).await?;

    let mut policy: Value = h.view(&h.guard, "get_policy", json!({})).await?;
    policy["max_payments_per_period"] = json!(20);
    let args = json!({ "policy": policy });
    let id = h.propose_function_call(&h.guard, "update_policy", &args, 0, GAS).await?;

    // The guardian freezes while the proposal is open, then the council approves it.
    freeze(&h).await?;
    let outcome = h.approve(id).await?;
    check(&outcome, "approving the policy")?;
    assert_eq!(h.proposal(id).await?["status"], "Approved");
    assert_eq!(events(&outcome)[0]["event"], "policy_updated");

    let status = h.guard_status().await?;
    assert_eq!(status["policy"]["max_payments_per_period"], 20);
    assert_eq!(status["frozen"], true);
    let outcome = h.request_payment("inv-1", &h.vendor, Some(&h.usdc), 10 * USDC, None).await?;
    check_panic(&outcome, "ERR_GUARD_FROZEN")?;
    Ok(())
}
