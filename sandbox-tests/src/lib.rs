//! Sandbox harness for the guard.
//!
//! [`Harness::start`] runs a local NEAR node and sets up everything the guard works with: a real
//! Sputnik DAO v2, a mock USDC token, the guard itself and an agent that only holds a function
//! call key for `request_payment`. Every test starts its own node, so tests never share state.

use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::Engine;
use near_api::signer::generate_secret_key;
use near_api::types::transaction::actions::FunctionCallPermission;
use near_api::types::transaction::result::{ExecutionFinalResult, TransactionResult};
use near_api::types::AccessKeyPermission;
use near_api::{
    Account, AccountId, Chain, Contract, NearGas, NearToken, NetworkConfig, PublicKey, Signer,
};
use near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY;
use near_sandbox::Sandbox;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

/// Sputnik DAO v2 exactly as the mainnet factory `sputnik-dao.near` deploys it today: version
/// 3.2, commit 2b0f34a3860460f285307ec2ccf36345acbe05a2. `scripts/fetch_sputnik_wasm.py`
/// downloads it again and checks it against the factory's code hash.
pub const SPUTNIK_WASM: &[u8] = include_bytes!("../res/sputnikdao2.wasm");

pub const NEAR: u128 = 10u128.pow(24);
/// One USDC in base units of the mock token.
pub const USDC: u128 = 10u128.pow(6);
pub const USDC_DECIMALS: u8 = 6;

pub const COUNCIL: &str = "council.test.near";
pub const DAO: &str = "dao.test.near";
pub const GUARD: &str = "guard.test.near";
pub const USDC_TOKEN: &str = "usdc.test.near";
pub const AGENT: &str = "agent.test.near";
pub const VENDOR: &str = "vendor.test.near";

/// Name of the DAO role that lets the guard file Transfer proposals and nothing else.
pub const GUARD_ROLE: &str = "GuardRequestor";
/// USDC the DAO holds after setup, before it funds the guard.
pub const DAO_USDC: u128 = 10_000 * USDC;
/// Gas allowance of the agent's function call key.
pub const AGENT_KEY_ALLOWANCE: NearToken = NearToken::from_millinear(250);

const STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(10);
const NO_DEPOSIT: NearToken = NearToken::from_yoctonear(0);

/// Setup choices. `Options::default()` matches the scenarios in the E2 epic.
#[derive(Clone, Copy)]
pub struct Options {
    /// Bond the DAO asks for each proposal. The guard policy gets the same value.
    pub proposal_bond: NearToken,
    /// NEAR the DAO moves into the guard budget with a `fund` proposal. Zero skips it.
    pub near_budget: NearToken,
    /// USDC in base units the DAO moves into the guard budget with `ft_transfer_call`.
    /// Zero skips it.
    pub usdc_budget: u128,
    /// Edits the default guard policy (see [`default_policy`]) before `new`.
    pub policy: fn(&mut Value),
}

impl Default for Options {
    fn default() -> Self {
        Self {
            proposal_bond: NearToken::from_millinear(100),
            near_budget: NearToken::from_near(5),
            usdc_budget: 1_000 * USDC,
            policy: |_| {},
        }
    }
}

/// The guard policy every test starts from, anchored at `now_sec`.
///
/// USDC: 100 per payment, 300 per period. NEAR: 1 per payment, 3 per period. One day periods,
/// 10 payouts and 5 escalations per period, the council as guardian.
pub fn default_policy(now_sec: u64, proposal_bond: NearToken) -> Value {
    json!({
        "agents": [AGENT],
        "recipients": [VENDOR],
        "token_limits": [
            {
                "token_id": USDC_TOKEN,
                "max_per_payment": (100 * USDC).to_string(),
                "max_per_period": (300 * USDC).to_string(),
            },
            {
                "token_id": null,
                "max_per_payment": NEAR.to_string(),
                "max_per_period": (3 * NEAR).to_string(),
            },
        ],
        "period_seconds": 86_400,
        "period_anchor": now_sec,
        "expires_at": null,
        "guardian": COUNCIL,
        "max_payments_per_period": 10,
        "max_escalations_per_period": 5,
        "proposal_bond": proposal_bond.as_yoctonear().to_string(),
        "min_near_reserve": NEAR.to_string(),
    })
}

/// Sputnik policy with a one member council and the guard role.
pub fn dao_policy(proposal_bond: NearToken) -> Value {
    json!({
        "roles": [
            {
                "name": "council",
                "kind": { "Group": [COUNCIL] },
                "permissions": ["*:*"],
                "vote_policy": {},
            },
            {
                "name": GUARD_ROLE,
                "kind": { "Group": [GUARD] },
                "permissions": ["transfer:AddProposal"],
                "vote_policy": {},
            },
        ],
        "default_vote_policy": { "weight_kind": "RoleWeight", "quorum": "0", "threshold": [1, 2] },
        "proposal_bond": proposal_bond.as_yoctonear().to_string(),
        "proposal_period": "604800000000000",
        "bounty_bond": NEAR.to_string(),
        "bounty_forgiveness_period": "86400000000000",
    })
}

fn build_once(cell: &'static OnceLock<Vec<u8>>, manifest: &str) -> &'static [u8] {
    cell.get_or_init(|| {
        let opts = cargo_near_build::BuildOpts {
            manifest_path: Some(manifest.into()),
            ..Default::default()
        };
        let path = cargo_near_build::build_with_cli(opts)
            .unwrap_or_else(|err| panic!("building {manifest} failed: {err:?}"));
        std::fs::read(&path).unwrap_or_else(|err| panic!("reading {path} failed: {err}"))
    })
}

/// The guard wasm, built once per test binary with `cargo near build`.
pub fn guard_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    build_once(&WASM, concat!(env!("CARGO_MANIFEST_DIR"), "/../contracts/guard/Cargo.toml"))
}

/// The mock token wasm, built once per test binary.
pub fn mock_ft_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    build_once(&WASM, concat!(env!("CARGO_MANIFEST_DIR"), "/../contracts/mock-ft/Cargo.toml"))
}

pub fn account(id: &str) -> Result<AccountId> {
    id.parse().with_context(|| format!("invalid account id {id}"))
}

fn full(result: TransactionResult) -> Result<ExecutionFinalResult> {
    result.into_full().ok_or_else(|| anyhow!("transaction is still pending"))
}

/// Fails when the transaction or any of its receipts failed.
pub fn check(outcome: &ExecutionFinalResult, what: &str) -> Result<()> {
    let failures = outcome.failures();
    if outcome.is_failure() || !failures.is_empty() {
        bail!("{what} failed: {failures:?}");
    }
    Ok(())
}

pub struct Harness {
    /// Keeps the node alive. Dropping the harness stops it.
    pub sandbox: Sandbox,
    pub network: NetworkConfig,
    /// Full access key of every account the harness creates.
    pub signer: Arc<Signer>,
    /// Holds only the agent's function call key.
    pub agent_signer: Arc<Signer>,
    pub agent_key: PublicKey,
    pub council: AccountId,
    pub dao: AccountId,
    pub guard: AccountId,
    pub usdc: AccountId,
    pub agent: AccountId,
    pub vendor: AccountId,
    pub proposal_bond: NearToken,
}

impl Harness {
    /// Starts a node and runs the full E2-S1 setup:
    /// 1. accounts for the council, DAO, guard, token, agent and vendor,
    /// 2. the Sputnik DAO with the council and the `GuardRequestor` role,
    /// 3. the USDC mock with storage for the DAO, guard and vendor, and USDC minted to the DAO,
    /// 4. the guard with the default policy, funded through approved DAO proposals,
    /// 5. the guard account locked: every access key deleted after `new`,
    /// 6. a function call key for the agent, limited to `request_payment` on the guard.
    pub async fn start(opts: Options) -> Result<Self> {
        let sandbox = Sandbox::start_sandbox().await.context("starting the sandbox")?;
        let network = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
        let signer = Signer::from_secret_key(DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;

        for (id, near) in
            [(COUNCIL, 100), (DAO, 100), (GUARD, 20), (USDC_TOKEN, 20), (AGENT, 5), (VENDOR, 5)]
        {
            sandbox
                .create_account(id.parse()?)
                .initial_balance(NearToken::from_near(near))
                .send()
                .await
                .with_context(|| format!("creating {id}"))?;
        }

        let agent_secret = generate_secret_key()?;
        let agent_key = agent_secret.public_key();
        let agent_signer = Signer::from_secret_key(agent_secret)?;

        let harness = Self {
            sandbox,
            network,
            signer,
            agent_signer,
            agent_key,
            council: account(COUNCIL)?,
            dao: account(DAO)?,
            guard: account(GUARD)?,
            usdc: account(USDC_TOKEN)?,
            agent: account(AGENT)?,
            vendor: account(VENDOR)?,
            proposal_bond: opts.proposal_bond,
        };
        harness.deploy_dao().await?;
        harness.deploy_usdc().await?;
        harness.deploy_guard(opts.policy).await?;
        harness.lock_guard().await?;
        harness.fund_guard(opts.near_budget, opts.usdc_budget).await?;
        harness.add_agent_key().await?;
        Ok(harness)
    }

    async fn deploy_dao(&self) -> Result<()> {
        let config =
            json!({ "name": "tripwire", "purpose": "Tripwire sandbox tests", "metadata": "" });
        let args = json!({ "config": config, "policy": dao_policy(self.proposal_bond) });
        self.deploy(&self.dao, SPUTNIK_WASM, args).await
    }

    async fn deploy_usdc(&self) -> Result<()> {
        let args = json!({ "symbol": "USDC", "decimals": USDC_DECIMALS });
        self.deploy(&self.usdc, mock_ft_wasm(), args).await?;
        for holder in [&self.dao, &self.guard, &self.vendor] {
            self.storage_deposit(holder).await?;
        }
        let args = json!({ "account_id": self.dao, "amount": DAO_USDC.to_string() });
        let gas = NearGas::from_tgas(30);
        let outcome = self.call(&self.usdc, &self.usdc, "mint", args, NO_DEPOSIT, gas).await?;
        check(&outcome, "minting USDC to the DAO")
    }

    async fn deploy_guard(&self, edit_policy: fn(&mut Value)) -> Result<()> {
        let mut policy = default_policy(self.now_sec().await?, self.proposal_bond);
        edit_policy(&mut policy);
        let args = json!({ "dao_id": self.dao, "policy": policy });
        self.deploy(&self.guard, guard_wasm(), args).await
    }

    /// Deletes every access key of the guard. From here on only its own code can move its
    /// funds, and nobody can redeploy it.
    async fn lock_guard(&self) -> Result<()> {
        let keys = self.access_keys(&self.guard).await?;
        let result = Account(self.guard.clone())
            .delete_keys(keys)
            .with_signer(self.signer.clone())
            .send_to(&self.network)
            .await?;
        check(&full(result)?, "deleting the guard keys")
    }

    async fn fund_guard(&self, near: NearToken, usdc: u128) -> Result<()> {
        if !near.is_zero() {
            let gas = NearGas::from_tgas(30);
            self.dao_function_call(&self.guard, "fund", &json!({}), near.as_yoctonear(), gas)
                .await
                .context("funding the guard with NEAR")?;
        }
        if usdc > 0 {
            let args = json!({ "receiver_id": self.guard, "amount": usdc.to_string(), "msg": "" });
            let gas = NearGas::from_tgas(100);
            self.dao_function_call(&self.usdc, "ft_transfer_call", &args, 1, gas)
                .await
                .context("funding the guard with USDC")?;
        }
        Ok(())
    }

    async fn add_agent_key(&self) -> Result<()> {
        let permission = AccessKeyPermission::FunctionCall(FunctionCallPermission {
            allowance: Some(AGENT_KEY_ALLOWANCE),
            receiver_id: self.guard.to_string(),
            method_names: vec!["request_payment".to_string()],
        });
        let result = Account(self.agent.clone())
            .add_key(permission, self.agent_key)
            .with_signer(self.signer.clone())
            .send_to(&self.network)
            .await?;
        check(&full(result)?, "adding the agent key")
    }

    async fn deploy(&self, id: &AccountId, wasm: &[u8], init_args: Value) -> Result<()> {
        let result = Contract::deploy(id.clone())
            .use_code(wasm.to_vec())
            .with_init_call("new", init_args)?
            .gas(NearGas::from_tgas(300))
            .with_signer(self.signer.clone())
            .send_to(&self.network)
            .await?;
        check(&full(result)?, &format!("deploying {id}"))
    }

    async fn storage_deposit(&self, holder: &AccountId) -> Result<()> {
        let args = json!({ "account_id": holder, "registration_only": true });
        let gas = NearGas::from_tgas(30);
        let outcome = self
            .call(&self.council, &self.usdc, "storage_deposit", args, STORAGE_DEPOSIT, gas)
            .await?;
        check(&outcome, &format!("registering {holder} on the token"))
    }

    /// Current block time in seconds, the clock the guard sees.
    pub async fn now_sec(&self) -> Result<u64> {
        let block = Chain::block().fetch_from(&self.network).await?;
        Ok(block.header.timestamp / 1_000_000_000)
    }

    pub async fn view<T>(&self, contract: &AccountId, method: &str, args: Value) -> Result<T>
    where
        T: DeserializeOwned + Send + Sync,
    {
        let data = Contract(contract.clone())
            .call_function(method, args)
            .read_only::<T>()
            .fetch_from(&self.network)
            .await
            .with_context(|| format!("view {contract}.{method}"))?;
        Ok(data.data)
    }

    /// Signs with the full access key of `signer_id` and returns the outcome, failed or not.
    pub async fn call(
        &self,
        signer_id: &AccountId,
        contract: &AccountId,
        method: &str,
        args: Value,
        deposit: NearToken,
        gas: NearGas,
    ) -> Result<ExecutionFinalResult> {
        let result = Contract(contract.clone())
            .call_function(method, args)
            .transaction()
            .deposit(deposit)
            .gas(gas)
            .with_signer(signer_id.clone(), self.signer.clone())
            .send_to(&self.network)
            .await?;
        full(result)
    }

    /// Like [`Harness::call`], but fails when any receipt failed.
    pub async fn call_ok(
        &self,
        signer_id: &AccountId,
        contract: &AccountId,
        method: &str,
        args: Value,
        deposit: NearToken,
        gas: NearGas,
    ) -> Result<ExecutionFinalResult> {
        let outcome = self.call(signer_id, contract, method, args, deposit, gas).await?;
        check(&outcome, &format!("{signer_id} calling {contract}.{method}"))?;
        Ok(outcome)
    }

    /// Signs as the agent with its function call key only. The runtime rejects the transaction
    /// before execution when the key does not allow it, which surfaces here as an error.
    pub async fn agent_call(
        &self,
        contract: &AccountId,
        method: &str,
        args: Value,
        deposit: NearToken,
        gas: NearGas,
    ) -> Result<ExecutionFinalResult> {
        let result = Contract(contract.clone())
            .call_function(method, args)
            .transaction()
            .deposit(deposit)
            .gas(gas)
            .with_signer(self.agent.clone(), self.agent_signer.clone())
            .send_to(&self.network)
            .await?;
        full(result)
    }

    /// Files a proposal from the council with the DAO bond and returns its id.
    pub async fn propose(&self, description: &str, kind: Value) -> Result<u64> {
        let args = json!({ "proposal": { "description": description, "kind": kind } });
        let gas = NearGas::from_tgas(50);
        let outcome = self
            .call_ok(&self.council, &self.dao, "add_proposal", args, self.proposal_bond, gas)
            .await?;
        Ok(outcome.json::<u64>()?)
    }

    pub async fn proposal(&self, id: u64) -> Result<Value> {
        self.view(&self.dao, "get_proposal", json!({ "id": id })).await
    }

    /// The council votes to approve. With a one member council this executes the proposal.
    pub async fn approve(&self, id: u64) -> Result<ExecutionFinalResult> {
        let kind = self.proposal(id).await?["kind"].clone();
        let args = json!({ "id": id, "action": "VoteApprove", "proposal": kind });
        self.call(
            &self.council,
            &self.dao,
            "act_proposal",
            args,
            NO_DEPOSIT,
            NearGas::from_tgas(300),
        )
        .await
    }

    /// Proposes a single FunctionCall from the DAO, approves it and checks that it ran.
    pub async fn dao_function_call(
        &self,
        receiver: &AccountId,
        method: &str,
        args: &Value,
        deposit_yocto: u128,
        gas: NearGas,
    ) -> Result<u64> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(args)?);
        let kind = json!({ "FunctionCall": {
            "receiver_id": receiver,
            "actions": [{
                "method_name": method,
                "args": encoded,
                "deposit": deposit_yocto.to_string(),
                "gas": gas.as_gas().to_string(),
            }],
        } });
        let id = self.propose(&format!("Call {method} on {receiver}"), kind).await?;
        check(&self.approve(id).await?, &format!("approving proposal {id}"))?;
        let status = self.proposal(id).await?["status"].clone();
        ensure!(status == "Approved", "proposal {id} ended as {status}");
        Ok(id)
    }

    pub async fn access_keys(&self, id: &AccountId) -> Result<Vec<PublicKey>> {
        let keys = Account(id.clone()).list_keys().fetch_from(&self.network).await?;
        Ok(keys.data.into_iter().map(|(public_key, _)| public_key).collect())
    }

    pub async fn guard_status(&self) -> Result<Value> {
        self.view(&self.guard, "get_status", json!({})).await
    }

    /// USDC balance in base units.
    pub async fn usdc_balance(&self, holder: &AccountId) -> Result<u128> {
        let balance: String =
            self.view(&self.usdc, "ft_balance_of", json!({ "account_id": holder })).await?;
        Ok(balance.parse()?)
    }

    pub async fn near_balance(&self, holder: &AccountId) -> Result<NearToken> {
        Ok(Account(holder.clone()).view().fetch_from(&self.network).await?.data.amount)
    }
}
