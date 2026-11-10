//! The three invoice demo.
//!
//! ```sh
//! scripts/demo.sh            # a fresh local node, no setup
//! scripts/demo.sh testnet    # the deployment in deployments/testnet.json
//! ```
//!
//! Writes every invoice's result to `target/demo/<network>-payments.csv`. On testnet
//! `TRIPWIRE_RUN` sets the tag appended to the invoice numbers, by default one from the clock.

use anyhow::{bail, Result};
use sandbox_tests::demo;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = demo::repo_root();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["local"] => {
            let csv = root.join("target/demo/local-payments.csv");
            let _ = std::fs::remove_file(&csv);
            println!("Starting a local NEAR node with a Sputnik DAO, mock USDC and the guard...\n");
            demo::local(Some(csv)).await?;
        }
        ["testnet"] => {
            let csv = root.join("target/demo/testnet-payments.csv");
            let run = std::env::var("TRIPWIRE_RUN").ok();
            demo::testnet(&root.join("deployments/testnet.json"), run, Some(csv)).await?;
        }
        _ => bail!("usage: demo [local|testnet]"),
    }
    Ok(())
}
