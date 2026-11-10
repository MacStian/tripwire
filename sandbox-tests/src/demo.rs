//! E4-S2: the three invoice demo of `tripwire-agent` on a local node or on testnet.
//!
//! Locally it needs nothing: a fresh node gets the full setup under the testnet account names,
//! so the invoices in `invoices/` work unchanged, and the agent reads its function call key
//! from a key file as it does on testnet. On testnet it reads `deployments/testnet.json` and
//! the key files in `~/.near-credentials/testnet`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use near_api::{AccountId, NetworkConfig, Signer};
use serde_json::Value;
use tripwire_agent::demo::{run_tag, Council, Demo, Summary};
use tripwire_agent::guard::Target;
use tripwire_agent::{keys, Agent};

use crate::{account, deploy, Harness, Options};

/// The master of the testnet deployment. The local demo uses the same account names.
pub const DEMO_MASTER: &str = "macstian-tripwire.testnet";

/// The invoice files of the demo.
pub fn invoices_dir() -> PathBuf {
    repo_root().join("invoices")
}

/// The repository root.
pub fn repo_root() -> PathBuf {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."));
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

/// A directory removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("tripwire-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Starts a local node with the setup under [`DEMO_MASTER`] and runs the demo. The agent's key
/// goes into a key file, readable by the owner only, and the agent loads it from there.
pub async fn local(csv: Option<PathBuf>) -> Result<(Harness, Summary)> {
    let h = Harness::start_under(DEMO_MASTER, Options::testnet()).await?;
    let dir = TempDir::new("demo-keys")?;
    let file = keys::key_file(&dir.0, &h.agent);
    deploy::write_key_file(&file, &h.agent, &h.agent_key.to_string(), &h.agent_secret.to_string())?;
    let key = keys::load(&h.agent, "sandbox", Some(&file)).await?;
    let target = Target { network: h.network.clone(), guard: h.guard.clone() };
    let agent = Agent::connect(target, h.agent.clone(), key).await?;
    // The local node gives every account the genesis key, the council included.
    let council = Council { id: h.council.clone(), signer: h.signer.clone() };
    let demo = Demo { agent, council: Some(council), invoices: invoices_dir(), run: None, csv };
    let summary = demo.run().await?;
    Ok((h, summary))
}

/// Runs the demo against the deployment in `deployment` (`deployments/testnet.json`).
///
/// The agent key comes from `TRIPWIRE_AGENT_KEY` or the agent's key file. With the council's
/// key file in the same directory the council votes, otherwise the proposals stay open.
/// `run` is appended to every invoice number, by default a tag from the clock, because the
/// guard pays each invoice once.
pub async fn testnet(
    deployment: &Path,
    run: Option<String>,
    csv: Option<PathBuf>,
) -> Result<Summary> {
    let text = std::fs::read_to_string(deployment)
        .with_context(|| format!("reading {}", deployment.display()))?;
    let summary: Value = serde_json::from_str(&text)?;
    let name = |key: &str| -> Result<AccountId> {
        account(summary["accounts"][key].as_str().with_context(|| format!("no accounts.{key}"))?)
    };
    let network = match std::env::var("NEAR_RPC_URL") {
        Ok(url) => NetworkConfig::from_rpc_url("testnet", url.parse()?),
        Err(_) => NetworkConfig::testnet(),
    };
    let agent_id = name("agent")?;
    let council_id = name("council")?;
    let key = keys::load(&agent_id, "testnet", None).await?;
    let target = Target { network, guard: name("guard")? };
    let agent = Agent::connect(target, agent_id, key).await?;

    let council_file = keys::key_file(&keys::credentials_dir("testnet")?, &council_id);
    let council = if council_file.exists() {
        let signer = Signer::from_access_keyfile(council_file.clone())
            .with_context(|| format!("reading {}", council_file.display()))?;
        Some(Council { id: council_id, signer })
    } else {
        println!("No council key at {}, the council will not vote.", council_file.display());
        None
    };
    let run = Some(run.unwrap_or_else(run_tag));
    let demo = Demo { agent, council, invoices: invoices_dir(), run, csv };
    demo.run().await
}
