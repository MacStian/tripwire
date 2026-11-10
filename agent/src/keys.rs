//! The agent's key: where it comes from, and the check that it can do no more than ask the
//! guard to pay.
//!
//! The secret never goes on the command line, where it would land in shell history and in the
//! process list. It comes from `TRIPWIRE_AGENT_KEY` or from a key file in the near CLI format,
//! by default `~/.near-credentials/<network>/<agent>.json`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use near_api::types::{AccessKeyPermission, SecretKey};
use near_api::{Account, AccountId, NetworkConfig, PublicKey, Signer};

/// Environment variable with the agent's secret key, `ed25519:...`.
pub const KEY_ENV: &str = "TRIPWIRE_AGENT_KEY";
/// Overrides the key directory, as in the near CLI.
pub const CREDENTIALS_ENV: &str = "NEAR_CREDENTIALS_DIR";

/// The method the agent key must be limited to.
pub const METHOD: &str = "request_payment";

pub struct AgentKey {
    pub signer: Arc<Signer>,
    pub public_key: PublicKey,
    /// Where the key came from, for the log. Never the secret.
    pub source: String,
}

/// `$NEAR_CREDENTIALS_DIR`, or `~/.near-credentials/<network>`.
pub fn credentials_dir(network_name: &str) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CREDENTIALS_ENV) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".near-credentials").join(network_name))
}

/// The key file of `account` in `dir`, as the near CLI legacy keychain names it.
pub fn key_file(dir: &Path, account: &AccountId) -> PathBuf {
    dir.join(format!("{account}.json"))
}

/// Loads the agent key from `key_file_arg` when given, else from `TRIPWIRE_AGENT_KEY`, else
/// from the keychain of `network_name`.
pub async fn load(
    agent: &AccountId,
    network_name: &str,
    key_file_arg: Option<&Path>,
) -> Result<AgentKey> {
    let env_secret = std::env::var(KEY_ENV).ok().filter(|_| key_file_arg.is_none());
    let (signer, source) = if let Some(secret) = env_secret {
        let secret: SecretKey =
            secret.trim().parse().with_context(|| format!("{KEY_ENV} is not a secret key"))?;
        (Signer::from_secret_key(secret)?, format!("environment variable {KEY_ENV}"))
    } else {
        let path = match key_file_arg {
            Some(path) => path.to_path_buf(),
            None => key_file(&credentials_dir(network_name)?, agent),
        };
        ensure!(
            path.exists(),
            "no agent key: set {KEY_ENV} or put the key file at {}",
            path.display()
        );
        warn_if_readable_by_others(&path);
        let signer = Signer::from_access_keyfile(path.clone())
            .with_context(|| format!("reading the key file {}", path.display()))?;
        (signer, format!("key file {}", path.display()))
    };
    let public_key = signer.get_public_key().await?;
    Ok(AgentKey { signer, public_key, source })
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            eprintln!(
                "warning: {} can be read by other users (mode {:o}), run chmod 600 on it",
                path.display(),
                mode & 0o777
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &Path) {}

/// What the chain says the key may do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyScope {
    pub receiver: AccountId,
    pub methods: Vec<String>,
    /// Gas allowance left, in yoctoNEAR. `None` is unlimited.
    pub allowance: Option<u128>,
}

/// Reads the key's permission from the chain and refuses to go on unless it is a function call
/// key for `request_payment` on `guard` and nothing else. A full access key, or one that
/// reaches other methods or contracts, is an error: the agent reads untrusted invoices and must
/// never hold more than the right to ask.
pub async fn verify_scope(
    network: &NetworkConfig,
    agent: &AccountId,
    public_key: PublicKey,
    guard: &AccountId,
) -> Result<KeyScope> {
    let view = Account(agent.clone())
        .access_key(public_key)
        .fetch_from(network)
        .await
        .with_context(|| format!("the key {public_key} is not on {agent}"))?;
    match view.data.permission {
        AccessKeyPermission::FullAccess => bail!(
            "refusing to run: {public_key} is a full access key of {agent}. The agent only runs \
             with a function call key for {METHOD} on {guard}"
        ),
        AccessKeyPermission::FunctionCall(permission) => {
            ensure!(
                permission.receiver_id == guard.as_str(),
                "refusing to run: the key calls {}, not the guard {guard}",
                permission.receiver_id
            );
            ensure!(
                permission.method_names == [METHOD],
                "refusing to run: the key allows the methods {:?}, it must allow only {METHOD}",
                permission.method_names
            );
            Ok(KeyScope {
                receiver: guard.clone(),
                methods: permission.method_names,
                allowance: permission.allowance.map(|a| a.as_yoctonear()),
            })
        }
    }
}
