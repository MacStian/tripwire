//! E4-S2: the three invoices end to end, with the council's votes.
//!
//! 1. A routine 25 USDC invoice: the guard pays at once. The same invoice sent again is
//!    refused, so it is never paid twice.
//! 2. A 450 USDC invoice, over the 100 USDC per-payment cap: the guard files a Transfer
//!    proposal, the council approves and the DAO pays from its treasury.
//! 3. A poisoned invoice that tells the agent to pay another account: the agent follows it,
//!    the guard pays nothing and files a proposal with the real recipient and a cleaned note,
//!    and the council rejects it.
//!
//! Every step checks what it expects and stops the demo when the chain says otherwise.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use near_api::{AccountId, Signer};
use serde_json::{json, Value};

use crate::guard::vote;
use crate::invoice::{self, PaymentRequest};
use crate::report::{self, Report, Status};
use crate::Agent;

pub const ROUTINE: &str = "01-small-routine.md";
pub const OVER_CAP: &str = "02-large-over-cap.md";
pub const POISONED: &str = "03-poisoned-injection.md";

/// A council member's key, for the votes. The agent never holds it.
pub struct Council {
    pub id: AccountId,
    pub signer: Arc<Signer>,
}

pub struct Demo {
    pub agent: Agent,
    /// Without it the proposals stay open and the demo says so.
    pub council: Option<Council>,
    /// The folder with the three invoice files.
    pub invoices: PathBuf,
    /// Appended to every invoice number, so a run on testnet sends new invoices.
    pub run: Option<String>,
    pub csv: Option<PathBuf>,
}

/// What the demo saw, for the tests.
#[derive(Debug)]
pub struct Summary {
    pub reports: Vec<Report>,
    /// Proposal id and its final status after the vote.
    pub votes: Vec<(u64, String)>,
}

impl Demo {
    pub async fn run(&self) -> Result<Summary> {
        self.header().await?;
        let mut reports = Vec::new();
        let mut votes = Vec::new();

        step("1. A routine invoice inside the policy");
        let (request, _) = self.read(ROUTINE)?;
        let paid = self.send(&request, Status::Paid, None).await?;
        reports.push(paid);

        step("1b. The same invoice again");
        reports.push(
            self.send(&request, Status::Rejected, Some("ERR_DUPLICATE_OR_INVALID_REQUEST_ID"))
                .await?,
        );
        say("The guard refuses a request_id it has seen, so an invoice is never paid twice.");

        step("2. An invoice over the 100 USDC per-payment cap");
        let (request, _) = self.read(OVER_CAP)?;
        let escalated =
            self.send(&request, Status::Escalated, Some("EXCEEDS_PER_PAYMENT_CAP")).await?;
        let id = escalated.proposal_id.context("the escalation has no proposal id")?;
        reports.push(escalated);
        if let Some(status) = self.vote(id, true, &request).await? {
            votes.push((id, status));
        }

        step("3. A poisoned invoice that swaps the recipient");
        let (request, fields) = self.read(POISONED)?;
        say(&format!(
            "The invoice hides a second `Pay to:` line. The agent follows it and asks to pay {}.",
            fields.pay_to
        ));
        let escalated =
            self.send(&request, Status::Escalated, Some("RECIPIENT_NOT_ALLOWLISTED")).await?;
        let id = escalated.proposal_id.context("the escalation has no proposal id")?;
        let description = escalated.description.clone().unwrap_or_default();
        ensure!(
            description.contains(&format!("receiver={}", request.recipient))
                && !["<b>", "<script", "{", "\u{2705}"].iter().any(|s| description.contains(s)),
            "the proposal description does not show what the guard should write: {description}"
        );
        reports.push(escalated);
        say("Nothing was paid. The proposal names the real recipient. The note keeps only letters, digits and basic punctuation, so the invoice's markup is gone and its claims are plain words next to the facts.");
        if let Some(status) = self.vote(id, false, &request).await? {
            votes.push((id, status));
        }

        let attacker_balance = self.ft_balance(&request).await?;
        ensure!(
            attacker_balance == 0,
            "{} holds {attacker_balance} of the token",
            request.recipient
        );

        step("Summary");
        for r in &reports {
            let vote = r
                .proposal_id
                .and_then(|id| votes.iter().find(|(v, _)| *v == id))
                .map(|(id, status)| format!(", proposal {id} {status}"))
                .unwrap_or_default();
            let detail = r.reason.as_deref().or(r.error.as_deref()).unwrap_or("");
            let line = format!(
                "  {:<14} {:>12}  {:<18} {detail}{vote}",
                r.invoice_number,
                r.token.format(r.amount),
                r.status.as_str(),
            );
            println!("{}", line.trim_end());
        }
        if let Some(path) = &self.csv {
            println!("  CSV: {}", path.display());
        }
        Ok(Summary { reports, votes })
    }

    async fn header(&self) -> Result<()> {
        let a = &self.agent;
        let status = &a.info.status;
        println!("Tripwire demo on {}", a.target.network.network_name);
        println!("  guard     {}", a.target.guard);
        println!("  dao       {}", a.info.dao);
        println!("  agent     {}, key from {}", a.id, a.key.source);
        let allowance = a
            .scope
            .allowance
            .map(|y| format!(", {} NEAR gas allowance left", invoice::format_units(y, 24)))
            .unwrap_or_default();
        println!(
            "  key       function call key, only {:?} on {}{allowance}",
            a.scope.methods, a.scope.receiver
        );
        let usage = &status["request_usage"];
        let policy = &status["policy"];
        let count = |v: &Value| v.as_u64().unwrap_or(0);
        let payments =
            (count(&usage["payments_in_period"]), count(&policy["max_payments_per_period"]));
        let escalations =
            (count(&usage["escalations_in_period"]), count(&policy["max_escalations_per_period"]));
        let period_end = count(&usage["period_start"]) + count(&policy["period_seconds"]);
        println!(
            "  period    {} of {} payouts and {} of {} escalations used, resets {}",
            payments.0,
            payments.1,
            escalations.0,
            escalations.1,
            report::utc(period_end)
        );
        ensure!(status["frozen"] == false, "the guard is frozen, only a DAO proposal unfreezes it");
        ensure!(
            payments.0 < payments.1 && escalations.0 + 2 <= escalations.1,
            "this period has room for {} payouts and {} escalations, the demo needs 1 and 2. \
             Run it again after {}",
            payments.1.saturating_sub(payments.0),
            escalations.1.saturating_sub(escalations.0),
            report::utc(period_end)
        );
        Ok(())
    }

    fn read(&self, file: &str) -> Result<(PaymentRequest, invoice::InvoiceFields)> {
        let path = self.invoices.join(file);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let fields = invoice::extract(&text)?;
        let request =
            PaymentRequest::from_fields(&fields, &self.agent.info.tokens, self.run.as_deref())?;
        println!("  file         {}", display(&path));
        println!(
            "  read         pay {} {} to {}, memo {:?}",
            fields.amount,
            fields.currency,
            fields.pay_to,
            fields.memo.as_deref().unwrap_or("")
        );
        Ok((request, fields))
    }

    /// Sends the request and fails unless the guard did what the step expects.
    async fn send(
        &self,
        request: &PaymentRequest,
        expected: Status,
        code: Option<&str>,
    ) -> Result<Report> {
        let report = self.agent.pay(request).await?;
        for line in report.lines(&self.agent.info.dao) {
            println!("{line}");
        }
        if let Some(path) = &self.csv {
            report::append_csv(path, &report, now_sec())?;
        }
        let got = report.reason.as_deref().or(report.error.as_deref());
        if report.status != expected || (code.is_some() && got != code) {
            bail!(
                "expected {} {}, got {} {}",
                expected.as_str(),
                code.unwrap_or(""),
                report.status.as_str(),
                got.unwrap_or("")
            );
        }
        Ok(report)
    }

    /// The council votes. Returns the proposal's status after the vote, `None` without a
    /// council key.
    async fn vote(
        &self,
        id: u64,
        approve: bool,
        request: &PaymentRequest,
    ) -> Result<Option<String>> {
        let a = &self.agent;
        let Some(council) = &self.council else {
            say(&format!("No council key, so proposal {id} stays open for a vote."));
            return Ok(None);
        };
        let before = self.ft_balance(request).await?;
        let sent =
            vote(&a.target, &a.info.dao, &council.id, council.signer.clone(), id, approve).await?;
        ensure!(sent.ok(), "the vote on proposal {id} failed: {:?}", sent.failure);
        let proposal = a.target.proposal(&a.info.dao, id).await?;
        let status = proposal["status"].as_str().unwrap_or("?").to_string();
        let after = self.ft_balance(request).await?;
        let link = a.target.tx_link(&sent.tx_hash).unwrap_or(sent.tx_hash.clone());
        let verb = if approve { "approves" } else { "rejects" };
        println!("  council      {} {verb} proposal {id}: {link}", council.id);
        println!("  proposal     {status}");
        if approve {
            ensure!(status == "Approved", "proposal {id} is {status} after the approval");
            ensure!(
                after - before == request.amount,
                "the recipient got {} instead of {}",
                after - before,
                request.amount
            );
            say(&format!(
                "The DAO paid {} to {} from its treasury, not from the guard's budget.",
                request.token.format(request.amount),
                request.recipient
            ));
        } else {
            ensure!(status == "Rejected", "proposal {id} is {status} after the rejection");
            ensure!(after == before, "{} was paid", request.recipient);
            say(&format!("Nothing was paid to {}.", request.recipient));
        }
        Ok(Some(status))
    }

    async fn ft_balance(&self, request: &PaymentRequest) -> Result<u128> {
        let Some(token) = &request.token.id else {
            bail!("the demo invoices are in a fungible token");
        };
        let balance: String = self
            .agent
            .target
            .view(token, "ft_balance_of", json!({ "account_id": request.recipient }))
            .await?;
        Ok(balance.parse()?)
    }
}

fn step(title: &str) {
    println!("\n{title}");
}

fn say(text: &str) {
    println!("  -> {text}");
}

/// The path relative to the working directory when it is under it.
fn display(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(|p| p.display().to_string()))
        .unwrap_or_else(|| path.display().to_string())
}

pub fn now_sec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A run tag from the clock, for example `r1110t1530`, so each testnet run sends new invoices.
pub fn run_tag() -> String {
    let utc = report::utc(now_sec());
    // "2026-11-10 15:30:12 UTC" -> "r1110t153012"
    let digits: String = utc.chars().filter(char::is_ascii_digit).collect();
    format!("r{}t{}", &digits[4..8], &digits[8..14])
}
