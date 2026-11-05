//! E2-S1: the harness builds the setup the other sandbox tests rely on.

use near_api::types::AccessKeyPermission;
use near_api::{Account, NearToken};
use sandbox_tests::{Harness, Options, DAO_USDC, GUARD_ROLE, NEAR, USDC};
use serde_json::{json, Value};

#[tokio::test]
async fn default_setup() -> anyhow::Result<()> {
    let h = Harness::start(Options::default()).await?;

    // The DAO: one council member, the guard role and the bond the guard expects.
    let policy: Value = h.view(&h.dao, "get_policy", json!({})).await?;
    let bond = h.proposal_bond.as_yoctonear().to_string();
    assert_eq!(policy["proposal_bond"], bond);
    let roles = policy["roles"].as_array().unwrap();
    let council = roles.iter().find(|r| r["name"] == "council").unwrap();
    assert_eq!(council["kind"], json!({ "Group": [h.council] }));
    let guard_role = roles.iter().find(|r| r["name"] == GUARD_ROLE).unwrap();
    assert_eq!(guard_role["kind"], json!({ "Group": [h.guard] }));
    assert_eq!(guard_role["permissions"], json!(["transfer:AddProposal"]));

    // The token: registered holders, the DAO's USDC minus what it moved to the guard.
    for holder in [&h.dao, &h.guard, &h.vendor] {
        let storage: Value =
            h.view(&h.usdc, "storage_balance_of", json!({ "account_id": holder })).await?;
        assert!(!storage.is_null(), "{holder} has no storage on the token");
    }
    let metadata: Value = h.view(&h.usdc, "ft_metadata", json!({})).await?;
    assert_eq!(metadata["decimals"], 6);
    assert_eq!(h.usdc_balance(&h.dao).await?, DAO_USDC - 1_000 * USDC);
    assert_eq!(h.usdc_balance(&h.guard).await?, 1_000 * USDC);
    assert_eq!(h.usdc_balance(&h.vendor).await?, 0);

    // The guard: tied to the DAO, budgets credited by the approved proposals.
    let status = h.guard_status().await?;
    assert_eq!(status["dao_id"], h.dao.to_string());
    assert_eq!(status["frozen"], false);
    assert_eq!(status["near_budget"], (5 * NEAR).to_string());
    let usdc = status["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["token_id"] == h.usdc.to_string())
        .unwrap();
    assert_eq!(usdc["budget"], (1_000 * USDC).to_string());
    assert_eq!(usdc["spent_in_period"], "0");
    assert_eq!(status["policy"]["agents"], json!([h.agent]));
    assert_eq!(status["policy"]["proposal_bond"], bond);

    // The agent: a function call key that only reaches request_payment on the guard.
    let key = Account(h.agent.clone()).access_key(h.agent_key).fetch_from(&h.network).await?;
    let AccessKeyPermission::FunctionCall(permission) = key.data.permission else {
        panic!("the agent key has full access");
    };
    assert_eq!(permission.receiver_id, h.guard.to_string());
    assert_eq!(permission.method_names, vec!["request_payment".to_string()]);
    assert_eq!(permission.allowance, Some(sandbox_tests::AGENT_KEY_ALLOWANCE));
    Ok(())
}

#[tokio::test]
async fn custom_bond_and_no_funding() -> anyhow::Result<()> {
    let opts = Options {
        proposal_bond: NearToken::from_near(1),
        near_budget: NearToken::from_near(0),
        usdc_budget: 0,
        policy: |policy| policy["max_escalations_per_period"] = json!(1),
    };
    let h = Harness::start(opts).await?;

    let policy: Value = h.view(&h.dao, "get_policy", json!({})).await?;
    assert_eq!(policy["proposal_bond"], NEAR.to_string());

    let status = h.guard_status().await?;
    assert_eq!(status["policy"]["proposal_bond"], NEAR.to_string());
    assert_eq!(status["policy"]["max_escalations_per_period"], 1);
    assert_eq!(status["near_budget"], "0");
    assert_eq!(h.usdc_balance(&h.guard).await?, 0);
    assert_eq!(h.usdc_balance(&h.dao).await?, DAO_USDC);
    Ok(())
}
