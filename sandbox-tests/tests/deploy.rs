//! E4-S1: the testnet deploy, run on a local node with exactly the NEAR it asks for. The deploy
//! runs on Unix only, like the sandbox.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use near_api::types::transaction::result::ExecutionFinalResult;
use near_api::types::AccessKeyPermission;
use near_api::{Account, AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use near_sandbox::config::{
    DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY, DEFAULT_GENESIS_ACCOUNT_PUBLIC_KEY,
};
use near_sandbox::Sandbox;
use sandbox_tests::{
    account, check, deploy, full, returned, Options, AGENT_GAS, NEAR, STORAGE_PRICE_PER_BYTE, USDC,
};
use serde_json::{json, Value};

const MASTER: &str = "macstian.test.near";
/// Code hash of the pinned Sputnik wasm, see the README.
const SPUTNIK_CODE_HASH: &str = "DtHZA1mUmGYeScEaUoUZYQGGFHSMpnjMCvkr5U9hJXuF";

/// A fresh key directory, removed when the test ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A key directory with the master's key file in the near CLI format. The sandbox gives the
/// master the genesis key.
fn master_key_file(name: &str) -> Result<(TempDir, AccountId)> {
    let path = std::env::temp_dir().join(format!("tripwire-{name}-{}", std::process::id()));
    let dir = TempDir(path);
    std::fs::create_dir_all(&dir.0)?;
    let master = account(MASTER)?;
    let keys = json!({
        "account_id": MASTER,
        "public_key": DEFAULT_GENESIS_ACCOUNT_PUBLIC_KEY,
        "private_key": DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY,
    });
    std::fs::write(deploy::key_file(&dir.0, &master), keys.to_string())?;
    Ok((dir, master))
}

async fn send(
    network: &NetworkConfig,
    signer_id: &AccountId,
    signer: Arc<Signer>,
    contract: &AccountId,
    method: &str,
    args: Value,
    gas: NearGas,
) -> Result<ExecutionFinalResult> {
    let result = Contract(contract.clone())
        .call_function(method, args)
        .transaction()
        .gas(gas)
        .with_signer(signer_id.clone(), signer)
        .send_to(network)
        .await?;
    full(result)
}

/// Gives the master `balance` NEAR, plus the stake for its own storage when `plus_stake`.
async fn fund_master(
    sandbox: &Sandbox,
    network: &NetworkConfig,
    balance: NearToken,
    plus_stake: bool,
) -> Result<()> {
    sandbox.create_account(MASTER.parse()?).initial_balance(balance).send().await?;
    if plus_stake {
        let view = Account(account(MASTER)?).view().fetch_from(network).await?.data;
        let stake =
            NearToken::from_yoctonear(u128::from(view.storage_usage) * STORAGE_PRICE_PER_BYTE);
        let balance = balance.saturating_add(stake);
        sandbox.create_account(MASTER.parse()?).initial_balance(balance).send().await?;
    }
    Ok(())
}

fn assert_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{}", path.display());
    Ok(())
}

#[tokio::test]
async fn the_testnet_deploy_fits_its_balances_and_runs_the_demo() -> Result<()> {
    let sandbox = Sandbox::start_sandbox().await?;
    let network = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let opts = Options::testnet();
    fund_master(&sandbox, &network, deploy::required_balance(&opts), true).await?;
    let (dir, master) = master_key_file("full")?;
    assert_eq!(spendable(&network, &master).await?, deploy::required_balance(&opts));

    let (d, summary) = deploy::run(network.clone(), master.clone(), &dir.0, opts).await?;

    // The summary: the accounts, the pinned Sputnik code, a guard without keys, no secrets.
    assert_eq!(summary["network"], "sandbox");
    assert_eq!(summary["accounts"]["guard"], "guard.macstian.test.near");
    assert_eq!(summary["accounts"]["agent"], "agent.macstian.test.near");
    assert_eq!(summary["contracts"]["dao"]["code_hash"], SPUTNIK_CODE_HASH);
    assert_eq!(summary["guard_keys"], json!([]));
    assert_eq!(summary["agent_key"]["public_key"], d.agent_key.to_string());
    assert_eq!(summary["guard_policy"]["agents"], json!([d.agent]));
    assert!(summary["explorer"].is_null());
    let text = summary.to_string();
    assert!(!text.contains(&d.agent_secret.to_string()));
    assert!(!text.contains(DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY));

    // The key files: the agent file holds the function call key, the council file a full
    // access key of the council's own, not the master key. Both are readable by the owner only.
    let agent_file = deploy::key_file(&dir.0, &d.agent);
    let council_file = deploy::key_file(&dir.0, &d.council);
    let agent_signer = Signer::from_access_keyfile(agent_file.clone())?;
    let council_signer = Signer::from_access_keyfile(council_file.clone())?;
    assert_eq!(agent_signer.get_public_key().await?, d.agent_key);
    let council_key = council_signer.get_public_key().await?;
    assert_ne!(council_key.to_string(), DEFAULT_GENESIS_ACCOUNT_PUBLIC_KEY);
    assert!(!std::fs::read_to_string(&council_file)?.contains(DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY));
    assert_owner_only(&agent_file)?;
    assert_owner_only(&council_file)?;
    let key = Account(d.agent.clone()).access_key(d.agent_key).fetch_from(&network).await?;
    assert!(matches!(key.data.permission, AccessKeyPermission::FunctionCall(_)));
    let key = Account(d.council.clone()).access_key(council_key).fetch_from(&network).await?;
    assert!(matches!(key.data.permission, AccessKeyPermission::FullAccess));

    // Every account keeps NEAR to spare after paying for its code and state.
    let accounts = [&d.council, &d.dao, &d.guard, &d.usdc, &d.agent, &d.vendor];
    for id in accounts {
        let left = d.spendable(id).await?.as_yoctonear();
        eprintln!("{id}: {:.3} NEAR spendable", left as f64 / NEAR as f64);
        assert!(left >= NEAR / 20, "{id} has only {left} yoctoNEAR to spend");
    }
    let free_reserve: u128 = d.guard_status().await?["free_reserve"].as_str().unwrap().parse()?;
    eprintln!("guard free reserve: {:.3} NEAR", free_reserve as f64 / NEAR as f64);
    assert!(free_reserve >= 3 * d.proposal_bond.as_yoctonear(), "room for three escalations");

    // The demo with the written key files: a payment inside the policy, then one over the cap
    // that the council approves.
    let request = |id: &str, amount: u128| {
        json!({
            "request_id": id,
            "recipient": d.vendor,
            "token_id": d.usdc,
            "amount": amount.to_string(),
            "memo": null,
        })
    };
    let pay = request("inv-1", 50 * USDC);
    let outcome =
        send(&network, &d.agent, agent_signer.clone(), &d.guard, "request_payment", pay, AGENT_GAS)
            .await?;
    check(&outcome, "the payment")?;
    assert_eq!(returned(&outcome)?, json!({ "Paid": { "request_id": "inv-1" } }));

    let escalate = request("inv-2", 450 * USDC);
    let outcome =
        send(&network, &d.agent, agent_signer, &d.guard, "request_payment", escalate, AGENT_GAS)
            .await?;
    check(&outcome, "the escalation")?;
    let id = returned(&outcome)?["Escalated"]["proposal_id"].as_u64().unwrap();
    let kind = d.proposal(id).await?["kind"].clone();
    let vote = json!({ "id": id, "action": "VoteApprove", "proposal": kind });
    let gas = NearGas::from_tgas(300);
    let outcome =
        send(&network, &d.council, council_signer, &d.dao, "act_proposal", vote, gas).await?;
    check(&outcome, "the council vote")?;
    assert_eq!(d.proposal(id).await?["status"], "Approved");
    assert_eq!(d.usdc_balance(&d.vendor).await?, 500 * USDC);

    // A second run under the same master stops before it sends anything.
    let balance = d.near_balance(&master).await?;
    let again = deploy::run(network, master.clone(), &dir.0, Options::testnet()).await;
    let error = format!("{:#}", again.err().expect("the second run must fail"));
    assert!(error.contains("already exists"), "{error}");
    assert_eq!(d.near_balance(&master).await?, balance);
    Ok(())
}

#[tokio::test]
async fn the_deploy_counts_only_what_the_master_can_spend() -> Result<()> {
    let sandbox = Sandbox::start_sandbox().await?;
    let network = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let opts = Options::testnet();
    // The balance is exactly what the deploy needs, but part of it backs the master's storage.
    fund_master(&sandbox, &network, deploy::required_balance(&opts), false).await?;
    let (dir, master) = master_key_file("short")?;

    let result = deploy::run(network.clone(), master.clone(), &dir.0, opts).await;
    let error = format!("{:#}", result.err().expect("a short master must be refused"));
    assert!(error.contains("the setup needs"), "{error}");
    let council = account("council.macstian.test.near")?;
    assert!(!deploy::account_exists(&network, &council).await?);
    assert!(!deploy::key_file(&dir.0, &council).exists());
    Ok(())
}

#[tokio::test]
async fn the_account_check_takes_only_unknown_account_as_missing() -> Result<()> {
    let sandbox = Sandbox::start_sandbox().await?;
    let network = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    assert!(deploy::account_exists(&network, &account("sandbox")?).await?);
    assert!(!deploy::account_exists(&network, &account("nobody.test.near")?).await?);

    // An RPC that does not answer is an error, not a missing account, and the deploy stops.
    let dead = NetworkConfig::from_rpc_url("dead", "http://127.0.0.1:9".parse()?);
    let check = deploy::account_exists(&dead, &account("nobody.test.near")?).await;
    assert!(check.is_err());
    let (dir, master) = master_key_file("dead")?;
    let result = deploy::run(dead, master, &dir.0, Options::testnet()).await;
    let error = format!("{:#}", result.err().expect("the deploy must stop"));
    assert!(error.contains("checking whether"), "{error}");
    Ok(())
}

/// NEAR the account can spend: its balance less the stake for its storage.
async fn spendable(network: &NetworkConfig, id: &AccountId) -> Result<NearToken> {
    let view = Account(id.clone()).view().fetch_from(network).await?.data;
    let stake = u128::from(view.storage_usage) * STORAGE_PRICE_PER_BYTE;
    Ok(NearToken::from_yoctonear(view.amount.as_yoctonear() - stake))
}
