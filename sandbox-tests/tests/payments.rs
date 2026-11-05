//! E2-S2: in-policy payouts, signed by the agent with its function call key only.

use anyhow::Result;
use sandbox_tests::{check, event_names, events, returned, Harness, Options, NEAR, USDC};
use serde_json::json;

#[tokio::test]
async fn pays_usdc_inside_the_policy() -> Result<()> {
    let h = Harness::start(Options::default()).await?;

    let outcome = h.request_payment("inv-1001", &h.vendor, Some(&h.usdc), 80 * USDC, None).await?;
    check(&outcome, "request_payment")?;
    assert_eq!(returned(&outcome)?, json!({ "Paid": { "request_id": "inv-1001" } }));
    assert_eq!(event_names(&outcome), ["payment_executed"]);
    assert_eq!(
        events(&outcome)[0]["data"],
        json!({
            "request_id": "inv-1001",
            "agent": h.agent,
            "recipient": h.vendor,
            "token_id": h.usdc,
            "amount": (80 * USDC).to_string(),
            "spent_in_period": (80 * USDC).to_string(),
        })
    );

    assert_eq!(h.usdc_balance(&h.vendor).await?, 80 * USDC);
    assert_eq!(h.usdc_balance(&h.guard).await?, 920 * USDC);
    let status = h.token_status(Some(&h.usdc)).await?;
    assert_eq!(status["budget"], (920 * USDC).to_string());
    assert_eq!(status["spent_in_period"], (80 * USDC).to_string());
    assert_eq!(status["remaining_in_period"], (220 * USDC).to_string());
    assert_eq!(h.guard_status().await?["request_usage"]["payments_in_period"], 1);
    let processed: bool =
        h.view(&h.guard, "is_request_processed", json!({ "request_id": "inv-1001" })).await?;
    assert!(processed);

    // The same invoice again is rejected before anything moves.
    let again = h.request_payment("inv-1001", &h.vendor, Some(&h.usdc), 80 * USDC, None).await?;
    assert!(format!("{:?}", again.failures()).contains("ERR_DUPLICATE_OR_INVALID_REQUEST_ID"));
    assert_eq!(h.usdc_balance(&h.vendor).await?, 80 * USDC);
    Ok(())
}

#[tokio::test]
async fn pays_near_inside_the_policy() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let before = h.near_balance(&h.vendor).await?;

    let outcome = h.request_payment("inv-2001", &h.vendor, None, NEAR, None).await?;
    check(&outcome, "request_payment")?;
    assert_eq!(returned(&outcome)?, json!({ "Paid": { "request_id": "inv-2001" } }));
    assert_eq!(event_names(&outcome), ["payment_executed"]);
    assert_eq!(events(&outcome)[0]["data"]["token_id"], json!(null));

    // The vendor pays no gas here, so its balance grows by exactly the payment.
    let after = h.near_balance(&h.vendor).await?;
    assert_eq!(after.as_yoctonear() - before.as_yoctonear(), NEAR);
    let status = h.guard_status().await?;
    assert_eq!(status["near_budget"], (4 * NEAR).to_string());
    assert_eq!(h.token_status(None).await?["spent_in_period"], NEAR.to_string());
    Ok(())
}
