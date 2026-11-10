//! What happened to one invoice, for a person reading the terminal and for a bookkeeper's CSV.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use near_api::AccountId;

use crate::invoice::Token;

/// The state an invoice ends in after the agent's request (README, "What happens to an
/// invoice").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Paid by the guard from its budget.
    Paid,
    /// Became a DAO proposal. Nothing is paid until the council approves.
    Escalated,
    /// The guard refused the request, nothing paid and nothing filed.
    Rejected,
    /// The payout or the proposal failed and the guard rolled it back. Can be retried.
    Failed,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Paid => "paid",
            Self::Escalated => "waiting_for_vote",
            Self::Rejected => "rejected_by_guard",
            Self::Failed => "failed_can_retry",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Report {
    pub invoice_number: String,
    pub request_id: String,
    pub recipient: AccountId,
    pub token: Token,
    pub amount: u128,
    pub status: Status,
    pub tx_hash: String,
    pub tx_link: Option<String>,
    /// Escalation reason, for example `EXCEEDS_PER_PAYMENT_CAP`.
    pub reason: Option<String>,
    pub proposal_id: Option<u64>,
    /// The proposal's description, read back from the DAO.
    pub description: Option<String>,
    /// The guard's error code, for example `ERR_DUPLICATE_OR_INVALID_REQUEST_ID`.
    pub error: Option<String>,
    /// Token spending of the current period after this request, and the cap.
    pub spent_in_period: Option<u128>,
    pub max_per_period: Option<u128>,
    /// Unix seconds when the guard's period ends and its counters reset.
    pub period_ends: Option<u64>,
}

impl Report {
    /// The lines the agent prints for this invoice.
    pub fn lines(&self, dao: &AccountId) -> Vec<String> {
        let mut lines = vec![
            format!("  invoice      {}", self.invoice_number),
            format!("  request_id   {}", self.request_id),
            format!("  asks         {} to {}", self.token.format(self.amount), self.recipient),
        ];
        let result = match self.status {
            Status::Paid => "PAID by the guard from its budget".to_string(),
            Status::Escalated => "ESCALATED to a DAO vote, nothing paid yet".to_string(),
            Status::Rejected => format!(
                "REJECTED by the guard, nothing paid or filed: {}",
                self.error.as_deref().unwrap_or("unknown error")
            ),
            Status::Failed => format!(
                "FAILED and rolled back, the invoice can be sent again: {}",
                self.error.as_deref().unwrap_or("see the transaction")
            ),
        };
        lines.push(format!("  result       {result}"));
        lines.push(format!("  transaction  {}", self.tx_link.as_deref().unwrap_or(&self.tx_hash)));
        if let Some(reason) = &self.reason {
            lines.push(format!("  reason       {reason}"));
        }
        if let Some(id) = self.proposal_id {
            lines.push(format!("  proposal     {id} on {dao}"));
        }
        if let Some(description) = &self.description {
            lines.push(format!("  description  {description}"));
        }
        if let (Some(spent), Some(max)) = (self.spent_in_period, self.max_per_period) {
            let left = max.saturating_sub(spent);
            let resets =
                self.period_ends.map(|t| format!(", resets {}", utc(t))).unwrap_or_default();
            lines.push(format!(
                "  period       {} of {} spent, {} left{resets}",
                crate::invoice::format_units(spent, self.token.decimals),
                self.token.format(max),
                crate::invoice::format_units(left, self.token.decimals),
            ));
        }
        lines
    }
}

pub const CSV_HEADER: &str = "time_utc,invoice,request_id,status,recipient,token,amount,currency,\
reason,proposal_id,error,transaction,link";

/// Appends one row per invoice, with a header when the file is new.
pub fn append_csv(path: &Path, report: &Report, now_sec: u64) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let new = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if new {
        writeln!(file, "{CSV_HEADER}")?;
    }
    writeln!(file, "{}", csv_row(report, now_sec))?;
    Ok(())
}

pub fn csv_row(r: &Report, now_sec: u64) -> String {
    let fields = [
        utc(now_sec),
        r.invoice_number.clone(),
        r.request_id.clone(),
        r.status.as_str().to_string(),
        r.recipient.to_string(),
        r.token.name().to_string(),
        crate::invoice::format_units(r.amount, r.token.decimals),
        r.token.symbol.clone(),
        r.reason.clone().unwrap_or_default(),
        r.proposal_id.map(|id| id.to_string()).unwrap_or_default(),
        r.error.clone().unwrap_or_default(),
        r.tx_hash.clone(),
        r.tx_link.clone().unwrap_or_default(),
    ];
    fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",")
}

/// RFC 4180 quoting. A leading `=`, `+`, `-` or `@` gets a `'` so a spreadsheet does not run
/// invoice text as a formula.
pub fn csv_field(value: &str) -> String {
    let value = if value.starts_with(['=', '+', '-', '@']) {
        format!("'{value}")
    } else {
        value.to_string()
    };
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
    }
}

/// Unix seconds as `2026-11-10 14:01:38 UTC`.
pub fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        // The testnet guard's period anchor.
        assert_eq!(utc(1_793_973_698), "2026-11-06 14:01:38 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
    }

    #[test]
    fn quotes_csv_fields() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("=HYPERLINK(1)"), "'=HYPERLINK(1)");
        assert_eq!(csv_field("-1"), "'-1");
    }

    #[test]
    fn writes_a_row_per_invoice() {
        let report = Report {
            invoice_number: "NH-1".into(),
            request_id: "inv-ab".into(),
            recipient: "vendor.testnet".parse().unwrap(),
            token: Token {
                id: Some("usdc.testnet".parse().unwrap()),
                symbol: "USDC".into(),
                decimals: 6,
            },
            amount: 25_000_000,
            status: Status::Paid,
            tx_hash: "HASH".into(),
            tx_link: None,
            reason: None,
            proposal_id: None,
            description: None,
            error: None,
            spent_in_period: Some(25_000_000),
            max_per_period: Some(300_000_000),
            period_ends: None,
        };
        assert_eq!(
            csv_row(&report, 0),
            "1970-01-01 00:00:00 UTC,NH-1,inv-ab,paid,vendor.testnet,usdc.testnet,25.00,USDC,,,,HASH,"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out/payments.csv");
        append_csv(&path, &report, 0).unwrap();
        append_csv(&path, &report, 0).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert_eq!(text.lines().next(), Some(CSV_HEADER));
        let lines = report.lines(&"dao.testnet".parse().unwrap());
        assert!(lines.iter().any(|l| l.contains("25.00 of 300.00 USDC spent, 275.00 left")));
    }
}
