use async_stream::stream;
use cached::proc_macro::cached;
use log::info;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::SerializableTransaction;
use solana_sdk::{
    commitment_config::CommitmentConfig, hash::Hash, transaction::VersionedTransaction,
};
use solana_transaction_builder::{PreparedTransaction, SignedTransaction, TransactionBuilder};
use solana_transaction_executor::{
    PriorityFeeConfiguration, PriorityFeePolicy, TransactionExecutor,
};
use std::sync::Arc;
use uuid::Uuid;

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
        info!(
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
    info!("Fetched a new blockhash: {blockhash}");
    Ok(blockhash)
}

pub async fn execute_transactions_in_sequence(
    transaction_executor: Arc<TransactionExecutor>,
    async_transaction_builders: Vec<TransactionBuilderExecutionData>,
) -> anyhow::Result<()> {
    let sequence_length = async_transaction_builders.len();
    for (index, async_transaction_builder) in async_transaction_builders.into_iter().enumerate() {
        let human_index = index + 1;
        let tx_uuid = &async_transaction_builder.tx_uuid;
        info!("Building the transaction {human_index}/{sequence_length}: {tx_uuid}");

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
            Ok(result) => {
                info!("Successfully executed the transaction {tx_uuid} {human_index}/{sequence_length}: {:?}", result)
            }
            Err(err) => {
                anyhow::bail!("Failed to execute the transaction {tx_uuid} {human_index}/{sequence_length}, err: {err}");
            }
        };
    }

    Ok(())
}

pub fn builder_to_execution_data(
    rpc_url: String,
    transaction_builder: &mut TransactionBuilder,
    priority_fee_policy: Option<PriorityFeePolicy>,
) -> Vec<TransactionBuilderExecutionData> {
    transaction_builder
        .sequence_combined()
        .map(|prepared_transaction| {
            TransactionBuilderExecutionData::new(
                prepared_transaction,
                rpc_url.clone(),
                priority_fee_policy
                    .clone()
                    .map_or(PriorityFeePolicy::default(), |policy| policy),
            )
        })
        .collect()
}
