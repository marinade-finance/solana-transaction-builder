use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use litesvm::LiteSVM;
use solana_client::{
    client_error::ClientErrorKind,
    rpc_response::{Response, RpcResponseContext, RpcResult, RpcSimulateTransactionResult},
};
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_instruction::error::InstructionError;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use solana_transaction_executor::{
    PrioritizationFeesProvider, SendTransactionMeta, SendTransactionProvider,
    SignatureStatusesProvider, SimulateTransactionProvider, TransactionExecutor,
    TransactionExecutorConfig,
};
use solana_transaction_status_client_types::{
    TransactionConfirmationStatus, TransactionStatus, UiTransactionError,
};

/// LiteSVM behind the executor's providers; simulation skips signatures, sends must verify.
pub struct Chain {
    pub svm: Mutex<LiteSVM>,
    pub sent: Mutex<Vec<VersionedTransaction>>,
    landed: Mutex<HashMap<Signature, Result<(), TransactionError>>>,
    /// A send naming this account lands with `POISONED` although its simulation passed.
    pub poison: Mutex<Option<Pubkey>>,
    pub drop_sends: AtomicBool,
    pub block_height_fails: AtomicBool,
    block_height: AtomicU64,
    pub blockhash_reads: AtomicUsize,
    /// The blockhash read with this index and every later one fail.
    pub blockhash_fails_from: AtomicUsize,
    pub simulation_rpc_fails: AtomicBool,
    pub hide_units: AtomicBool,
}

pub const POISONED: TransactionError =
    TransactionError::InstructionError(0, InstructionError::Custom(77));

impl Chain {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            svm: Mutex::new(LiteSVM::new().with_sigverify(false)),
            sent: Mutex::new(Vec::new()),
            landed: Mutex::new(HashMap::new()),
            poison: Mutex::new(None),
            drop_sends: AtomicBool::new(false),
            block_height_fails: AtomicBool::new(false),
            block_height: AtomicU64::new(0),
            blockhash_reads: AtomicUsize::new(0),
            blockhash_fails_from: AtomicUsize::new(usize::MAX),
            simulation_rpc_fails: AtomicBool::new(false),
            hide_units: AtomicBool::new(false),
        })
    }

    pub fn executor(self: &Arc<Self>) -> TransactionExecutor {
        TransactionExecutor::new(
            TransactionExecutorConfig {
                signature_status_check_interval: Duration::from_millis(50),
                transaction_send_interval: Duration::from_millis(200),
                waiting_time_after_last_send: Duration::from_secs(1),
                max_attempts: 3,
                ..TransactionExecutorConfig::default()
            },
            self.clone(),
            self.clone(),
            self.clone(),
            self.clone(),
        )
    }

    pub fn airdrop(&self, address: &Pubkey, lamports: u64) {
        self.svm.lock().unwrap().airdrop(address, lamports).unwrap();
    }

    pub fn balance(&self, address: &Pubkey) -> u64 {
        self.svm.lock().unwrap().get_balance(address).unwrap_or(0)
    }

    fn simulate(
        &self,
        transaction: VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        let result = self.svm.lock().unwrap().simulate_transaction(transaction);
        let (err, meta) = match result {
            Ok(info) => (None, info.meta),
            Err(failed) => (Some(UiTransactionError::from(failed.err)), failed.meta),
        };
        Ok(Response {
            context: RpcResponseContext {
                slot: 1,
                api_version: None,
            },
            value: RpcSimulateTransactionResult {
                err,
                logs: Some(meta.logs),
                accounts: None,
                units_consumed: (!self.hide_units.load(Ordering::SeqCst))
                    .then_some(meta.compute_units_consumed),
                loaded_accounts_data_size: None,
                return_data: None,
                inner_instructions: None,
                replacement_blockhash: None,
                fee: None,
                pre_balances: None,
                post_balances: None,
                pre_token_balances: None,
                post_token_balances: None,
                loaded_addresses: None,
            },
        })
    }
}

#[async_trait]
impl SimulateTransactionProvider for Chain {
    async fn simulate_transaction(
        &self,
        transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        self.simulate(transaction.clone())
    }

    async fn simulate_unsigned_transaction(
        &self,
        transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        if self.simulation_rpc_fails.load(Ordering::SeqCst) {
            return Err(ClientErrorKind::Custom(
                "error sending request for url (https://rpc.example.com/?api-key=SECRET)".into(),
            )
            .into());
        }
        let mut transaction = transaction.clone();
        let blockhash = self.svm.lock().unwrap().latest_blockhash();
        transaction.message.set_recent_blockhash(blockhash);
        self.simulate(transaction)
    }
}

#[async_trait]
impl SendTransactionProvider for Chain {
    async fn send_transaction(
        &self,
        transaction: &VersionedTransaction,
        _meta: SendTransactionMeta,
    ) -> anyhow::Result<Signature> {
        anyhow::ensure!(
            transaction.verify_with_results().iter().all(|valid| *valid),
            "a signature does not verify"
        );
        self.sent.lock().unwrap().push(transaction.clone());
        let signature = transaction.signatures[0];
        if self.drop_sends.load(Ordering::SeqCst) {
            return Ok(signature);
        }
        let poison = *self.poison.lock().unwrap();
        let result = if poison
            .is_some_and(|poison| transaction.message.static_account_keys().contains(&poison))
        {
            Err(POISONED)
        } else {
            self.svm
                .lock()
                .unwrap()
                .send_transaction(transaction.clone())
                .map(|_| ())
                .map_err(|failed| failed.err)
        };
        self.landed.lock().unwrap().insert(signature, result);
        Ok(signature)
    }
}

#[async_trait]
impl SignatureStatusesProvider for Chain {
    async fn get_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        let landed = self.landed.lock().unwrap();
        Ok(Response {
            context: RpcResponseContext {
                slot: 1,
                api_version: None,
            },
            value: signatures
                .iter()
                .map(|signature| {
                    landed.get(signature).map(|result| TransactionStatus {
                        slot: 1,
                        confirmations: None,
                        status: result.clone(),
                        err: result.clone().err(),
                        confirmation_status: Some(TransactionConfirmationStatus::Finalized),
                    })
                })
                .collect(),
        })
    }

    async fn get_signature_statuses_with_history(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        self.get_signature_statuses(signatures).await
    }

    async fn latest_blockhash(&self) -> anyhow::Result<(Hash, u64)> {
        let read = self.blockhash_reads.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(
            read < self.blockhash_fails_from.load(Ordering::SeqCst),
            "the node is down"
        );
        let blockhash = self.svm.lock().unwrap().latest_blockhash();
        Ok((blockhash, self.block_height.load(Ordering::SeqCst)))
    }

    /// Each read is a new block, so a blockhash expires at the first check after its read.
    async fn block_height(&self) -> anyhow::Result<u64> {
        anyhow::ensure!(
            !self.block_height_fails.load(Ordering::SeqCst),
            "the node is down"
        );
        Ok(self.block_height.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn commitment(&self) -> CommitmentConfig {
        CommitmentConfig::finalized()
    }
}

#[async_trait]
impl PrioritizationFeesProvider for Chain {
    async fn recent_prioritization_fees(&self, _writable: &[Pubkey]) -> anyhow::Result<Vec<u64>> {
        Ok(vec![])
    }
}
