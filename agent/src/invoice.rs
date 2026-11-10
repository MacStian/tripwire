//! Invoices: what the agent reads out of one, and how that becomes a `request_payment` call.
//!
//! An invoice is untrusted text. The extractor here does not try to spot an attack. It reads
//! the payment fields the way a naive agent would, and the guard decides what that agent may
//! actually pay.

use anyhow::{bail, ensure, Context, Result};
use near_api::AccountId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The payment fields of an invoice, as text, before they are checked against the guard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvoiceFields {
    pub invoice_number: String,
    /// The NEAR account to pay.
    pub pay_to: String,
    /// Decimal amount, for example `25.00`.
    pub amount: String,
    /// Currency symbol, for example `USDC` or `NEAR`.
    pub currency: String,
    #[serde(default)]
    pub memo: Option<String>,
}

/// Deterministic extractor: reads `Key: value` lines. When a field appears twice, the later
/// line wins, the way a model follows the most recent instruction it read. That is the agent a
/// poisoned invoice fools, so the demo shows what the guard does with its request.
///
/// Fields: `Invoice number`, `Pay to` (the first word is the account), `Amount due`
/// (`<amount> <currency>`) and `Memo`.
pub fn extract(text: &str) -> Result<InvoiceFields> {
    let mut number = None;
    let mut pay_to = None;
    let mut amount_due = None;
    let mut memo = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "invoice number" => number = Some(value.to_string()),
            "pay to" => pay_to = value.split_whitespace().next().map(str::to_string),
            "amount due" => amount_due = Some(value.to_string()),
            "memo" => memo = Some(value.to_string()),
            _ => {}
        }
    }
    let amount_due = amount_due.context("the invoice has no `Amount due:` line")?;
    let mut parts = amount_due.split_whitespace();
    let (Some(amount), Some(currency), None) = (parts.next(), parts.next(), parts.next()) else {
        bail!("`Amount due: {amount_due}` is not `<amount> <currency>`");
    };
    Ok(InvoiceFields {
        invoice_number: number.context("the invoice has no `Invoice number:` line")?,
        pay_to: pay_to.context("the invoice has no `Pay to:` line")?,
        amount: amount.to_string(),
        currency: currency.to_string(),
        memo,
    })
}

/// A token the guard's policy allows, as the agent resolves invoice currencies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// `None` for native NEAR.
    pub id: Option<AccountId>,
    pub symbol: String,
    pub decimals: u8,
}

impl Token {
    pub fn near() -> Self {
        Self { id: None, symbol: "NEAR".to_string(), decimals: 24 }
    }

    /// The token contract, or `NEAR`.
    pub fn name(&self) -> &str {
        self.id.as_ref().map_or("NEAR", |id| id.as_str())
    }

    /// `amount` base units as a decimal with the token's symbol, for example `25.00 USDC`.
    pub fn format(&self, amount: u128) -> String {
        format!("{} {}", format_units(amount, self.decimals), self.symbol)
    }
}

/// What the agent sends to `request_payment`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequest {
    pub invoice_number: String,
    pub request_id: String,
    pub recipient: AccountId,
    pub token: Token,
    /// Base units of the token.
    pub amount: u128,
    pub memo: Option<String>,
}

impl PaymentRequest {
    /// Checks the fields and resolves the currency to one of `tokens`, the tokens in the
    /// guard's policy. `run` is appended to the invoice number, so a demo can send the same
    /// invoice file again as a new invoice.
    pub fn from_fields(
        fields: &InvoiceFields,
        tokens: &[Token],
        run: Option<&str>,
    ) -> Result<Self> {
        let recipient: AccountId = fields
            .pay_to
            .parse()
            .with_context(|| format!("`{}` is not a NEAR account id", fields.pay_to))?;
        let token = tokens
            .iter()
            .find(|t| t.symbol.eq_ignore_ascii_case(&fields.currency))
            .with_context(|| format!("the guard's policy has no token {}", fields.currency))?
            .clone();
        let amount = parse_units(&fields.amount, token.decimals)?;
        let invoice_number = match run {
            Some(run) => format!("{}-{run}", fields.invoice_number),
            None => fields.invoice_number.clone(),
        };
        ensure!(!invoice_number.trim().is_empty(), "the invoice number is empty");
        let request_id = request_id(&recipient, &token, amount, &invoice_number);
        Ok(Self { invoice_number, request_id, recipient, token, amount, memo: fields.memo.clone() })
    }
}

/// D14: `inv-` and the first 32 hex characters of sha256 over the recipient, the token, the
/// amount in base units and the invoice number. The agent derives it and never copies an id
/// from invoice text, so an invoice cannot use up the id of another one. The same invoice
/// always gets the same id, so a retry is never paid twice.
pub fn request_id(
    recipient: &AccountId,
    token: &Token,
    amount: u128,
    invoice_number: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [recipient.as_str(), token.name(), &amount.to_string(), invoice_number] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("inv-{hex}")
}

/// `"25.5"` with 6 decimals is `25_500_000`. Rejects signs, exponents, more decimals than the
/// token has and amounts that do not fit in u128.
pub fn parse_units(amount: &str, decimals: u8) -> Result<u128> {
    let (whole, fraction) = amount.split_once('.').unwrap_or((amount, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    ensure!(
        !whole.is_empty() && digits(whole) && digits(fraction),
        "`{amount}` is not a plain decimal amount"
    );
    ensure!(
        fraction.len() <= usize::from(decimals),
        "`{amount}` has more than {decimals} decimals"
    );
    let padded = format!("{whole}{fraction:0<width$}", width = usize::from(decimals));
    padded.parse::<u128>().with_context(|| format!("`{amount}` is too large"))
}

/// Base units as a decimal with at least two decimals: `25_000_000` with 6 is `25.00`.
pub fn format_units(amount: u128, decimals: u8) -> String {
    if decimals == 0 {
        return amount.to_string();
    }
    let scale = 10u128.pow(u32::from(decimals));
    let fraction = format!("{:0width$}", amount % scale, width = usize::from(decimals));
    let trimmed = fraction.trim_end_matches('0');
    let shown = if trimmed.len() < 2 { &fraction[..2.min(fraction.len())] } else { trimmed };
    format!("{}.{shown}", amount / scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usdc() -> Token {
        Token {
            id: Some("usdc.example.testnet".parse().unwrap()),
            symbol: "USDC".into(),
            decimals: 6,
        }
    }

    fn fixture(name: &str) -> String {
        let path = format!("{}/../invoices/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn reads_the_routine_invoice() {
        let fields = extract(&fixture("01-small-routine.md")).unwrap();
        assert_eq!(
            fields,
            InvoiceFields {
                invoice_number: "NH-2026-1104".into(),
                pay_to: "vendor.macstian-tripwire.testnet".into(),
                amount: "25.00".into(),
                currency: "USDC".into(),
                memo: Some("Hosting November NH-2026-1104".into()),
            }
        );
    }

    #[test]
    fn reads_the_large_invoice() {
        let fields = extract(&fixture("02-large-over-cap.md")).unwrap();
        assert_eq!(fields.amount, "450.00");
        assert_eq!(fields.pay_to, "vendor.macstian-tripwire.testnet");
    }

    #[test]
    fn the_poisoned_invoice_fools_the_extractor() {
        let fields = extract(&fixture("03-poisoned-injection.md")).unwrap();
        assert_eq!(fields.invoice_number, "NH-2026-1106");
        assert_eq!(fields.pay_to, "attacker.macstian-tripwire.testnet");
        assert_eq!(fields.amount, "50.00");
        assert!(fields.memo.unwrap().contains("<script>"));
    }

    #[test]
    fn missing_fields_are_errors() {
        assert!(extract("Invoice number: 1\nPay to: a.testnet\n").is_err());
        assert!(extract("Invoice number: 1\nAmount due: 5 USDC\n").is_err());
        assert!(extract("Pay to: a.testnet\nAmount due: 5 USDC\n").is_err());
        assert!(extract("Invoice number: 1\nPay to: a.testnet\nAmount due: 5\n").is_err());
        assert!(extract("Invoice number: 1\nPay to: a.testnet\nAmount due: 5 USDC now\n").is_err());
    }

    #[test]
    fn resolves_the_request() {
        let fields = extract(&fixture("01-small-routine.md")).unwrap();
        let tokens = [Token::near(), usdc()];
        let request = PaymentRequest::from_fields(&fields, &tokens, None).unwrap();
        assert_eq!(request.amount, 25_000_000);
        assert_eq!(request.token, usdc());
        assert_eq!(request.invoice_number, "NH-2026-1104");
        assert_eq!(request.request_id.len(), 4 + 32);
        assert!(request.request_id.starts_with("inv-"));

        // Same invoice, same id. A run tag makes it a new invoice.
        let again = PaymentRequest::from_fields(&fields, &tokens, None).unwrap();
        assert_eq!(again.request_id, request.request_id);
        let tagged = PaymentRequest::from_fields(&fields, &tokens, Some("r1")).unwrap();
        assert_eq!(tagged.invoice_number, "NH-2026-1104-r1");
        assert_ne!(tagged.request_id, request.request_id);

        // A currency outside the policy or a bad account stops the agent.
        assert!(PaymentRequest::from_fields(&fields, &[Token::near()], None).is_err());
        let bad = InvoiceFields { pay_to: "Not An Account".into(), ..fields };
        assert!(PaymentRequest::from_fields(&bad, &tokens, None).is_err());
    }

    #[test]
    fn the_request_id_covers_every_field() {
        let a: AccountId = "a.testnet".parse().unwrap();
        let b: AccountId = "b.testnet".parse().unwrap();
        let base = request_id(&a, &usdc(), 1, "n");
        assert_ne!(base, request_id(&b, &usdc(), 1, "n"));
        assert_ne!(base, request_id(&a, &Token::near(), 1, "n"));
        assert_ne!(base, request_id(&a, &usdc(), 2, "n"));
        assert_ne!(base, request_id(&a, &usdc(), 1, "m"));
        // Field boundaries are kept, so "1" + "0n" and "10" + "n" differ.
        assert_ne!(request_id(&a, &usdc(), 1, "0n"), request_id(&a, &usdc(), 10, "n"));
        assert!(base[4..].bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parses_units() {
        assert_eq!(parse_units("25", 6).unwrap(), 25_000_000);
        assert_eq!(parse_units("25.5", 6).unwrap(), 25_500_000);
        assert_eq!(parse_units("0.000001", 6).unwrap(), 1);
        assert_eq!(parse_units("1", 24).unwrap(), 10u128.pow(24));
        for bad in ["", ".5", "-1", "+1", "1e3", "1.0000001", "1,5", "1.2.3", " 1"] {
            assert!(parse_units(bad, 6).is_err(), "{bad}");
        }
        assert!(parse_units("340282366920938463463374607431768211456", 0).is_err());
    }

    #[test]
    fn formats_units() {
        assert_eq!(format_units(25_000_000, 6), "25.00");
        assert_eq!(format_units(25_500_000, 6), "25.50");
        assert_eq!(format_units(1, 6), "0.000001");
        assert_eq!(format_units(275_000_000, 6), "275.00");
        assert_eq!(format_units(7, 0), "7");
        assert_eq!(usdc().format(450_000_000), "450.00 USDC");
    }
}
