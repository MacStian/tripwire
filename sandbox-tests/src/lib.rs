//! Setup and test harness for the guard.
//!
//! [`Deployment::create`] sets up everything the guard works with under one master account: a
//! real Sputnik DAO v2, a mock USDC token, the guard itself and an agent that only holds a
//! function call key for `request_payment`. It runs the same way on a local node and on
//! testnet (see [`deploy`]). [`Harness::start`] runs it on a fresh local node. Every test starts
//! its own node, so tests never share state.

pub mod deploy;

use std::ops::Deref;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::Engine;
use near_api::signer::generate_secret_key;
use near_api::types::account::ContractState;
use near_api::types::transaction::actions::FunctionCallPermission;
use near_api::types::transaction::result::{ExecutionFinalResult, TransactionResult};
use near_api::types::{AccessKeyPermission, SecretKey};
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
pub const SPUTNIK_COMMIT: &str = "2b0f34a3860460f285307ec2ccf36345acbe05a2";

pub const NEAR: u128 = 10u128.pow(24);
/// One USDC in base units of the mock token.
pub const USDC: u128 = 10u128.pow(6);
pub const USDC_DECIMALS: u8 = 6;

/// Parent of the local accounts. The sandbox node runs its validator on it, so the harness
/// creates the accounts with a state patch instead of through it.
pub const MASTER: &str = "test.near";
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
/// Gas the agent attaches to `request_payment` in the scenario tests.
pub const AGENT_GAS: NearGas = NearGas::from_tgas(100);

/// Storage stake: 1 NEAR per 100 kB.
pub const STORAGE_PRICE_PER_BYTE: u128 = 10u128.pow(19);
/// Accounts that use at most this many bytes need no storage stake (zero balance accounts,
/// NEP-448).
pub const ZERO_BALANCE_ACCOUNT_STORAGE_LIMIT: u64 = 770;

/// NEAR an account can spend under the protocol's storage rule: `amount + locked` must cover
/// the stake for its storage after every transaction, unless the account is small enough to
/// need none.
pub fn spendable_balance(amount: NearToken, locked: NearToken, storage_usage: u64) -> NearToken {
    if storage_usage <= ZERO_BALANCE_ACCOUNT_STORAGE_LIMIT {
        return amount;
    }
    let stake = u128::from(storage_usage) * STORAGE_PRICE_PER_BYTE;
    let uncovered = stake.saturating_sub(locked.as_yoctonear());
    NearToken::from_yoctonear(amount.as_yoctonear().saturating_sub(uncovered))
}

/// [`spendable_balance`] of `id`, read from the network.
pub async fn spendable(network: &NetworkConfig, id: &AccountId) -> Result<NearToken> {
    let view = Account(id.clone())
        .view()
        .fetch_from(network)
        .await
        .with_context(|| format!("reading {id}"))?
        .data;
    Ok(spendable_balance(view.amount, view.locked, view.storage_usage))
}

const STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(10);
const NO_DEPOSIT: NearToken = NearToken::from_yoctonear(0);
/// near-sandbox sends `sandbox_fast_forward` without a time limit, so a node that stops
/// producing blocks halfway would hold the test forever. A forward here takes a few seconds.
const FAST_FORWARD_TIMEOUT: Duration = Duration::from_secs(90);

/// NEAR each account gets when the setup creates it.
#[derive(Clone, Copy, Debug)]
pub struct Balances {
    pub council: NearToken,
    pub dao: NearToken,
    pub guard: NearToken,
    pub usdc: NearToken,
    pub agent: NearToken,
    pub vendor: NearToken,
}

impl Balances {
    /// Plenty, for the tests.
    pub const LOCAL: Self = Self {
        council: NearToken::from_near(100),
        dao: NearToken::from_near(100),
        guard: NearToken::from_near(20),
        usdc: NearToken::from_near(20),
        agent: NearToken::from_near(5),
        vendor: NearToken::from_near(5),
    };

    /// Enough for the testnet demo. Contract code is staked at 1 NEAR per 100 kB: Sputnik
    /// 444 kB, guard 218 kB, mock token 153 kB. The DAO also pays the 1 NEAR budget of
    /// [`Options::testnet`], and the guard keeps its `min_near_reserve` of 1 NEAR plus room for
    /// a few proposal bonds.
    pub const TESTNET: Self = Self {
        council: NearToken::from_millinear(1_000),
        dao: NearToken::from_millinear(6_500),
        guard: NearToken::from_millinear(3_800),
        usdc: NearToken::from_millinear(2_000),
        agent: NearToken::from_millinear(500),
        vendor: NearToken::from_millinear(100),
    };

    pub fn total(&self) -> NearToken {
        let all = [self.council, self.dao, self.guard, self.usdc, self.agent, self.vendor];
        NearToken::from_yoctonear(all.iter().map(NearToken::as_yoctonear).sum())
    }
}

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
    /// Edits the default guard policy (see [`Deployment::default_policy`]) before `new`.
    pub policy: fn(&mut Value),
    pub balances: Balances,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            proposal_bond: NearToken::from_millinear(100),
            near_budget: NearToken::from_near(5),
            usdc_budget: 1_000 * USDC,
            policy: |_| {},
            balances: Balances::LOCAL,
        }
    }
}

impl Options {
    /// The testnet demo: the default policy with a 1 NEAR and 1,000 USDC budget.
    pub fn testnet() -> Self {
        Self {
            near_budget: NearToken::from_near(1),
            balances: Balances::TESTNET,
            ..Self::default()
        }
    }
}

/// Sputnik policy with a one member council and the guard role.
pub fn dao_policy(council: &AccountId, guard: &AccountId, proposal_bond: NearToken) -> Value {
    json!({
        "roles": [
            {
                "name": "council",
                "kind": { "Group": [council] },
                "permissions": ["*:*"],
                "vote_policy": {},
            },
            {
                "name": GUARD_ROLE,
                "kind": { "Group": [guard] },
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

/// The guard wasm, built once per process with `cargo near build`.
pub fn guard_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    build_once(&WASM, concat!(env!("CARGO_MANIFEST_DIR"), "/../contracts/guard/Cargo.toml"))
}

/// The mock token wasm, built once per process.
pub fn mock_ft_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    build_once(&WASM, concat!(env!("CARGO_MANIFEST_DIR"), "/../contracts/mock-ft/Cargo.toml"))
}

pub fn account(id: &str) -> Result<AccountId> {
    id.parse().with_context(|| format!("invalid account id {id}"))
}

/// The final outcome of a transaction sent with near-api.
pub fn full(result: TransactionResult) -> Result<ExecutionFinalResult> {
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

/// Fails unless a receipt of the transaction failed with exactly this contract panic, for
/// example `ERR_GUARD_FROZEN`. In a DAO vote the vote itself succeeds and only the receipt on
/// the guard fails.
pub fn check_panic(outcome: &ExecutionFinalResult, message: &str) -> Result<()> {
    let failures = format!("{:?}", outcome.failures());
    let expected = format!("{:?}", format!("Smart contract panicked: {message}"));
    ensure!(failures.contains(&expected), "expected the panic {message}, got {failures}");
    Ok(())
}

/// The `tripwire` events of all receipts, in execution order, as the NEP-297 JSON objects.
pub fn events(outcome: &ExecutionFinalResult) -> Vec<Value> {
    outcome
        .logs()
        .into_iter()
        .filter_map(|log| log.strip_prefix("EVENT_JSON:"))
        .filter_map(|json| serde_json::from_str::<Value>(json).ok())
        .filter(|event| event["standard"] == "tripwire")
        .collect()
}

/// The names of the `tripwire` events, in order.
pub fn event_names(outcome: &ExecutionFinalResult) -> Vec<String> {
    events(outcome).iter().map(|event| event["event"].as_str().unwrap_or("").to_string()).collect()
}

/// One receipt of a transaction: where it ran, the gas it burnt and its own outcome.
#[derive(Clone, Debug)]
pub struct ReceiptGas {
    pub executor: AccountId,
    pub gas: NearGas,
    pub failed: bool,
    /// Names of the `tripwire` events it emitted.
    pub events: Vec<String>,
}

/// Every receipt of the transaction, in execution order.
pub fn receipts(outcome: &ExecutionFinalResult) -> Vec<ReceiptGas> {
    outcome
        .receipt_outcomes()
        .iter()
        .map(|receipt| ReceiptGas {
            executor: receipt.executor_id.clone(),
            gas: receipt.gas_burnt,
            failed: receipt.is_failure(),
            events: receipt
                .logs
                .iter()
                .filter_map(|log| log.strip_prefix("EVENT_JSON:"))
                .filter_map(|json| serde_json::from_str::<Value>(json).ok())
                .filter(|event| event["standard"] == "tripwire")
                .map(|event| event["event"].as_str().unwrap_or("").to_string())
                .collect(),
        })
        .collect()
}

/// The return value of the whole transaction, for `request_payment` the `PaymentOutcome`.
pub fn returned(outcome: &ExecutionFinalResult) -> Result<Value> {
    Ok(outcome.clone().json::<Value>()?)
}

/// Everything the setup created, and the keys to drive it.
pub struct Deployment {
    pub network: NetworkConfig,
    /// Full access key of the master. Every account the setup creates gets the same key, and
    /// the guard loses it after `new`.
    pub signer: Arc<Signer>,
    /// Holds only the agent's function call key.
    pub agent_signer: Arc<Signer>,
    pub agent_key: PublicKey,
    /// Secret of the agent's function call key, for the agent process.
    pub agent_secret: SecretKey,
    pub master: AccountId,
    pub council: AccountId,
    pub dao: AccountId,
    pub guard: AccountId,
    pub usdc: AccountId,
    pub agent: AccountId,
    pub vendor: AccountId,
    pub proposal_bond: NearToken,
}

impl Deployment {
    /// The accounts the setup creates under `master`, in creation order.
    pub fn account_names(master: &AccountId) -> Result<[AccountId; 6]> {
        let sub = |name: &str| account(&format!("{name}.{master}"));
        Ok([
            sub("council")?,
            sub("dao")?,
            sub("guard")?,
            sub("usdc")?,
            sub("agent")?,
            sub("vendor")?,
        ])
    }

    /// Runs the full setup under `master`, which signs with `signer`:
    /// 1. subaccounts `council`, `dao`, `guard`, `usdc`, `agent` and `vendor` with the master key
    ///    and the NEAR of `opts.balances`, paid by the master,
    /// 2. to 7. [`Deployment::set_up`].
    pub async fn create(
        network: NetworkConfig,
        master: AccountId,
        signer: Arc<Signer>,
        opts: Options,
    ) -> Result<Self> {
        let deployment = Self::new(network, master, signer, opts.proposal_bond)?;
        deployment.create_accounts(&opts.balances).await?;
        deployment.set_up(&opts).await?;
        Ok(deployment)
    }

    /// Step 1 of [`Deployment::create`]: the six subaccounts, funded by the master.
    pub async fn create_accounts(&self, balances: &Balances) -> Result<()> {
        for (id, balance) in self.balances(balances) {
            self.create_subaccount(id, balance).await?;
        }
        Ok(())
    }

    /// The account ids under `master` and a new agent key. Sends nothing.
    pub fn new(
        network: NetworkConfig,
        master: AccountId,
        signer: Arc<Signer>,
        proposal_bond: NearToken,
    ) -> Result<Self> {
        let [council, dao, guard, usdc, agent, vendor] = Self::account_names(&master)?;
        let agent_secret = generate_secret_key()?;
        Ok(Self {
            network,
            signer,
            agent_signer: Signer::from_secret_key(agent_secret.clone())?,
            agent_key: agent_secret.public_key(),
            agent_secret,
            master,
            council,
            dao,
            guard,
            usdc,
            agent,
            vendor,
            proposal_bond,
        })
    }

    /// Each account with the NEAR it starts with.
    pub fn balances(&self, b: &Balances) -> [(&AccountId, NearToken); 6] {
        [
            (&self.council, b.council),
            (&self.dao, b.dao),
            (&self.guard, b.guard),
            (&self.usdc, b.usdc),
            (&self.agent, b.agent),
            (&self.vendor, b.vendor),
        ]
    }

    /// Sets up the contracts on accounts that exist and hold the signer's key:
    /// 2. the Sputnik DAO with the council and the `GuardRequestor` role,
    /// 3. the USDC mock with storage for the DAO, guard and vendor, and USDC minted to the DAO,
    /// 4. the guard with the default policy,
    /// 5. the guard account locked: every access key deleted after `new`, and checked,
    /// 6. the guard budget funded through approved DAO proposals,
    /// 7. a function call key for the agent, limited to `request_payment` on the guard.
    pub async fn set_up(&self, opts: &Options) -> Result<()> {
        self.deploy_dao().await?;
        self.deploy_usdc().await?;
        self.deploy_guard(opts.policy).await?;
        self.lock_guard().await?;
        self.fund_guard(opts.near_budget, opts.usdc_budget).await?;
        self.add_agent_key().await
    }

    /// Creates a subaccount of the master that holds the master key.
    pub async fn create_subaccount(&self, id: &AccountId, balance: NearToken) -> Result<()> {
        ensure!(id.is_sub_account_of(&self.master), "{id} is not a subaccount of {}", self.master);
        let key = self.signer.get_public_key().await?;
        let result = Account::create_account(id.clone())
            .fund_myself(self.master.clone(), balance)
            .with_public_key(key)
            .with_signer(self.signer.clone())
            .send_to(&self.network)
            .await?;
        check(&full(result)?, &format!("creating {id}"))
    }

    /// The guard policy the setup starts from, anchored at `now_sec`.
    ///
    /// USDC: 100 per payment, 300 per period. NEAR: 1 per payment, 3 per period. One day
    /// periods, 10 payouts and 5 escalations per period, the council as guardian.
    pub fn default_policy(&self, now_sec: u64) -> Value {
        json!({
            "agents": [self.agent],
            "recipients": [self.vendor],
            "token_limits": [
                {
                    "token_id": self.usdc,
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
            "guardian": self.council,
            "max_payments_per_period": 10,
            "max_escalations_per_period": 5,
            "proposal_bond": self.proposal_bond.as_yoctonear().to_string(),
            "min_near_reserve": NEAR.to_string(),
        })
    }

    async fn deploy_dao(&self) -> Result<()> {
        let config = json!({ "name": "tripwire", "purpose": "Tripwire demo DAO", "metadata": "" });
        let policy = dao_policy(&self.council, &self.guard, self.proposal_bond);
        let args = json!({ "config": config, "policy": policy });
        self.deploy(&self.dao, SPUTNIK_WASM, args).await
    }

    async fn deploy_usdc(&self) -> Result<()> {
        let args = json!({ "symbol": "USDC", "decimals": USDC_DECIMALS });
        self.deploy(&self.usdc, mock_ft_wasm(), args).await?;
        for holder in [&self.dao, &self.guard, &self.vendor] {
            self.register(&self.usdc, holder).await?;
        }
        let args = json!({ "account_id": self.dao, "amount": DAO_USDC.to_string() });
        let gas = NearGas::from_tgas(30);
        let outcome = self.call(&self.usdc, &self.usdc, "mint", args, NO_DEPOSIT, gas).await?;
        check(&outcome, "minting USDC to the DAO")
    }

    async fn deploy_guard(&self, edit_policy: fn(&mut Value)) -> Result<()> {
        let mut policy = self.default_policy(self.now_sec().await?);
        edit_policy(&mut policy);
        let args = json!({ "dao_id": self.dao, "policy": policy });
        self.deploy(&self.guard, guard_wasm(), args).await
    }

    /// Deletes every access key of the guard and checks that none is left. From here on only
    /// its own code can move its funds, and nobody can redeploy it.
    async fn lock_guard(&self) -> Result<()> {
        let keys = self.access_keys(&self.guard).await?;
        let result = Account(self.guard.clone())
            .delete_keys(keys)
            .with_signer(self.signer.clone())
            .send_to(&self.network)
            .await?;
        check(&full(result)?, "deleting the guard keys")?;
        let left = self.access_keys(&self.guard).await?;
        ensure!(left.is_empty(), "the guard still has access keys: {left:?}");
        Ok(())
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
        self.add_key(&self.agent, permission, self.agent_key).await
    }

    /// Adds `key` to `id`, signing with the master key the account holds.
    pub async fn add_key(
        &self,
        id: &AccountId,
        permission: AccessKeyPermission,
        key: PublicKey,
    ) -> Result<()> {
        let result = Account(id.clone())
            .add_key(permission, key)
            .with_signer(self.signer.clone())
            .send_to(&self.network)
            .await?;
        check(&full(result)?, &format!("adding a key to {id}"))
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

    /// Pays storage for `holder` on a mock token, from the council.
    pub async fn register(&self, token: &AccountId, holder: &AccountId) -> Result<()> {
        let args = json!({ "account_id": holder, "registration_only": true });
        let gas = NearGas::from_tgas(30);
        let outcome =
            self.call(&self.council, token, "storage_deposit", args, STORAGE_DEPOSIT, gas).await?;
        check(&outcome, &format!("registering {holder} on {token}"))
    }

    /// Deploys another mock token on `token`, an account with the signer's key, with storage
    /// for the guard and the council and `council_balance` minted to the council. It is in no
    /// guard policy.
    pub async fn deploy_token(&self, token: &AccountId, council_balance: u128) -> Result<()> {
        let args = json!({ "symbol": "FAKE", "decimals": USDC_DECIMALS });
        self.deploy(token, mock_ft_wasm(), args).await?;
        for holder in [&self.guard, &self.council] {
            self.register(token, holder).await?;
        }
        let args = json!({ "account_id": self.council, "amount": council_balance.to_string() });
        let gas = NearGas::from_tgas(30);
        let outcome = self.call(token, token, "mint", args, NO_DEPOSIT, gas).await?;
        check(&outcome, &format!("minting {token} to the council"))
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

    /// Like [`Deployment::call`], but fails when any receipt failed.
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

    /// Signs as the agent with its function call key and sends the transaction with a raw
    /// `send_tx`, so a transaction the runtime refuses comes back with the full reason
    /// (near-api keeps only the kind of that error). Returns the runtime's `InvalidTxError`
    /// and fails when the transaction was accepted.
    pub async fn agent_refusal(
        &self,
        contract: &AccountId,
        method: &str,
        args: Value,
        deposit: NearToken,
        gas: NearGas,
    ) -> Result<Value> {
        let signed = Contract(contract.clone())
            .call_function(method, args)
            .transaction()
            .deposit(deposit)
            .gas(gas)
            .with_signer(self.agent.clone(), self.agent_signer.clone())
            .presign_with(&self.network)
            .await?
            .transaction
            .signed()
            .context("the transaction was not signed")?;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(near_sdk::borsh::to_vec(&signed)?);
        let request = json!({
            "jsonrpc": "2.0",
            "id": "tripwire",
            "method": "send_tx",
            "params": { "signed_tx_base64": encoded, "wait_until": "EXECUTED_OPTIMISTIC" },
        });
        let rpc = self.network.rpc_endpoints.first().context("no RPC endpoint")?;
        let response: Value = reqwest::Client::new()
            .post(rpc.url.clone())
            .json(&request)
            .send()
            .await?
            .json()
            .await?;
        let reason = &response["error"]["data"]["TxExecutionError"]["InvalidTxError"];
        ensure!(!reason.is_null(), "{contract}.{method} was not refused: {response}");
        Ok(reason.clone())
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

    /// The agent asks the guard to pay, signing with its function call key and `gas`.
    pub async fn request_payment_with_gas(
        &self,
        request_id: &str,
        recipient: &AccountId,
        token_id: Option<&AccountId>,
        amount: u128,
        memo: Option<&str>,
        gas: NearGas,
    ) -> Result<ExecutionFinalResult> {
        let args = json!({
            "request_id": request_id,
            "recipient": recipient,
            "token_id": token_id,
            "amount": amount.to_string(),
            "memo": memo,
        });
        self.agent_call(&self.guard, "request_payment", args, NO_DEPOSIT, gas).await
    }

    /// [`Deployment::request_payment_with_gas`] with [`AGENT_GAS`].
    pub async fn request_payment(
        &self,
        request_id: &str,
        recipient: &AccountId,
        token_id: Option<&AccountId>,
        amount: u128,
        memo: Option<&str>,
    ) -> Result<ExecutionFinalResult> {
        self.request_payment_with_gas(request_id, recipient, token_id, amount, memo, AGENT_GAS)
            .await
    }

    /// Files a proposal for a single FunctionCall from the DAO, without voting on it.
    pub async fn propose_function_call(
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
        self.propose(&format!("Call {method} on {receiver}"), kind).await
    }

    /// Proposes a single FunctionCall from the DAO and approves it. Returns the proposal id and
    /// the outcome of the vote that executed it, failed or not.
    pub async fn dao_function_call_outcome(
        &self,
        receiver: &AccountId,
        method: &str,
        args: &Value,
        deposit_yocto: u128,
        gas: NearGas,
    ) -> Result<(u64, ExecutionFinalResult)> {
        let id = self.propose_function_call(receiver, method, args, deposit_yocto, gas).await?;
        Ok((id, self.approve(id).await?))
    }

    /// Like [`Deployment::dao_function_call_outcome`], but fails unless the call ran and the
    /// proposal ended as `Approved`.
    pub async fn dao_function_call(
        &self,
        receiver: &AccountId,
        method: &str,
        args: &Value,
        deposit_yocto: u128,
        gas: NearGas,
    ) -> Result<u64> {
        let (id, outcome) =
            self.dao_function_call_outcome(receiver, method, args, deposit_yocto, gas).await?;
        check(&outcome, &format!("approving proposal {id}"))?;
        let status = self.proposal(id).await?["status"].clone();
        ensure!(status == "Approved", "proposal {id} ended as {status}");
        Ok(id)
    }

    pub async fn access_keys(&self, id: &AccountId) -> Result<Vec<PublicKey>> {
        let keys = Account(id.clone()).list_keys().fetch_from(&self.network).await?;
        Ok(keys.data.into_iter().map(|(public_key, _)| public_key).collect())
    }

    /// Id the DAO gives the next proposal.
    pub async fn next_proposal_id(&self) -> Result<u64> {
        self.view(&self.dao, "get_last_proposal_id", json!({})).await
    }

    /// NEAR the account can spend, see [`spendable_balance`].
    pub async fn spendable(&self, id: &AccountId) -> Result<NearToken> {
        spendable(&self.network, id).await
    }

    /// Bytes of storage the account uses.
    pub async fn storage_usage(&self, id: &AccountId) -> Result<u64> {
        Ok(Account(id.clone()).view().fetch_from(&self.network).await?.data.storage_usage)
    }

    /// Base58 hash of the code deployed on the account.
    pub async fn code_hash(&self, id: &AccountId) -> Result<String> {
        let view = Account(id.clone()).view().fetch_from(&self.network).await?;
        match view.data.contract_state {
            ContractState::LocalHash(hash) => Ok(hash.to_string()),
            other => bail!("{id} has no code of its own: {other:?}"),
        }
    }

    pub async fn guard_status(&self) -> Result<Value> {
        self.view(&self.guard, "get_status", json!({})).await
    }

    /// `get_token_status` of the guard, `None` for native NEAR.
    pub async fn token_status(&self, token_id: Option<&AccountId>) -> Result<Value> {
        self.view(&self.guard, "get_token_status", json!({ "token_id": token_id })).await
    }

    /// USDC balance in base units.
    pub async fn usdc_balance(&self, holder: &AccountId) -> Result<u128> {
        self.ft_balance(&self.usdc, holder).await
    }

    /// Balance on any NEP-141 token in base units.
    pub async fn ft_balance(&self, token: &AccountId, holder: &AccountId) -> Result<u128> {
        let balance: String =
            self.view(token, "ft_balance_of", json!({ "account_id": holder })).await?;
        Ok(balance.parse()?)
    }

    pub async fn near_balance(&self, holder: &AccountId) -> Result<NearToken> {
        Ok(Account(holder.clone()).view().fetch_from(&self.network).await?.data.amount)
    }
}

/// A local node with the full setup under [`MASTER`]. Derefs to the [`Deployment`].
pub struct Harness {
    /// Keeps the node alive. Dropping the harness stops it.
    pub sandbox: Sandbox,
    pub deployment: Deployment,
}

impl Deref for Harness {
    type Target = Deployment;

    fn deref(&self) -> &Deployment {
        &self.deployment
    }
}

impl Harness {
    /// Starts a node, creates the accounts under [`MASTER`] with the genesis key and the NEAR
    /// of `opts.balances`, and runs [`Deployment::set_up`] on them.
    pub async fn start(opts: Options) -> Result<Self> {
        let sandbox = Sandbox::start_sandbox().await.context("starting the sandbox")?;
        let network = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
        let signer = Signer::from_secret_key(DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
        let deployment = Deployment::new(network, account(MASTER)?, signer, opts.proposal_bond)?;
        let harness = Self { sandbox, deployment };
        for (id, balance) in harness.balances(&opts.balances) {
            harness.create_account(id, balance).await?;
        }
        harness.set_up(&opts).await?;
        Ok(harness)
    }

    /// Creates an account with the genesis key through a state patch.
    pub async fn create_account(&self, id: &AccountId, balance: NearToken) -> Result<()> {
        self.sandbox
            .create_account(id.as_str().parse()?)
            .initial_balance(balance)
            .send()
            .await
            .with_context(|| format!("creating {id}"))
    }

    /// Creates another mock token at `id` (see [`Deployment::deploy_token`]).
    pub async fn create_token(&self, id: &str, council_balance: u128) -> Result<AccountId> {
        let token = account(id)?;
        self.create_account(&token, NearToken::from_near(20)).await?;
        self.deploy_token(&token, council_balance).await?;
        Ok(token)
    }

    /// Moves the chain clock to at least `target_sec` with sandbox fast forward, which
    /// produces the blocks at once instead of waiting for real time.
    pub async fn advance_to(&self, target_sec: u64) -> Result<()> {
        for _ in 0..30 {
            let now = self.now_sec().await?;
            if now >= target_sec {
                return Ok(());
            }
            let blocks = target_sec - now;
            let forward = self.sandbox.fast_forward(blocks);
            tokio::time::timeout(FAST_FORWARD_TIMEOUT, forward).await.with_context(|| {
                format!("the local node stopped while it fast forwarded {blocks} blocks")
            })??;
        }
        bail!("the chain clock did not reach {target_sec}")
    }
}
