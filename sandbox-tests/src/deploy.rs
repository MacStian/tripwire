//! E4-S1: the demo setup on a public network.
//!
//! [`run`] creates the same accounts and contracts as the sandbox tests under a master account
//! you own, writes the key files the demo needs next to the master key file and returns a
//! summary for `deployments/<network>.json`. It stops before creating anything when the master
//! holds too little NEAR, when an account it would create already exists or when a key file it
//! would write is already there.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use near_api::{Account, AccountId, NearToken, NetworkConfig, Signer};
use serde_json::{json, Map, Value};

use crate::{Deployment, Options, AGENT_KEY_ALLOWANCE, SPUTNIK_COMMIT};

/// What the master pays on top of the balances it hands out: the gas of the six account
/// creations. Every other transaction is signed, and paid for, by the account it acts as.
pub const GAS_MARGIN: NearToken = NearToken::from_millinear(100);

/// NEAR the master needs for [`run`].
pub fn required_balance(opts: &Options) -> NearToken {
    opts.balances.total().saturating_add(GAS_MARGIN)
}

/// Where the near CLI legacy keychain keeps the key of `id`.
pub fn key_file(credentials: &Path, id: &AccountId) -> PathBuf {
    credentials.join(format!("{id}.json"))
}

/// Deploys under `master`, signing with its key file in `credentials`.
///
/// Writes two key files to `credentials`: the agent's function call key, for the agent
/// process, and the council's full access key, for the votes in the demo. The council key is
/// the master key, like on every account the setup creates.
pub async fn run(
    network: NetworkConfig,
    master: AccountId,
    credentials: &Path,
    opts: Options,
) -> Result<(Deployment, Value)> {
    let master_file = key_file(credentials, &master);
    let signer = Signer::from_access_keyfile(master_file.clone())
        .with_context(|| format!("reading the master key {}", master_file.display()))?;
    let master_keys: Value = serde_json::from_str(&std::fs::read_to_string(&master_file)?)?;

    let accounts = Deployment::account_names(&master)?;
    for id in &accounts {
        let existing = Account(id.clone()).view().fetch_from(&network).await;
        ensure!(existing.is_err(), "{id} already exists, deploy under another master account");
    }
    let [council, _, _, _, agent, _] = &accounts;
    let agent_file = key_file(credentials, agent);
    let council_file = key_file(credentials, council);
    for file in [&agent_file, &council_file] {
        ensure!(!file.exists(), "{} already exists", file.display());
    }
    let balance = Account(master.clone())
        .view()
        .fetch_from(&network)
        .await
        .with_context(|| format!("reading {master}"))?
        .data
        .amount;
    let needed = required_balance(&opts);
    ensure!(balance >= needed, "{master} holds {balance}, the setup needs {needed}");

    let deployment = Deployment::create(network, master, signer, opts).await?;
    write_key_file(
        &agent_file,
        &deployment.agent,
        &deployment.agent_key.to_string(),
        &deployment.agent_secret.to_string(),
    )?;
    write_key_file(
        &council_file,
        &deployment.council,
        master_keys["public_key"].as_str().context("no public_key in the master key file")?,
        master_keys["private_key"].as_str().context("no private_key in the master key file")?,
    )?;
    let summary = summary(&deployment, &opts).await?;
    Ok((deployment, summary))
}

/// Writes a key file in the near CLI format, readable by the owner only. Never overwrites.
fn write_key_file(path: &Path, id: &AccountId, public_key: &str, private_key: &str) -> Result<()> {
    let body = json!({ "account_id": id, "public_key": public_key, "private_key": private_key });
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("creating {}", path.display()))?;
    file.write_all(serde_json::to_string(&body)?.as_bytes())?;
    Ok(())
}

/// Public facts about the deployment, safe to commit: accounts, code hashes, the agent's public
/// key and its limits, the guard policy and explorer links on testnet. No secrets.
async fn summary(d: &Deployment, opts: &Options) -> Result<Value> {
    let policy: Value = d.view(&d.guard, "get_policy", json!({})).await?;
    let accounts = [
        ("council", &d.council),
        ("dao", &d.dao),
        ("guard", &d.guard),
        ("usdc", &d.usdc),
        ("agent", &d.agent),
        ("vendor", &d.vendor),
    ];
    let ids: Map<String, Value> =
        accounts.iter().map(|(name, id)| (name.to_string(), json!(id))).collect();
    let explorer = (d.network.network_name == "testnet").then(|| {
        let links = accounts.iter().map(|(name, id)| {
            (name.to_string(), json!(format!("https://testnet.nearblocks.io/address/{id}")))
        });
        Value::Object(links.collect())
    });
    let sputnik = format!("near-daos/sputnik-dao-contract@{SPUTNIK_COMMIT}");
    Ok(json!({
        "network": d.network.network_name,
        "master": d.master,
        "accounts": ids,
        "contracts": {
            "dao": { "code_hash": d.code_hash(&d.dao).await?, "source": sputnik },
            "guard": { "code_hash": d.code_hash(&d.guard).await?, "source": "contracts/guard" },
            "usdc": { "code_hash": d.code_hash(&d.usdc).await?, "source": "contracts/mock-ft" },
        },
        "agent_key": {
            "public_key": d.agent_key,
            "receiver_id": d.guard,
            "method_names": ["request_payment"],
            "allowance": AGENT_KEY_ALLOWANCE.as_yoctonear().to_string(),
        },
        "guard_keys": d.access_keys(&d.guard).await?,
        "budgets": {
            "near": opts.near_budget.as_yoctonear().to_string(),
            "usdc": opts.usdc_budget.to_string(),
        },
        "guard_policy": policy,
        "explorer": explorer,
    }))
}
