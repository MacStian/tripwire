//! E2-S2: requests outside the policy become Transfer proposals that the council votes on.

use anyhow::Result;
use sandbox_tests::{check, event_names, events, returned, Harness, Options, USDC};
use serde_json::{json, Value};

/// The proposal id from an `Escalated` outcome.
fn proposal_id(outcome: &Value) -> u64 {
    outcome["Escalated"]["proposal_id"].as_u64().expect("an escalation with a proposal id")
}

#[tokio::test]
async fn over_the_per_payment_cap_the_council_decides() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let dao_usdc = h.usdc_balance(&h.dao).await?;

    let outcome = h.request_payment("inv-450", &h.vendor, Some(&h.usdc), 450 * USDC, None).await?;
    check(&outcome, "request_payment")?;
    let value = returned(&outcome)?;
    let id = proposal_id(&value);
    assert_eq!(value["Escalated"]["request_id"], "inv-450");
    assert_eq!(event_names(&outcome), ["payment_escalated"]);
    assert_eq!(
        events(&outcome)[0]["data"],
        json!({
            "request_id": "inv-450",
            "agent": h.agent,
            "recipient": h.vendor,
            "token_id": h.usdc,
            "amount": (450 * USDC).to_string(),
            "reason": "EXCEEDS_PER_PAYMENT_CAP",
            "proposal_id": id,
        })
    );

    // Nothing is paid from the guard, the DAO holds a Transfer proposal filed by the guard.
    assert_eq!(h.usdc_balance(&h.vendor).await?, 0);
    assert_eq!(h.token_status(Some(&h.usdc)).await?["budget"], (1_000 * USDC).to_string());
    let proposal = h.proposal(id).await?;
    assert_eq!(proposal["proposer"], h.guard.to_string());
    assert_eq!(proposal["status"], "InProgress");
    assert_eq!(
        proposal["kind"],
        json!({ "Transfer": {
            "token_id": h.usdc,
            "receiver_id": h.vendor,
            "amount": (450 * USDC).to_string(),
            "msg": null,
        } })
    );
    assert_eq!(
        proposal["description"],
        "* Notes: [Guard Escalation] req=inv-450 reason=EXCEEDS_PER_PAYMENT_CAP \
         receiver=vendor.test.near amount=450000000 token=usdc.test.near note= <br>"
    );

    // The council approves, and the DAO pays from its own treasury.
    check(&h.approve(id).await?, "approving the escalation")?;
    assert_eq!(h.proposal(id).await?["status"], "Approved");
    assert_eq!(h.usdc_balance(&h.vendor).await?, 450 * USDC);
    assert_eq!(h.usdc_balance(&h.dao).await?, dao_usdc - 450 * USDC);
    assert_eq!(h.usdc_balance(&h.guard).await?, 1_000 * USDC);
    Ok(())
}

#[tokio::test]
async fn over_the_period_cap_the_next_payment_escalates() -> Result<()> {
    let h = Harness::start(Options::default()).await?;

    // 300 USDC per period: three payments of 100 fit.
    for id in ["inv-1", "inv-2", "inv-3"] {
        let outcome = h.request_payment(id, &h.vendor, Some(&h.usdc), 100 * USDC, None).await?;
        check(&outcome, id)?;
        assert_eq!(returned(&outcome)?, json!({ "Paid": { "request_id": id } }));
    }
    let status = h.token_status(Some(&h.usdc)).await?;
    assert_eq!(status["spent_in_period"], (300 * USDC).to_string());
    assert_eq!(status["remaining_in_period"], "0");

    let outcome = h.request_payment("inv-4", &h.vendor, Some(&h.usdc), 50 * USDC, None).await?;
    check(&outcome, "inv-4")?;
    let id = proposal_id(&returned(&outcome)?);
    assert_eq!(events(&outcome)[0]["data"]["reason"], "EXCEEDS_PERIOD_CAP");
    assert_eq!(h.proposal(id).await?["kind"]["Transfer"]["amount"], (50 * USDC).to_string());
    assert_eq!(h.usdc_balance(&h.vendor).await?, 300 * USDC);

    let status = h.guard_status().await?;
    assert_eq!(status["request_usage"]["payments_in_period"], 3);
    assert_eq!(status["request_usage"]["escalations_in_period"], 1);
    Ok(())
}

#[tokio::test]
async fn an_unknown_recipient_escalates_with_a_clean_note() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let attacker = sandbox_tests::account("attacker.test.near")?;

    // Markup and a line break in the memo, as an invoice could carry them.
    let memo = "Invoice #42\n<b>urgent</b> wire today";
    let outcome =
        h.request_payment("inv-77", &attacker, Some(&h.usdc), 50 * USDC, Some(memo)).await?;
    check(&outcome, "request_payment")?;
    let id = proposal_id(&returned(&outcome)?);
    assert_eq!(events(&outcome)[0]["data"]["reason"], "RECIPIENT_NOT_ALLOWLISTED");

    let proposal = h.proposal(id).await?;
    let description = proposal["description"].as_str().unwrap();
    assert_eq!(
        description,
        "* Notes: [Guard Escalation] req=inv-77 reason=RECIPIENT_NOT_ALLOWLISTED \
         receiver=attacker.test.near amount=50000000 token=usdc.test.near \
         note=Invoice #42burgent/b wire today <br>"
    );
    // The vote is on the real receiver, whatever the note says.
    assert_eq!(proposal["kind"]["Transfer"]["receiver_id"], attacker.to_string());
    assert_eq!(h.token_status(Some(&h.usdc)).await?["budget"], (1_000 * USDC).to_string());
    Ok(())
}
