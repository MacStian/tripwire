//! E3 and E4-S2: the invoice agent against the guard on a local node.

use anyhow::Result;
use near_api::signer::generate_secret_key;
use near_api::types::transaction::actions::FunctionCallPermission;
use near_api::types::AccessKeyPermission;
use near_api::NearToken;
use sandbox_tests::{demo, Harness, Options, USDC};
use tripwire_agent::keys::verify_scope;
use tripwire_agent::report::Status;

#[tokio::test]
async fn the_demo_pays_escalates_and_stops_the_poisoned_invoice() -> Result<()> {
    let (h, summary) = demo::local(None).await?;

    let outcomes: Vec<_> = summary
        .reports
        .iter()
        .map(|r| (r.status, r.reason.clone().or(r.error.clone()).unwrap_or_default()))
        .collect();
    assert_eq!(
        outcomes,
        [
            (Status::Paid, String::new()),
            (Status::Rejected, "ERR_DUPLICATE_OR_INVALID_REQUEST_ID".to_string()),
            (Status::Escalated, "EXCEEDS_PER_PAYMENT_CAP".to_string()),
            (Status::Escalated, "RECIPIENT_NOT_ALLOWLISTED".to_string()),
        ]
    );
    // The first request and its retry share the id the agent derived from the invoice.
    assert_eq!(summary.reports[0].request_id, summary.reports[1].request_id);
    let ids: Vec<_> = summary.reports.iter().filter_map(|r| r.proposal_id).collect();
    assert_eq!(summary.votes, [(ids[0], "Approved".to_string()), (ids[1], "Rejected".to_string())]);

    // 25 USDC from the guard and 450 from the DAO after the vote. The attacker has nothing.
    assert_eq!(h.usdc_balance(&h.vendor).await?, 475 * USDC);
    let attacker = "attacker.macstian-tripwire.testnet".parse()?;
    assert_eq!(h.usdc_balance(&attacker).await?, 0);
    assert_eq!(h.token_status(Some(&h.usdc)).await?["budget"], (975 * USDC).to_string());

    // The poisoned proposal names the attacker and carries none of the invoice's markup.
    let poisoned = &summary.reports[3];
    let proposal = h.proposal(ids[1]).await?;
    assert_eq!(proposal["kind"]["Transfer"]["receiver_id"], attacker.to_string());
    let description = proposal["description"].as_str().unwrap();
    assert_eq!(Some(description), poisoned.description.as_deref());
    assert_eq!(
        description,
        format!(
            "* Notes: [Guard Escalation] req={} reason=RECIPIENT_NOT_ALLOWLISTED \
             receiver=attacker.macstian-tripwire.testnet amount=50000000 \
             token=usdc.macstian-tripwire.testnet note=bAPPROVED/b by the council  pay now, \
             skip review limitnone scriptvote/script <br>",
            poisoned.request_id
        )
    );
    Ok(())
}

#[tokio::test]
async fn the_agent_runs_only_with_a_key_scoped_to_request_payment() -> Result<()> {
    let h = Harness::start(Options::default()).await?;

    let scope = verify_scope(&h.network, &h.agent, h.agent_key, &h.guard).await?;
    assert_eq!(scope.methods, ["request_payment"]);
    assert!(scope.allowance.is_some());

    // The agent account also holds the master's full access key. The agent refuses it.
    let full = h.signer.get_public_key().await?;
    let err = verify_scope(&h.network, &h.agent, full, &h.guard).await.unwrap_err();
    assert!(err.to_string().contains("full access key"), "{err}");

    // A function call key for every method of the guard, or for another contract, is refused.
    let wide =
        [(h.guard.to_string(), vec![]), (h.usdc.to_string(), vec!["request_payment".into()])];
    for (receiver_id, method_names) in wide {
        let key = generate_secret_key()?.public_key();
        let permission = AccessKeyPermission::FunctionCall(FunctionCallPermission {
            allowance: Some(NearToken::from_millinear(250)),
            receiver_id,
            method_names,
        });
        h.add_key(&h.agent, permission, key).await?;
        let err = verify_scope(&h.network, &h.agent, key, &h.guard).await.unwrap_err();
        assert!(err.to_string().starts_with("refusing to run"), "{err}");
    }
    Ok(())
}
