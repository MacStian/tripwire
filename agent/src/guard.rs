//! Talking to the guard: what it allows, sending `request_payment` and reading what happened.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use near_api::types::transaction::result::ExecutionFinalResult;
use near_api::{AccountId, Contract, NearGas, NetworkConfig, Signer};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::invoice::{PaymentRequest, Token};

/// Gas the agent attaches. The guard needs at least 50 TGas (D17), unused gas goes back to the
/// key's allowance.
pub const REQUEST_GAS: NearGas = NearGas::from_tgas(100);
/// Gas for a council vote that executes a proposal.
pub const VOTE_GAS: NearGas = NearGas::from_tgas(300);

/// A network and where to find the guard on it.
#[derive(Clone)]
pub struct Target {
    pub network: NetworkConfig,
    pub guard: AccountId,
}

impl Target {
    /// `https://testnet.nearblocks.io/txns/<hash>` on testnet, `None` elsewhere.
    pub fn tx_link(&self, hash: &str) -> Option<String> {
        (self.network.network_name == "testnet")
            .then(|| format!("https://testnet.nearblocks.io/txns/{hash}"))
    }

    pub async fn view<T: DeserializeOwned + Send + Sync>(
        &self,
        contract: &AccountId,
        method: &str,
        args: Value,
    ) -> Result<T> {
        let data = Contract(contract.clone())
            .call_function(method, args)
            .read_only::<T>()
            .fetch_from(&self.network)
            .await
            .with_context(|| format!("view {contract}.{method}"))?;
        Ok(data.data)
    }

    /// The guard's `get_status`: policy, budgets, this period's usage.
    pub async fn status(&self) -> Result<Value> {
        self.view(&self.guard, "get_status", json!({})).await
    }

    /// The DAO the guard escalates to and the tokens in its policy, with symbol and decimals
    /// from each token's `ft_metadata`.
    pub async fn info(&self) -> Result<GuardInfo> {
        let status = self.status().await?;
        let dao: AccountId = serde_json::from_value(status["dao_id"].clone())?;
        let mut tokens = Vec::new();
        for limit in status["policy"]["token_limits"].as_array().context("no token_limits")? {
            match limit["token_id"].as_str() {
                None => tokens.push(Token::near()),
                Some(id) => {
                    let id: AccountId = id.parse()?;
                    let meta: Value = self.view(&id, "ft_metadata", json!({})).await?;
                    tokens.push(Token {
                        symbol: meta["symbol"].as_str().context("no symbol")?.to_string(),
                        decimals: meta["decimals"].as_u64().context("no decimals")?.try_into()?,
                        id: Some(id),
                    });
                }
            }
        }
        Ok(GuardInfo { dao, tokens, status })
    }

    /// `get_token_status` for one token.
    pub async fn token_status(&self, token: &Token) -> Result<Value> {
        self.view(&self.guard, "get_token_status", json!({ "token_id": token.id })).await
    }

    pub async fn proposal(&self, dao: &AccountId, id: u64) -> Result<Value> {
        self.view(dao, "get_proposal", json!({ "id": id })).await
    }

    /// Signs `request_payment` with the agent key and reads the outcome. Returns an error only
    /// when the transaction did not run, for example when the runtime refused the key.
    pub async fn request_payment(
        &self,
        agent: &AccountId,
        signer: Arc<Signer>,
        request: &PaymentRequest,
    ) -> Result<Sent> {
        let args = json!({
            "request_id": request.request_id,
            "recipient": request.recipient,
            "token_id": request.token.id,
            "amount": request.amount.to_string(),
            "memo": request.memo,
        });
        let result = Contract(self.guard.clone())
            .call_function("request_payment", args)
            .transaction()
            .gas(REQUEST_GAS)
            .with_signer(agent.clone(), signer)
            .send_to(&self.network)
            .await
            .context("sending request_payment")?;
        let outcome =
            result.into_full().ok_or_else(|| anyhow!("the transaction is still pending"))?;
        Ok(Sent::read(&outcome))
    }
}

pub struct GuardInfo {
    pub dao: AccountId,
    pub tokens: Vec<Token>,
    /// `get_status` at the time of the call.
    pub status: Value,
}

/// A transaction that ran, read without assuming it succeeded.
#[derive(Clone, Debug)]
pub struct Sent {
    pub tx_hash: String,
    /// The contract panic of the first failed receipt, for example `ERR_GUARD_FROZEN`.
    pub panic: Option<String>,
    /// Any other failure, as the RPC reported it.
    pub failure: Option<String>,
    /// The return value, for `request_payment` the guard's `PaymentOutcome`.
    pub value: Option<Value>,
    /// The guard's NEP-297 events (`standard` = `tripwire`), in execution order.
    pub events: Vec<Value>,
}

impl Sent {
    pub fn read(outcome: &ExecutionFinalResult) -> Self {
        let tx_hash = outcome.outcome().transaction_hash.to_string();
        let events = outcome
            .logs()
            .into_iter()
            .filter_map(|log| log.strip_prefix("EVENT_JSON:"))
            .filter_map(|json| serde_json::from_str::<Value>(json).ok())
            .filter(|event| event["standard"] == "tripwire")
            .collect();
        // A transaction can succeed while one of its receipts fails, for example a payout the
        // guard rolled back. The value is read whenever the transaction itself succeeded.
        let failures = outcome.failures();
        let failure = (!failures.is_empty()).then(|| format!("{failures:?}"));
        let panic = failure.as_deref().and_then(panic_message);
        let value = if outcome.is_failure() { None } else { outcome.clone().json::<Value>().ok() };
        Self { tx_hash, panic, failure, value, events }
    }

    /// No receipt failed.
    pub fn ok(&self) -> bool {
        self.failure.is_none()
    }
}

/// The message after `Smart contract panicked: ` in a debug-printed failure.
pub fn panic_message(failures: &str) -> Option<String> {
    const PREFIX: &str = "Smart contract panicked: ";
    let start = failures.find(PREFIX)? + PREFIX.len();
    let rest = &failures[start..];
    let end = rest.find(['"', '\\']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Signs a council vote on a DAO proposal. Sputnik 3.2 wants the proposal kind with the vote.
pub async fn vote(
    target: &Target,
    dao: &AccountId,
    council: &AccountId,
    signer: Arc<Signer>,
    proposal_id: u64,
    approve: bool,
) -> Result<Sent> {
    let kind = target.proposal(dao, proposal_id).await?["kind"].clone();
    let action = if approve { "VoteApprove" } else { "VoteReject" };
    let args = json!({ "id": proposal_id, "action": action, "proposal": kind });
    let result = Contract(dao.clone())
        .call_function("act_proposal", args)
        .transaction()
        .gas(VOTE_GAS)
        .with_signer(council.clone(), signer)
        .send_to(&target.network)
        .await
        .with_context(|| format!("sending the vote on proposal {proposal_id}"))?;
    let outcome = result.into_full().ok_or_else(|| anyhow!("the vote is still pending"))?;
    Ok(Sent::read(&outcome))
}

#[cfg(test)]
mod tests {
    use super::panic_message;

    #[test]
    fn reads_the_panic_message() {
        let text = r#"[ExecutionOutcome { status: Failure(ActionError(ActionError { index: Some(0), kind: FunctionCallError(ExecutionError("Smart contract panicked: ERR_GUARD_FROZEN")) })) }]"#;
        assert_eq!(panic_message(text).as_deref(), Some("ERR_GUARD_FROZEN"));
        assert_eq!(panic_message("no panic here"), None);
    }
}
