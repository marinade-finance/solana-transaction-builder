use crate::TransactionBuilderExecutionErrors;
use async_stream::stream;
use cached::proc_macro::cached;
use log::debug;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::SerializableTransaction;
use solana_sdk::{
    commitment_config::CommitmentConfig, hash::Hash, transaction::VersionedTransaction,
};
use solana_transaction_builder::{
    get_prepared_transaction_iterator, PreparedTransaction, SignedTransaction, TransactionBuilder,
};
use solana_transaction_executor::{
    PriorityFeeConfiguration, PriorityFeePolicy, TransactionExecutor,
};
use std::sync::Arc;
use tokio::sync::Semaphore;
use uuid::Uuid;

const PARALLEL_EXECUTION_LIMIT: usize = 30;

#[derive(Clone)]
pub struct TransactionBuilderExecutionData {
    pub rpc_url: String,
    pub priority_fee_policy: PriorityFeePolicy,
    pub prepared_transaction: PreparedTransaction,
    pub tx_uuid: String,
}

impl TransactionBuilderExecutionData {
    pub fn new(
        prepared_transaction: PreparedTransaction,
        rpc_url: String,
        priority_fee_policy: PriorityFeePolicy,
    ) -> Self {
        Self {
            rpc_url,
            priority_fee_policy,
            prepared_transaction,
            tx_uuid: Uuid::new_v4().to_string(),
        }
    }

    async fn build(
        &self,
        priority_fee_configuration: PriorityFeeConfiguration,
    ) -> anyhow::Result<VersionedTransaction> {
        let latest_blockhash = get_latest_blockhash(self.rpc_url.clone()).await?;
        let transaction = self
            .prepared_transaction
            .signed_versioned_transaction(latest_blockhash)?;
        debug!(
            "Built transaction {} with blockhash {latest_blockhash} and prio fee config {priority_fee_configuration:?}",
            transaction.get_signature()
        );
        Ok(transaction)
    }
}

#[cached(result = true, time = 10, sync_writes = true)]
async fn get_latest_blockhash(url: String) -> anyhow::Result<Hash> {
    let blockhash = RpcClient::new_with_commitment(url, CommitmentConfig::finalized())
        .get_latest_blockhash()
        .await?;
    debug!("Fetched a new blockhash: {blockhash}");
    Ok(blockhash)
}

pub async fn execute_transaction_data_in_sequence(
    transaction_executor: Arc<TransactionExecutor>,
    execution_data: &[TransactionBuilderExecutionData],
    fail_on_first_error: bool,
) -> Result<(), TransactionBuilderExecutionErrors> {
    let sequence_length = execution_data.len();
    let mut errors = TransactionBuilderExecutionErrors::new();

    for (index, async_transaction_builder) in execution_data.into_iter().enumerate() {
        let human_index = index + 1;
        let tx_uuid = &async_transaction_builder.tx_uuid;
        debug!("Building the transaction {human_index}/{tx_uuid} (size: {sequence_length})");

        let async_transaction_builder = async_transaction_builder.clone();
        match transaction_executor
            .execute_transaction(stream! {
                let async_transaction_builder = async_transaction_builder.clone();
                for priority_fee_configuration in async_transaction_builder.priority_fee_policy.iter_priority_fee_configuration() {
                    yield async_transaction_builder.build(priority_fee_configuration).await;
                }
            })
            .await
        {
            Ok(sig) => {
                debug!("Transaction {sig} {human_index}/{tx_uuid} executed in sequence successfully");
            }
            Err(err) => {
                let error_description = format!("Transaction {human_index}/{tx_uuid} sequential execution failed: {:?}", err);
                debug!("{}", error_description);
                errors.add_error(
                    err,
                    error_description,
                    human_index,
                    tx_uuid.clone(),
                );
                if fail_on_first_error {
                    return Err(errors);
                }
            }
        };
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    Ok(())
}

pub async fn execute_transaction_data_in_parallel(
    transaction_executor: Arc<TransactionExecutor>,
    execution_data: &[TransactionBuilderExecutionData],
    parallel_execution_limit: Option<usize>,
) -> Result<(), TransactionBuilderExecutionErrors> {
    let sequence_length = execution_data.len();
    let parallel_execution_limit = parallel_execution_limit.unwrap_or(PARALLEL_EXECUTION_LIMIT);
    let semaphore = Arc::new(Semaphore::new(parallel_execution_limit));

    // Prepare the list of futures with their associated tx_uuid and human_index
    let futures = execution_data
        .into_iter()
        .enumerate()
        .map(|(index, async_transaction_builder)| {
            let human_index = index + 1;
            let tx_uuid = async_transaction_builder.tx_uuid.clone();
            let semaphore = Arc::clone(&semaphore);
            debug!("Building the transaction {human_index}/{tx_uuid} (size: {sequence_length})");
            let transaction_executor = Arc::clone(&transaction_executor);
            let transaction_future = async move {
                let _permit = semaphore.acquire().await.expect("Failed to acquire semaphore");
                let transaction_result = stream! {
                    for priority_fee_configuration in async_transaction_builder.priority_fee_policy.iter_priority_fee_configuration() {
                        yield async_transaction_builder.build(priority_fee_configuration).await;
                    }
                };
                transaction_executor.execute_transaction(transaction_result).await
            };
            // Return a tuple of tx_uuid, human_index, and the future
            (tx_uuid, human_index, transaction_future)
        })
        .collect::<Vec<_>>();

    // Await completion of all futures using join_all
    let results = futures::future::join_all(futures.into_iter().map(
        |(tx_uuid, human_index, future)| async move {
            let result = future.await;
            (tx_uuid, human_index, result)
        },
    ))
    .await;

    let mut errors = TransactionBuilderExecutionErrors::new();
    for (tx_uuid, human_index, result) in results {
        match result {
            Ok(sig) => {
                debug!(
                    "Transaction {sig} {human_index}/{tx_uuid} executed in parallel successfully"
                );
            }
            Err(err) => {
                let error_description =
                    format!("Transaction {human_index}/{tx_uuid} failed: {:?}", err);
                debug!("{}", error_description);
                errors.add_error(err, error_description, human_index, tx_uuid);
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    Ok(())
}

pub fn builder_to_execution_data(
    rpc_url: String,
    transaction_builder: &mut TransactionBuilder,
    priority_fee_policy: Option<PriorityFeePolicy>,
    is_one_by_one: bool,
) -> Vec<TransactionBuilderExecutionData> {
    let transaction_builder_iterator =
        get_prepared_transaction_iterator(transaction_builder, is_one_by_one);
    transaction_builder_iterator
        .map(|prepared_transaction| {
            let execution_data = TransactionBuilderExecutionData::new(
                prepared_transaction,
                rpc_url.clone(),
                priority_fee_policy
                    .clone()
                    .map_or(PriorityFeePolicy::default(), |policy| policy),
            );

            if log::log_enabled!(log::Level::Debug) {
                let description = execution_data
                    .prepared_transaction
                    .single_description()
                    .map_or_else(|| "".to_string(), |v| format!(", description: {}", v));
                debug!(
                    "Prepared transaction {}{}",
                    execution_data.tx_uuid, description
                );
            }

            execution_data
        })
        .collect()
}
