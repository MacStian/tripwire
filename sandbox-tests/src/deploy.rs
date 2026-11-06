//! E4-S1: the demo setup on a public network.
//!
//! [`run`] creates the same accounts and contracts as the sandbox tests under a master account
//! you own, writes the key files the demo needs next to the master key file and returns a
//! summary for `deployments/<network>.json`. Before it sends anything it builds both contracts,
//! writes the key files and stops when an account it would create already exists, when a key
//! file it would write is already there or when the master cannot spend enough NEAR.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use near_api::signer::generate_secret_key;
use near_api::types::AccessKeyPermission;
use near_api::{AccountId, NearToken, NetworkConfig, Signer};
use serde_json::{json, Map, Value};

use crate::{Deployment, Options, AGENT_KEY_ALLOWANCE, SPUTNIK_COMMIT};

/// What the master pays on top of the balances it hands out: the gas of the six account
/// creations. Every other transaction is signed, and paid for, by the account it acts as.
pub const GAS_MARGIN: NearToken = NearToken::from_millinear(100);

/// NEAR the master must be able to spend for [`run`], see [`crate::spendable_balance`].
pub fn required_balance(opts: &Options) -> NearToken {
    opts.balances.total().saturating_add(GAS_MARGIN)
}

/// Deadline of one RPC call, connection and full response, the same as near-api uses.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(15);

/// Where the near CLI legacy keychain keeps the key of `id`.
pub fn key_file(credentials: &Path, id: &AccountId) -> PathBuf {
    credentials.join(format!("{id}.json"))
}

/// Deploys under `master`, signing with its key file in `credentials`.
///
/// Writes two key files to `credentials` before it sends anything: the agent's function call
/// key, for the agent process, and a full access key of the council's own, for the votes in
/// the demo. The council key reaches only the council account. The master key stays on every
/// account the setup creates except the guard.
pub async fn run(
    network: NetworkConfig,
    master: AccountId,
    credentials: &Path,
    opts: Options,
) -> Result<(Deployment, Value)> {
    ensure!(
        cfg!(unix),
        "run the deploy on Linux or macOS (WSL on Windows), where the key files it writes can \
         be made readable by the owner only"
    );
    let master_file = key_file(credentials, &master);
    let signer = Signer::from_access_keyfile(master_file.clone())
        .with_context(|| format!("reading the master key {}", master_file.display()))?;

    // A failed build must not leave funded accounts behind, so build before anything is sent.
    // The builds panic with the cargo near output.
    std::panic::catch_unwind(crate::guard_wasm)
        .map_err(|_| anyhow!("building the guard failed, nothing was sent"))?;
    std::panic::catch_unwind(crate::mock_ft_wasm)
        .map_err(|_| anyhow!("building the mock token failed, nothing was sent"))?;

    let deployment = Deployment::new(network, master, signer, opts.proposal_bond)?;
    for id in Deployment::account_names(&deployment.master)? {
        let exists = account_exists(&deployment.network, &id).await?;
        ensure!(!exists, "{id} already exists, deploy under another master account");
    }
    let agent_file = key_file(credentials, &deployment.agent);
    let council_file = key_file(credentials, &deployment.council);
    for file in [&agent_file, &council_file] {
        ensure!(!file.exists(), "{} already exists", file.display());
    }
    let spendable = deployment.spendable(&deployment.master).await?;
    let needed = required_balance(&opts);
    ensure!(
        spendable >= needed,
        "{} can spend {spendable}, the setup needs {needed}",
        deployment.master
    );

    // Keys first, so a failure later cannot lose one.
    let council_secret = generate_secret_key()?;
    let council_key = council_secret.public_key();
    write_key_file(
        &agent_file,
        &deployment.agent,
        &deployment.agent_key.to_string(),
        &deployment.agent_secret.to_string(),
    )?;
    write_key_file(
        &council_file,
        &deployment.council,
        &council_key.to_string(),
        &council_secret.to_string(),
    )?;

    async {
        deployment.create_accounts(&opts.balances).await?;
        deployment.set_up(&opts).await?;
        deployment.add_key(&deployment.council, AccessKeyPermission::FullAccess, council_key).await
    }
    .await
    .context(
        "the deploy stopped partway, see the Testnet section of sandbox-tests/README.md to clean up",
    )?;

    let summary = summary(&deployment, &opts)
        .await
        .context("the deploy finished, but reading its summary failed")?;
    Ok((deployment, summary))
}

/// Whether `id` exists, at final finality. Only the RPC's `UNKNOWN_ACCOUNT` counts as missing.
/// Every other answer, an RPC error or no full answer within [`RPC_TIMEOUT`], is an error.
pub async fn account_exists(network: &NetworkConfig, id: &AccountId) -> Result<bool> {
    let rpc = network.rpc_endpoints.first().context("no RPC endpoint")?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": "tripwire",
        "method": "query",
        "params": { "request_type": "view_account", "finality": "final", "account_id": id },
    });
    let response: Value = async {
        let client =
            reqwest::Client::builder().connect_timeout(RPC_TIMEOUT).timeout(RPC_TIMEOUT).build()?;
        let response = client.post(rpc.url.clone()).json(&request).send().await?;
        response.error_for_status()?.json().await
    }
    .await
    .with_context(|| format!("checking whether {id} exists"))?;
    if response["result"].is_object() {
        return Ok(true);
    }
    if response["error"]["cause"]["name"] == "UNKNOWN_ACCOUNT" {
        return Ok(false);
    }
    bail!("checking whether {id} exists failed: {response}")
}

/// Writes a key file in the near CLI format, readable by the owner only, and syncs it to disk.
/// Never overwrites.
fn write_key_file(path: &Path, id: &AccountId, public_key: &str, private_key: &str) -> Result<()> {
    let body = json!({ "account_id": id, "public_key": public_key, "private_key": private_key });
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("creating {}", path.display()))?;
    file.write_all(serde_json::to_string(&body)?.as_bytes())?;
    file.sync_all().with_context(|| format!("syncing {}", path.display()))?;
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
