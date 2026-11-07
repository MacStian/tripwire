//! E2-S2: gas of every guard call and callback in its worst case, measured against the static
//! gas the guard reserves for it. Every request runs with exactly `MIN_PREPAID_GAS`.
//! `cargo test -p sandbox-tests --test gas -- --nocapture` prints the table.

use anyhow::{bail, Context, Result};
use guard::{
    GAS_ADD_PROPOSAL, GAS_FAIL_WITHDRAWAL, GAS_FT_TRANSFER, GAS_ON_ESCALATION, GAS_ON_PAYOUT,
    GAS_ON_WITHDRAW, MIN_PREPAID_GAS,
};
use near_api::{AccountId, NearGas};
use near_sdk::Gas;
use sandbox_tests::{
    account, receipts, returned, Harness, Options, ReceiptGas, AGENT, NEAR, USDC, VENDOR,
};
use serde_json::{json, Value};

/// Allowlisted, but without storage on the token, so `ft_transfer` to it fails.
const UNREGISTERED: &str = "unregistered.test.near";
/// Allowlisted, but the account does not exist, so a NEAR transfer to it fails.
const MISSING: &str = "missing.test.near";

fn gas(value: Gas) -> NearGas {
    NearGas::from_gas(value.as_gas())
}

/// The largest policy validation accepts. The accounts in use sit at the end of each list,
/// so every lookup scans the whole list.
fn max_policy(policy: &mut Value) {
    let mut agents: Vec<String> = (1..10).map(|i| format!("agent{i}.test.near")).collect();
    agents.push(AGENT.to_string());
    let mut recipients: Vec<String> = (1..=47).map(|i| format!("vendor{i}.test.near")).collect();
    recipients.extend([UNREGISTERED, MISSING, VENDOR].map(String::from));
    let mut limits: Vec<Value> = (1..=8)
        .map(|i| {
            json!({
                "token_id": format!("token{i}.test.near"),
                "max_per_payment": "1",
                "max_per_period": "1",
            })
        })
        .collect();
    // The default limits are USDC then NEAR. Reversed, USDC is the last entry.
    limits.extend(policy["token_limits"].as_array().unwrap().iter().rev().cloned());
    policy["agents"] = json!(agents);
    policy["recipients"] = json!(recipients);
    policy["token_limits"] = json!(limits);
}

/// A request id of the maximum length, 64 characters.
fn request_id(tag: &str) -> String {
    format!("{tag}-{}", "0".repeat(63 - tag.len()))
}

struct Row {
    what: &'static str,
    burnt: NearGas,
    /// Static gas the guard reserves for this receipt, if it reserves any.
    reserved: Option<NearGas>,
}

#[derive(Default)]
struct Table {
    rows: Vec<Row>,
}

impl Table {
    fn add(&mut self, what: &'static str, receipt: &ReceiptGas, reserved: Option<NearGas>) {
        self.rows.push(Row { what, burnt: receipt.gas, reserved });
    }

    fn print(&self) {
        let tgas = |g: NearGas| g.as_gas() as f64 / 1e12;
        println!("{:<44} {:>10} {:>10} {:>7}", "receipt", "burnt", "reserved", "margin");
        for row in &self.rows {
            match row.reserved {
                Some(reserved) => println!(
                    "{:<44} {:>10.2} {:>10.2} {:>6.1}x",
                    row.what,
                    tgas(row.burnt),
                    tgas(reserved),
                    tgas(reserved) / tgas(row.burnt),
                ),
                None => {
                    println!("{:<44} {:>10.2} {:>10} {:>7}", row.what, tgas(row.burnt), "-", "-")
                }
            }
        }
    }

    /// Every receipt with reserved gas must burn at most half of it.
    fn short_margins(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter_map(|row| {
                let reserved = row.reserved?;
                (row.burnt.as_gas() * 2 > reserved.as_gas()).then(|| row.what.to_string())
            })
            .collect()
    }
}

fn first<'a>(receipts: &'a [ReceiptGas], executor: &AccountId) -> Result<&'a ReceiptGas> {
    receipts.iter().find(|r| &r.executor == executor).context("no receipt on that account")
}

fn with_event<'a>(receipts: &'a [ReceiptGas], event: &str) -> Result<&'a ReceiptGas> {
    receipts
        .iter()
        .find(|r| r.events.iter().any(|e| e == event))
        .with_context(|| format!("no receipt emitted {event}"))
}

/// Sends a request with exactly `MIN_PREPAID_GAS` and returns its outcome and receipts.
async fn request(
    h: &Harness,
    tag: &str,
    recipient: &str,
    token_id: Option<&AccountId>,
    amount: u128,
    memo: Option<&str>,
) -> Result<(Value, Vec<ReceiptGas>)> {
    let id = request_id(tag);
    let recipient = account(recipient)?;
    let outcome = h
        .request_payment_with_gas(&id, &recipient, token_id, amount, memo, gas(MIN_PREPAID_GAS))
        .await?;
    if outcome.is_failure() {
        bail!("{tag} failed: {:?}", outcome.failures());
    }
    Ok((returned(&outcome)?, receipts(&outcome)))
}

#[tokio::test]
async fn worst_case_gas_fits_the_reserved_gas() -> Result<()> {
    let h = Harness::start(Options { policy: max_policy, ..Options::default() }).await?;
    let usdc = h.usdc.clone();
    let memo =
        "Invoice #0042 hosting and support for period 2026-11, see the contract annex / ".repeat(3);
    let mut table = Table::default();
    let min_prepaid = gas(MIN_PREPAID_GAS);

    // FT payout, then one that fails in ft_transfer and is rolled back.
    let (value, r) = request(&h, "ft-paid", VENDOR, Some(&usdc), 10 * USDC, None).await?;
    assert!(value.get("Paid").is_some(), "{value}");
    table.add("request_payment, FT payout", &r[0], None);
    table.add("ft_transfer (mock USDC)", first(&r, &usdc)?, Some(gas(GAS_FT_TRANSFER)));
    table.add(
        "on_payout_complete, paid",
        with_event(&r, "payment_executed")?,
        Some(gas(GAS_ON_PAYOUT)),
    );
    let ft_request = r[0].gas;

    let (value, r) = request(&h, "ft-failed", UNREGISTERED, Some(&usdc), 10 * USDC, None).await?;
    assert!(value.get("PaymentFailed").is_some(), "{value}");
    table.add(
        "on_payout_complete, FT rolled back",
        with_event(&r, "payment_failed")?,
        Some(gas(GAS_ON_PAYOUT)),
    );

    // NEAR payout, then one to an account that does not exist.
    let (value, r) = request(&h, "near-paid", VENDOR, None, NEAR / 10, None).await?;
    assert!(value.get("Paid").is_some(), "{value}");
    table.add("request_payment, NEAR payout", &r[0], None);
    table.add(
        "on_payout_complete, NEAR paid",
        with_event(&r, "payment_executed")?,
        Some(gas(GAS_ON_PAYOUT)),
    );
    let near_request = r[0].gas;

    let (value, r) = request(&h, "near-failed", MISSING, None, NEAR / 10, None).await?;
    assert!(value.get("PaymentFailed").is_some(), "{value}");
    table.add(
        "on_payout_complete, NEAR rolled back",
        with_event(&r, "payment_failed")?,
        Some(gas(GAS_ON_PAYOUT)),
    );

    // Escalation with the longest description: over the per-payment cap, maximum memo.
    let (value, r) = request(&h, "escalated", VENDOR, Some(&usdc), 150 * USDC, Some(&memo)).await?;
    assert!(value["Escalated"]["proposal_id"].is_u64(), "{value}");
    table.add("request_payment, escalation", &r[0], None);
    table.add("add_proposal (Sputnik 3.2)", first(&r, &h.dao)?, Some(gas(GAS_ADD_PROPOSAL)));
    table.add(
        "on_escalation_complete, escalated",
        with_event(&r, "payment_escalated")?,
        Some(gas(GAS_ON_ESCALATION)),
    );
    let escalation_request = r[0].gas;

    // A bond the DAO does not accept makes add_proposal fail and the escalation roll back.
    let mut policy: Value = h.view(&h.guard, "get_policy", json!({})).await?;
    policy["proposal_bond"] = json!("0");
    let args = json!({ "policy": policy });
    let (_, outcome) = h
        .dao_function_call_outcome(&h.guard, "update_policy", &args, 0, NearGas::from_tgas(100))
        .await?;
    let r = receipts(&outcome);
    table.add("update_policy, maximum policy", first(&r, &h.guard)?, None);
    let (value, r) =
        request(&h, "esc-failed", VENDOR, Some(&usdc), 150 * USDC, Some(&memo)).await?;
    assert!(value.get("EscalationFailed").is_some(), "{value}");
    table.add(
        "on_escalation_complete, rolled back",
        with_event(&r, "escalation_failed")?,
        Some(gas(GAS_ON_ESCALATION)),
    );

    // DAO withdrawals: one that arrives, one to an account without token storage.
    let args = json!({ "token_id": usdc, "amount": (10 * USDC).to_string(), "receiver_id": null });
    let (id, outcome) = h
        .dao_function_call_outcome(&h.guard, "withdraw_ft", &args, 0, NearGas::from_tgas(100))
        .await?;
    assert_eq!(h.proposal(id).await?["status"], "Approved");
    let r = receipts(&outcome);
    table.add("withdraw_ft", first(&r, &h.guard)?, None);
    table.add(
        "on_withdraw_complete, withdrawn",
        with_event(&r, "budget_withdrawn")?,
        Some(gas(GAS_ON_WITHDRAW)),
    );

    let args =
        json!({ "token_id": usdc, "amount": (10 * USDC).to_string(), "receiver_id": UNREGISTERED });
    let (id, outcome) = h
        .dao_function_call_outcome(&h.guard, "withdraw_ft", &args, 0, NearGas::from_tgas(100))
        .await?;
    assert_eq!(h.proposal(id).await?["status"], "Failed");
    let r = receipts(&outcome);
    table.add(
        "on_withdraw_complete, failed",
        with_event(&r, "withdrawal_failed")?,
        Some(gas(GAS_ON_WITHDRAW)),
    );
    let fail = r
        .iter()
        .find(|receipt| receipt.executor == h.guard && receipt.failed && receipt.events.is_empty())
        .context("no fail_withdrawal receipt")?;
    table.add("fail_withdrawal", fail, Some(gas(GAS_FAIL_WITHDRAWAL)));

    table.print();

    // The prepaid minimum covers the request itself twice over, plus the static gas of the
    // outgoing call and of the callback.
    let mut short = table.short_margins();
    for (what, burnt, out, cb) in [
        ("minimum for an FT payout", ft_request, GAS_FT_TRANSFER, GAS_ON_PAYOUT),
        ("minimum for a NEAR payout", near_request, Gas::from_gas(0), GAS_ON_PAYOUT),
        ("minimum for an escalation", escalation_request, GAS_ADD_PROPOSAL, GAS_ON_ESCALATION),
    ] {
        let needed = burnt.as_gas() * 2 + out.as_gas() + cb.as_gas();
        println!(
            "{what:<44} {:>10.2} of {:.2} TGas",
            needed as f64 / 1e12,
            min_prepaid.as_gas() as f64 / 1e12
        );
        if needed > min_prepaid.as_gas() {
            short.push(what.to_string());
        }
    }
    assert!(short.is_empty(), "less than 2x margin: {short:?}");
    Ok(())
}
