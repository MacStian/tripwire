//! Deploys the Tripwire demo setup to testnet under a master account you own.
//!
//! ```sh
//! scripts/deploy-testnet.sh <master>.testnet
//! ```
//!
//! Reads the master key from `~/.near-credentials/testnet/<master>.json`, the near CLI legacy
//! keychain, and writes the agent and council key files next to it. Writes the public summary
//! to `deployments/testnet.json`. `NEAR_RPC_URL` and `NEAR_CREDENTIALS_DIR` override the RPC
//! endpoint and the key directory.

use std::path::PathBuf;

use anyhow::{bail, ensure, Context, Result};
use near_api::NetworkConfig;
use sandbox_tests::{account, deploy, Options};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [master] = args.as_slice() else {
        bail!("usage: deploy <master account>, for example macstian.testnet");
    };
    let master = account(master)?;
    let network = match std::env::var("NEAR_RPC_URL") {
        Ok(url) => NetworkConfig::from_rpc_url("testnet", url.parse()?),
        Err(_) => NetworkConfig::testnet(),
    };
    let credentials = match std::env::var_os("NEAR_CREDENTIALS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join(".near-credentials/testnet"),
    };
    let out = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../deployments/testnet.json"));
    ensure!(!out.exists(), "{} already exists, move it away to deploy again", out.display());

    let opts = Options::testnet();
    println!("Deploying under {master}, which needs {}", deploy::required_balance(&opts));
    let (deployment, summary) = deploy::run(network, master, &credentials, opts).await?;
    std::fs::create_dir_all(out.parent().context("no parent directory")?)?;
    std::fs::write(&out, serde_json::to_string_pretty(&summary)? + "\n")?;

    println!("Guard: {}", deployment.guard);
    println!("DAO: {}", deployment.dao);
    println!("Summary: {}", out.display());
    println!("Agent key: {}", deploy::key_file(&credentials, &deployment.agent).display());
    println!("Council key: {}", deploy::key_file(&credentials, &deployment.council).display());
    Ok(())
}
