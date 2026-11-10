//! `tripwire-agent`: reads one invoice and asks the guard to pay it.
//!
//! ```sh
//! cargo run -p tripwire-agent -- --invoice invoices/01-small-routine.md
//! ```
//!
//! On testnet the guard and agent accounts default to `deployments/testnet.json`. The key comes
//! from `TRIPWIRE_AGENT_KEY` or `~/.near-credentials/testnet/<agent>.json`, never from the
//! command line. Run with `--help` for every option.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use near_api::{AccountId, NetworkConfig};
use serde_json::Value;
use tripwire_agent::guard::Target;
use tripwire_agent::invoice::{self, PaymentRequest};
use tripwire_agent::{keys, llm, report, Agent};

const USAGE: &str = "\
Usage: tripwire-agent --invoice <file> [options]

Reads an invoice and asks the Tripwire guard to pay it with the agent's function call key.
The guard pays inside its policy, files a DAO proposal outside it, or refuses.

Options:
  --invoice <file>      Invoice to pay (required)
  --network <name|url>  testnet (default) or the RPC URL of another node
  --guard-id <account>  Guard account. Default: accounts.guard in deployments/testnet.json
  --agent-id <account>  Agent account. Default: accounts.agent in deployments/testnet.json
  --key-file <file>     Agent key file. Default: ~/.near-credentials/<network>/<agent>.json
  --llm                 Read the invoice with a model instead of the deterministic extractor.
                        Needs OPENAI_API_KEY, optional OPENAI_BASE_URL and OPENAI_MODEL
  --run <tag>           Append -<tag> to the invoice number, to send a file again as a new invoice
  --csv <file>          Append the result to a CSV file for bookkeeping
  --dry-run             Print the request without sending it. Needs no key
  -h, --help            Show this text

Environment:
  TRIPWIRE_AGENT_KEY    The agent's secret key, instead of a key file
  NEAR_CREDENTIALS_DIR  Key directory instead of ~/.near-credentials/<network>

Exit codes: 0 paid or sent to a vote, 2 refused by the guard, 3 failed and rolled back,
1 any other error.";

#[derive(Default)]
struct Args {
    invoice: Option<PathBuf>,
    network: Option<String>,
    guard: Option<String>,
    agent: Option<String>,
    key_file: Option<PathBuf>,
    llm: bool,
    run: Option<String>,
    csv: Option<PathBuf>,
    dry_run: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--invoice" => args.invoice = Some(value()?.into()),
            "--network" => args.network = Some(value()?),
            "--guard-id" => args.guard = Some(value()?),
            "--agent-id" => args.agent = Some(value()?),
            "--key-file" => args.key_file = Some(value()?.into()),
            "--run" => args.run = Some(value()?),
            "--csv" => args.csv = Some(value()?.into()),
            "--llm" => args.llm = true,
            "--dry-run" => args.dry_run = true,
            "--agent-key" => bail!(
                "the agent key is never passed on the command line, where shell history and the \
                 process list keep it. Use {} or a key file",
                keys::KEY_ENV
            ),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}\n\n{USAGE}"),
        }
    }
    Ok(args)
}

/// `deployments/testnet.json` of this repository, when it exists.
fn deployment() -> Option<Value> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deployments/testnet.json");
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn network(name: Option<&str>) -> Result<NetworkConfig> {
    match name.unwrap_or("testnet") {
        "testnet" => Ok(match std::env::var("NEAR_RPC_URL") {
            Ok(url) => NetworkConfig::from_rpc_url("testnet", url.parse()?),
            Err(_) => NetworkConfig::testnet(),
        }),
        url if url.starts_with("http://") || url.starts_with("https://") => {
            Ok(NetworkConfig::from_rpc_url("custom", url.parse()?))
        }
        other => bail!("--network is testnet or an RPC URL, not {other}"),
    }
}

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            1
        }
    };
    std::process::exit(code);
}

async fn run() -> Result<i32> {
    let args = parse_args()?;
    let invoice_path = args.invoice.clone().context(format!("--invoice is required\n\n{USAGE}"))?;
    let network = network(args.network.as_deref())?;
    let on_testnet = network.network_name == "testnet";
    let deployed = if on_testnet { deployment() } else { None };
    let pick = |flag: &Option<String>, name: &str| -> Result<AccountId> {
        let id = flag
            .clone()
            .or_else(|| deployed.as_ref()?["accounts"][name].as_str().map(str::to_string))
            .with_context(|| format!("--{name}-id is required off testnet"))?;
        id.parse().with_context(|| format!("{id} is not an account id"))
    };
    let guard = pick(&args.guard, "guard")?;
    let agent_id = pick(&args.agent, "agent")?;
    let target = Target { network, guard };

    let text = std::fs::read_to_string(&invoice_path)
        .with_context(|| format!("reading {}", invoice_path.display()))?;
    let fields = if args.llm {
        let model = llm::Llm::from_env()?;
        eprintln!("asking {} at {} to read the invoice", model.model, model.base_url);
        let (raw, fields) = model.extract(&text).await?;
        println!("  model said   {raw}");
        fields
    } else {
        invoice::extract(&text)?
    };
    println!("{}", invoice_path.display());
    println!(
        "  read         pay {} {} to {}, memo {:?}",
        fields.amount,
        fields.currency,
        fields.pay_to,
        fields.memo.as_deref().unwrap_or("")
    );

    if args.dry_run {
        let info = target.info().await?;
        let request = PaymentRequest::from_fields(&fields, &info.tokens, args.run.as_deref())?;
        println!("  request_id   {}", request.request_id);
        println!(
            "  would ask    {} to {} (dry run, nothing sent)",
            request.token.format(request.amount),
            request.recipient
        );
        return Ok(0);
    }

    let key = keys::load(&agent_id, &target.network.network_name, args.key_file.as_deref()).await?;
    let agent = Agent::connect(target, agent_id, key).await?;
    let request = PaymentRequest::from_fields(&fields, &agent.info.tokens, args.run.as_deref())?;
    let report = agent.pay(&request).await?;
    for line in report.lines(&agent.info.dao) {
        println!("{line}");
    }
    if let Some(path) = &args.csv {
        report::append_csv(path, &report, tripwire_agent::demo::now_sec())?;
        println!("  csv          {}", path.display());
    }
    Ok(match report.status {
        report::Status::Paid | report::Status::Escalated => 0,
        report::Status::Rejected => 2,
        report::Status::Failed => 3,
    })
}
