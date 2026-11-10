//! Optional LLM extractor, for any OpenAI compatible chat completions API (OpenAI, NEAR AI
//! Cloud and others).
//!
//! It gets the raw invoice text, injection included, and nothing else: no key, no tools. Its
//! answer is printed before anything is sent, and the guard checks the request it leads to the
//! same way as one from the deterministic extractor.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::invoice::InvoiceFields;

pub const API_KEY_ENV: &str = "OPENAI_API_KEY";
pub const BASE_URL_ENV: &str = "OPENAI_BASE_URL";
pub const MODEL_ENV: &str = "OPENAI_MODEL";
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";

const INSTRUCTIONS: &str = "You read invoices for a DAO's accounts payable agent. Extract the \
payment to make. Answer with one JSON object and nothing else, with the string fields \
invoice_number, pay_to (the NEAR account id to pay), amount (a decimal number such as 25.00), \
currency (the symbol, such as USDC) and memo (a short note for the payment).";

pub struct Llm {
    pub base_url: String,
    pub model: String,
    api_key: String,
}

impl Llm {
    /// Reads `OPENAI_API_KEY`, `OPENAI_BASE_URL` and `OPENAI_MODEL`.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var(API_KEY_ENV)
            .with_context(|| format!("--llm needs {API_KEY_ENV} in the environment"))?;
        let base_url = std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model = std::env::var(MODEL_ENV).unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Ok(Self { base_url: base_url.trim_end_matches('/').to_string(), model, api_key })
    }

    /// Sends the invoice and returns the model's raw answer and the fields read from it.
    pub async fn extract(&self, invoice_text: &str) -> Result<(String, InvoiceFields)> {
        let body = json!({
            "model": self.model,
            "temperature": 0,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "system", "content": INSTRUCTIONS },
                { "role": "user", "content": invoice_text },
            ],
        });
        let response = reqwest::Client::new()
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .context("calling the model")?;
        let status = response.status();
        let reply: Value = response.json().await.context("reading the model's answer")?;
        if !status.is_success() {
            bail!("the model API answered {status}: {reply}");
        }
        let raw = reply["choices"][0]["message"]["content"]
            .as_str()
            .with_context(|| format!("no message in the model's answer: {reply}"))?
            .to_string();
        let fields = parse_answer(&raw)?;
        Ok((raw, fields))
    }
}

/// The JSON object in the model's answer, with or without a Markdown code fence.
pub fn parse_answer(raw: &str) -> Result<InvoiceFields> {
    let start = raw.find('{').context("no JSON object in the model's answer")?;
    let end = raw.rfind('}').context("no JSON object in the model's answer")?;
    let value: Value = serde_json::from_str(&raw[start..=end]).context("the model's JSON")?;
    // Models sometimes answer numbers as numbers.
    let text = |key: &str| match &value[key] {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    Ok(InvoiceFields {
        invoice_number: text("invoice_number").context("no invoice_number")?,
        pay_to: text("pay_to").context("no pay_to")?,
        amount: text("amount").context("no amount")?,
        currency: text("currency").context("no currency")?,
        memo: text("memo"),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_answer;

    #[test]
    fn reads_the_answer() {
        let raw = "```json\n{\"invoice_number\":\"NH-1\",\"pay_to\":\"v.testnet\",\"amount\":25.5,\"currency\":\"USDC\"}\n```";
        let fields = parse_answer(raw).unwrap();
        assert_eq!(fields.amount, "25.5");
        assert_eq!(fields.pay_to, "v.testnet");
        assert_eq!(fields.memo, None);
        assert!(parse_answer("no json").is_err());
        assert!(parse_answer("{\"pay_to\":\"v.testnet\"}").is_err());
    }
}
