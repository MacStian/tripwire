//! E2-S4: failed transfers roll back, tokens outside the policy or without `msg` are never
//! credited, and NEAR outside the budget goes back to the DAO.

use anyhow::Result;
use near_api::{NearGas, NearToken};
use sandbox_tests::{
    account, check, check_panic, event_names, events, returned, Harness, Options, NEAR, USDC,
};
use serde_json::{json, Value};

const GAS: NearGas = NearGas::from_tgas(100);

/// Allowlisted, but without storage on the token, so `ft_transfer` to it fails.
const UNREGISTERED: &str = "unregistered.test.near";
/// Allowlisted, but the account does not exist, so a NEAR transfer to it fails.
const MISSING: &str = "missing.test.near";

fn with_failing_recipients(policy: &mut Value) {
    let recipients = policy["recipients"].as_array_mut().unwrap();
    recipients.extend([json!(UNREGISTERED), json!(MISSING)]);
}

/// `get_status` without `free_reserve`, which moves with the gas rewards every call pays.
async fn state(h: &Harness) -> Result<Value> {
    let mut status = h.guard_status().await?;
    status.as_object_mut().unwrap().remove("free_reserve");
    Ok(status)
}

#[tokio::test]
async fn a_failed_payout_is_rolled_back_and_can_be_retried() -> Result<()> {
    let h =
        Harness::start(Options { policy: with_failing_recipients, ..Options::default() }).await?;
    let unregistered = account(UNREGISTERED)?;

    // The token refuses the transfer, the callback restores everything the request debited.
    let outcome = h.request_payment("inv-1", &unregistered, Some(&h.usdc), 60 * USDC, None).await?;
    assert_eq!(returned(&outcome)?, json!({ "PaymentFailed": { "request_id": "inv-1" } }));
    assert_eq!(event_names(&outcome), ["payment_failed"]);
    assert_eq!(
        events(&outcome)[0]["data"],
        json!({
            "request_id": "inv-1",
            "recipient": unregistered,
            "token_id": h.usdc,
            "amount": (60 * USDC).to_string(),
        })
    );
    let status = h.token_status(Some(&h.usdc)).await?;
    assert_eq!(status["budget"], (1_000 * USDC).to_string());
    assert_eq!(status["spent_in_period"], "0");
    assert_eq!(h.guard_status().await?["request_usage"]["payments_in_period"], 0);
    assert_eq!(h.usdc_balance(&h.guard).await?, 1_000 * USDC);
    let processed: bool =
        h.view(&h.guard, "is_request_processed", json!({ "request_id": "inv-1" })).await?;
    assert!(!processed);

    // The request id is free again, so the same invoice goes through once the recipient
    // has storage on the token.
    h.register(&h.usdc, &unregistered).await?;
    let outcome = h.request_payment("inv-1", &unregistered, Some(&h.usdc), 60 * USDC, None).await?;
    check(&outcome, "the retry")?;
    assert_eq!(returned(&outcome)?, json!({ "Paid": { "request_id": "inv-1" } }));
    assert_eq!(h.usdc_balance(&unregistered).await?, 60 * USDC);

    // A NEAR payout to an account that does not exist rolls back the same way.
    let missing = account(MISSING)?;
    let outcome = h.request_payment("inv-2", &missing, None, NEAR / 2, None).await?;
    assert_eq!(returned(&outcome)?, json!({ "PaymentFailed": { "request_id": "inv-2" } }));
    assert_eq!(h.guard_status().await?["near_budget"], (5 * NEAR).to_string());
    assert_eq!(h.token_status(None).await?["spent_in_period"], "0");
    Ok(())
}

#[tokio::test]
async fn a_failed_withdrawal_fails_the_proposal() -> Result<()> {
    let h = Harness::start(Options::default()).await?;

    let args = json!({ "token_id": h.usdc, "amount": (200 * USDC).to_string(), "receiver_id": UNREGISTERED });
    let (id, outcome) = h.dao_function_call_outcome(&h.guard, "withdraw_ft", &args, 0, GAS).await?;
    assert_eq!(h.proposal(id).await?["status"], "Failed");
    assert_eq!(event_names(&outcome), ["withdrawal_failed"]);
    assert_eq!(
        events(&outcome)[0]["data"],
        json!({
            "receiver_id": UNREGISTERED,
            "token_id": h.usdc,
            "amount": (200 * USDC).to_string(),
            "unallocated": false,
        })
    );
    assert_eq!(h.token_status(Some(&h.usdc)).await?["budget"], (1_000 * USDC).to_string());
    assert_eq!(h.usdc_balance(&h.guard).await?, 1_000 * USDC);
    Ok(())
}

#[tokio::test]
async fn a_returned_bond_goes_back_to_the_dao_as_unallocated_near() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let bond = h.proposal_bond.as_yoctonear();

    // The guard pays the bond when it escalates, and the DAO returns it on approval with a
    // plain transfer, which lands outside `near_budget`.
    let outcome = h.request_payment("inv-450", &h.vendor, Some(&h.usdc), 450 * USDC, None).await?;
    check(&outcome, "request_payment")?;
    let id = returned(&outcome)?["Escalated"]["proposal_id"].as_u64().unwrap();
    let before = h.near_balance(&h.guard).await?.as_yoctonear();
    check(&h.approve(id).await?, "approving the escalation")?;
    let returned_bond = h.near_balance(&h.guard).await?.as_yoctonear();
    assert_eq!(returned_bond - before, bond);
    assert_eq!(h.guard_status().await?["near_budget"], (5 * NEAR).to_string());

    let args = json!({ "amount": bond.to_string(), "receiver_id": null });
    let (_, outcome) =
        h.dao_function_call_outcome(&h.guard, "withdraw_unallocated_near", &args, 0, GAS).await?;
    check(&outcome, "withdraw_unallocated_near")?;
    assert_eq!(event_names(&outcome), ["reserve_withdrawn"]);
    assert_eq!(
        events(&outcome)[0]["data"],
        json!({ "receiver_id": h.dao, "amount": bond.to_string() })
    );

    // The bond left the guard, less the gas rewards of the two receipts, and the budget did
    // not move.
    let sent = returned_bond - h.near_balance(&h.guard).await?.as_yoctonear();
    assert!(sent <= bond && sent > bond - NEAR / 1_000, "sent {sent}");
    assert_eq!(h.guard_status().await?["near_budget"], (5 * NEAR).to_string());
    Ok(())
}

#[tokio::test]
async fn tokens_outside_the_policy_are_refused() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let fake = h.create_token("fake.test.near", 1_000 * USDC).await?;
    let before = state(&h).await?;
    let storage = h.storage_usage(&h.guard).await?;

    let args = json!({ "receiver_id": h.guard, "amount": (500 * USDC).to_string(), "msg": "" });
    let one_yocto = NearToken::from_yoctonear(1);
    let outcome = h.call_ok(&h.council, &fake, "ft_transfer_call", args, one_yocto, GAS).await?;

    // ft_on_transfer returned the full amount, so the token refunded the sender.
    assert_eq!(returned(&outcome)?, "0");
    assert!(event_names(&outcome).is_empty());
    assert_eq!(h.ft_balance(&fake, &h.council).await?, 1_000 * USDC);
    assert_eq!(h.ft_balance(&fake, &h.guard).await?, 0);
    assert!(h.token_status(Some(&fake)).await?.is_null());
    assert_eq!(state(&h).await?, before);
    assert_eq!(h.storage_usage(&h.guard).await?, storage);
    Ok(())
}

#[tokio::test]
async fn a_transfer_without_msg_is_not_credited() -> Result<()> {
    let h = Harness::start(Options::default()).await?;

    // A Sputnik Transfer without `msg` sends a plain ft_transfer, which runs no guard code.
    let kind = json!({ "Transfer": {
        "token_id": h.usdc,
        "receiver_id": h.guard,
        "amount": (100 * USDC).to_string(),
        "msg": null,
    } });
    let id = h.propose("Send 100 USDC to the guard without msg", kind).await?;
    check(&h.approve(id).await?, "approving the transfer")?;
    assert_eq!(h.proposal(id).await?["status"], "Approved");
    assert_eq!(h.usdc_balance(&h.guard).await?, 1_100 * USDC);
    assert_eq!(h.token_status(Some(&h.usdc)).await?["budget"], (1_000 * USDC).to_string());

    // Withdrawals are capped by the budget, so the extra 100 USDC stay in the guard.
    let args =
        json!({ "token_id": h.usdc, "amount": (1_100 * USDC).to_string(), "receiver_id": null });
    let (id, outcome) = h.dao_function_call_outcome(&h.guard, "withdraw_ft", &args, 0, GAS).await?;
    check_panic(&outcome, "ERR_INSUFFICIENT_BUDGET")?;
    assert_eq!(h.proposal(id).await?["status"], "Failed");
    assert_eq!(h.usdc_balance(&h.guard).await?, 1_100 * USDC);
    Ok(())
}
