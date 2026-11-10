//! Tripwire invoice agent (E3).
//!
//! Reads an invoice, turns it into a `request_payment` call and signs it with a NEAR function
//! call key that reaches only that method on the guard. The agent decides nothing about
//! limits. The guard pays, files a DAO proposal or refuses, and the agent reports which.
//!
//! - [`invoice`]: the deterministic extractor, amounts and the `request_id` (D14).
//! - [`llm`]: the optional model extractor.
//! - [`keys`]: where the key comes from and the check that it is scoped to the guard.
//! - [`guard`]: the calls to the guard and the DAO.
//! - [`report`]: terminal lines and CSV rows.
//! - [`demo`]: the three invoice scenarios with the council's votes.

pub mod demo;
pub mod guard;
pub mod invoice;
pub mod keys;
pub mod llm;
pub mod report;

use anyhow::{bail, Result};
use near_api::AccountId;
use serde_json::Value;

use crate::guard::{GuardInfo, Target};
use crate::invoice::PaymentRequest;
use crate::keys::{AgentKey, KeyScope};
use crate::report::{Report, Status};

/// An agent account with a checked key, connected to one guard.
pub struct Agent {
    pub target: Target,
    pub id: AccountId,
    pub key: AgentKey,
    pub scope: KeyScope,
    pub info: GuardInfo,
}

impl Agent {
    /// Checks on chain that the key is a function call key for `request_payment` on the guard
    /// only, and reads the guard's DAO and tokens.
    pub async fn connect(target: Target, id: AccountId, key: AgentKey) -> Result<Self> {
        let scope = keys::verify_scope(&target.network, &id, key.public_key, &target.guard).await?;
        let info = target.info().await?;
        Ok(Self { target, id, key, scope, info })
    }

    /// Sends the request and reads back what the guard did.
    pub async fn pay(&self, request: &PaymentRequest) -> Result<Report> {
        let sent = self.target.request_payment(&self.id, self.key.signer.clone(), request).await?;
        let event =
            |name: &str| sent.events.iter().find(|e| e["event"] == name).map(|e| e["data"].clone());
        let mut report = Report {
            invoice_number: request.invoice_number.clone(),
            request_id: request.request_id.clone(),
            recipient: request.recipient.clone(),
            token: request.token.clone(),
            amount: request.amount,
            status: Status::Rejected,
            tx_link: self.target.tx_link(&sent.tx_hash),
            tx_hash: sent.tx_hash.clone(),
            reason: None,
            proposal_id: None,
            description: None,
            error: sent.panic.clone(),
            spent_in_period: None,
            max_per_period: None,
            period_ends: None,
        };
        let outcome = sent.value.as_ref().and_then(Value::as_object);
        let kind = outcome.and_then(|o| o.keys().next().cloned());
        match kind.as_deref() {
            Some("Paid") => {
                report.status = Status::Paid;
                report.error = None;
                let token = self.target.token_status(&request.token).await?;
                report.spent_in_period =
                    token["spent_in_period"].as_str().and_then(|s| s.parse().ok());
                report.max_per_period =
                    token["max_per_period"].as_str().and_then(|s| s.parse().ok());
                let status = self.target.status().await?;
                report.period_ends = status["request_usage"]["period_start"]
                    .as_u64()
                    .zip(status["policy"]["period_seconds"].as_u64())
                    .map(|(start, len)| start + len);
            }
            Some("Escalated") => {
                report.status = Status::Escalated;
                report.error = None;
                let data = event("payment_escalated").unwrap_or_default();
                report.reason = data["reason"].as_str().map(str::to_string);
                report.proposal_id =
                    sent.value.as_ref().and_then(|v| v["Escalated"]["proposal_id"].as_u64());
                if let Some(id) = report.proposal_id {
                    let proposal = self.target.proposal(&self.info.dao, id).await?;
                    report.description = proposal["description"].as_str().map(str::to_string);
                }
            }
            Some("PaymentFailed") | Some("EscalationFailed") => {
                report.status = Status::Failed;
                let data = event("escalation_failed").unwrap_or_default();
                report.reason = data["reason"].as_str().map(str::to_string);
                if report.error.is_none() {
                    report.error = kind.clone();
                }
            }
            _ if sent.panic.is_some() => report.status = Status::Rejected,
            _ => bail!(
                "unexpected result of transaction {}: {:?} {:?}",
                sent.tx_hash,
                sent.value,
                sent.failure
            ),
        }
        Ok(report)
    }
}
