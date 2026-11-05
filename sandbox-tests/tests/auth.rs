//! E2-S3: the agent key reaches `request_payment` and nothing else, only listed agents may ask
//! for payments, and only an approved DAO proposal can change the policy, unfreeze or withdraw.
//! The guardian may freeze on its own.

use anyhow::Result;
use near_api::{NearGas, NearToken};
use sandbox_tests::{
    check, check_panic, event_names, events, returned, Harness, Options, AGENT_GAS, NEAR, USDC,
};
use serde_json::{json, Value};

const GAS: NearGas = NearGas::from_tgas(100);
const NO_DEPOSIT: NearToken = NearToken::from_yoctonear(0);

/// `get_status` without `free_reserve`, which moves with the gas rewards every call pays.
async fn state(h: &Harness) -> Result<Value> {
    let mut status = h.guard_status().await?;
    status.as_object_mut().unwrap().remove("free_reserve");
    Ok(status)
}

fn payment_args(h: &Harness, request_id: &str, amount: u128) -> Value {
    json!({
        "request_id": request_id,
        "recipient": h.vendor,
        "token_id": h.usdc,
        "amount": amount.to_string(),
        "memo": null,
    })
}

#[tokio::test]
async fn the_agent_key_only_reaches_request_payment() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let before = state(&h).await?;
    let policy = &before["policy"];

    let calls = [
        ("update_policy", json!({ "policy": policy })),
        ("freeze", json!({})),
        ("unfreeze", json!({})),
        ("withdraw_near", json!({ "amount": NEAR.to_string(), "receiver_id": h.agent })),
        (
            "withdraw_ft",
            json!({ "token_id": h.usdc, "amount": (100 * USDC).to_string(), "receiver_id": h.agent }),
        ),
        (
            "withdraw_unallocated_near",
            json!({ "amount": NEAR.to_string(), "receiver_id": h.agent }),
        ),
    ];
    // The runtime refuses each one before any code runs.
    for (method, args) in calls {
        assert_eq!(
            h.agent_refusal(&h.guard, method, args, NO_DEPOSIT, GAS).await?,
            json!({ "InvalidAccessKeyError": { "MethodNameMismatch": { "method_name": method } } }),
        );
    }

    // Nor any other contract, here the token the guard holds.
    let args = json!({ "receiver_id": h.agent, "amount": "1", "memo": null });
    assert_eq!(
        h.agent_refusal(&h.usdc, "ft_transfer", args, NO_DEPOSIT, GAS).await?,
        json!({ "InvalidAccessKeyError": { "ReceiverMismatch": {
            "ak_receiver": h.guard,
            "tx_receiver": h.usdc,
        } } }),
    );

    assert_eq!(state(&h).await?, before);
    assert_eq!(h.usdc_balance(&h.agent).await?, 0);
    Ok(())
}

#[tokio::test]
async fn the_agent_key_cannot_attach_a_deposit() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let before = state(&h).await?;

    let args = payment_args(&h, "inv-1", 10 * USDC);
    let one_yocto = NearToken::from_yoctonear(1);
    assert_eq!(
        h.agent_refusal(&h.guard, "request_payment", args, one_yocto, AGENT_GAS).await?,
        json!({ "InvalidAccessKeyError": "DepositWithFunctionCall" }),
    );

    assert_eq!(state(&h).await?, before);
    let processed: bool =
        h.view(&h.guard, "is_request_processed", json!({ "request_id": "inv-1" })).await?;
    assert!(!processed);
    Ok(())
}

#[tokio::test]
async fn full_access_keys_of_other_accounts_are_not_agents() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let before = state(&h).await?;

    // Each signs with its own full access key. Not being in `policy.agents` is enough.
    for (signer, request_id) in [(&h.vendor, "inv-v"), (&h.council, "inv-c"), (&h.dao, "inv-d")] {
        let args = payment_args(&h, request_id, 10 * USDC);
        let outcome =
            h.call(signer, &h.guard, "request_payment", args, NO_DEPOSIT, AGENT_GAS).await?;
        check_panic(&outcome, "ERR_UNAUTHORIZED_AGENT")?;
    }

    assert_eq!(state(&h).await?, before);
    assert_eq!(h.usdc_balance(&h.vendor).await?, 0);
    Ok(())
}

#[tokio::test]
async fn council_members_cannot_skip_the_vote() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let before = state(&h).await?;
    let mut policy = before["policy"].clone();
    policy["max_payments_per_period"] = json!(100);

    let calls = [
        ("update_policy", json!({ "policy": policy })),
        ("unfreeze", json!({})),
        ("withdraw_near", json!({ "amount": NEAR.to_string(), "receiver_id": h.council })),
        (
            "withdraw_ft",
            json!({ "token_id": h.usdc, "amount": (100 * USDC).to_string(), "receiver_id": h.council }),
        ),
        (
            "withdraw_unallocated_near",
            json!({ "amount": NEAR.to_string(), "receiver_id": h.council }),
        ),
    ];
    for (method, args) in calls {
        let outcome = h.call(&h.council, &h.guard, method, args, NO_DEPOSIT, GAS).await?;
        check_panic(&outcome, "ERR_NOT_DAO")?;
    }
    assert_eq!(state(&h).await?, before);

    // Someone who is neither the DAO nor the guardian cannot freeze.
    let outcome = h.call(&h.vendor, &h.guard, "freeze", json!({}), NO_DEPOSIT, GAS).await?;
    check_panic(&outcome, "ERR_NOT_DAO_OR_GUARDIAN")?;
    assert_eq!(h.guard_status().await?["frozen"], false);

    // The council is the guardian, so it may freeze without a vote, but only the DAO unfreezes.
    let outcome = h.call_ok(&h.council, &h.guard, "freeze", json!({}), NO_DEPOSIT, GAS).await?;
    assert_eq!(event_names(&outcome), ["guard_frozen"]);
    assert_eq!(events(&outcome)[0]["data"], json!({ "by": h.council }));
    assert_eq!(h.guard_status().await?["frozen"], true);

    let outcome = h.call(&h.council, &h.guard, "unfreeze", json!({}), NO_DEPOSIT, GAS).await?;
    check_panic(&outcome, "ERR_NOT_DAO")?;
    assert_eq!(h.guard_status().await?["frozen"], true);

    h.dao_function_call(&h.guard, "unfreeze", &json!({}), 0, GAS).await?;
    assert_eq!(h.guard_status().await?["frozen"], false);
    Ok(())
}

#[tokio::test]
async fn approved_dao_proposals_change_the_policy_and_withdraw() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let dao_usdc = h.usdc_balance(&h.dao).await?;

    // Lower the USDC cap per payment from 100 to 50.
    let mut policy: Value = h.view(&h.guard, "get_policy", json!({})).await?;
    policy["token_limits"][0]["max_per_payment"] = json!((50 * USDC).to_string());
    h.dao_function_call(&h.guard, "update_policy", &json!({ "policy": policy }), 0, GAS).await?;
    assert_eq!(h.view::<Value>(&h.guard, "get_policy", json!({})).await?, policy);

    // 80 USDC fitted the old policy, under the new one it goes to a vote.
    let outcome = h.request_payment("inv-80", &h.vendor, Some(&h.usdc), 80 * USDC, None).await?;
    check(&outcome, "request_payment")?;
    assert!(returned(&outcome)?["Escalated"]["proposal_id"].is_u64());
    assert_eq!(events(&outcome)[0]["data"]["reason"], "EXCEEDS_PER_PAYMENT_CAP");

    // Withdraw 100 USDC and 1 NEAR of the budget back to the DAO.
    let args =
        json!({ "token_id": h.usdc, "amount": (100 * USDC).to_string(), "receiver_id": null });
    h.dao_function_call(&h.guard, "withdraw_ft", &args, 0, GAS).await?;
    assert_eq!(h.usdc_balance(&h.dao).await?, dao_usdc + 100 * USDC);
    assert_eq!(h.token_status(Some(&h.usdc)).await?["budget"], (900 * USDC).to_string());

    let args = json!({ "amount": NEAR.to_string(), "receiver_id": null });
    h.dao_function_call(&h.guard, "withdraw_near", &args, 0, GAS).await?;
    assert_eq!(h.guard_status().await?["near_budget"], (4 * NEAR).to_string());
    Ok(())
}

#[tokio::test]
async fn callbacks_and_init_are_private() -> Result<()> {
    let h = Harness::start(Options::default()).await?;
    let before = state(&h).await?;

    for method in [
        "on_payout_complete",
        "on_escalation_complete",
        "on_withdraw_complete",
        "fail_withdrawal",
        "new",
    ] {
        let outcome = h.call(&h.council, &h.guard, method, json!({}), NO_DEPOSIT, GAS).await?;
        check_panic(&outcome, &format!("Method {method} is private"))?;
    }

    assert_eq!(state(&h).await?, before);
    Ok(())
}
