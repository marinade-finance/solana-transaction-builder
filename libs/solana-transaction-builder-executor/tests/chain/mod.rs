use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use litesvm::LiteSVM;
use solana_client::rpc_response::{
    Response, RpcResponseContext, RpcResult, RpcSimulateTransactionResult,
};
use solana_hash::Hash;
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
}

impl Chain {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            svm: Mutex::new(LiteSVM::new().with_sigverify(false)),
            sent: Mutex::new(Vec::new()),
            landed: Mutex::new(HashMap::new()),
        })
    }

    pub fn executor(self: &Arc<Self>) -> TransactionExecutor {
        TransactionExecutor::new(
            TransactionExecutorConfig {
                signature_status_check_interval: Duration::from_millis(100),
                transaction_send_interval: Duration::from_secs(2),
                waiting_time_after_last_send: Duration::from_secs(3),
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
                units_consumed: Some(meta.compute_units_consumed),
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
        let result = self
            .svm
            .lock()
            .unwrap()
            .send_transaction(transaction.clone())
            .map(|_| ())
            .map_err(|failed| failed.err);
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
                        confirmation_status: Some(TransactionConfirmationStatus::Confirmed),
                    })
                })
                .collect(),
        })
    }

    async fn is_blockhash_valid(&self, _blockhash: &Hash) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn latest_blockhash(&self) -> anyhow::Result<Hash> {
        Ok(self.svm.lock().unwrap().latest_blockhash())
    }
}

#[async_trait]
impl PrioritizationFeesProvider for Chain {
    async fn recent_prioritization_fees(&self, _writable: &[Pubkey]) -> anyhow::Result<Vec<u64>> {
        Ok(vec![])
    }
}
