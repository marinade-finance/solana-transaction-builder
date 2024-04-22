use async_stream::stream;
use cached::proc_macro::cached;
use log::{error, info};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::SerializableTransaction;
use solana_sdk::{
    commitment_config::CommitmentConfig, hash::Hash, transaction::VersionedTransaction,
};
use solana_transaction_builder::{
    SendablePreparedTransaction, SignedTransaction, TransactionBuilder,
};
use solana_transaction_executor::{
    PriorityFeeConfiguration, PriorityFeePolicy, TransactionExecutor,
};
use std::sync::Arc;
use tokio::{
    sync::{
        mpsc::{channel, Sender},
        Semaphore,
    },
    task::JoinHandle,
};
use uuid::Uuid;

const TRANSACTION_CHANNEL_SIZE: usize = 2;
const TRANSACTIONS_IN_PARALLEL: usize = 4;

#[derive(Clone)]
pub struct TransactionBuilderExecutionData {
    rpc_url: String,
    priority_fee_policy: PriorityFeePolicy,
    prepared_transaction: SendablePreparedTransaction,
    tx_uuid: String,
}

impl TransactionBuilderExecutionData {
    pub fn new(
        prepared_transaction: SendablePreparedTransaction,
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

pub async fn send_async_transaction_builder_combined(
    rpc_url: String,
    tx_transaction: Sender<Vec<TransactionBuilderExecutionData>>,
    transaction_builder: &mut TransactionBuilder,
    priority_fee_policy: Option<PriorityFeePolicy>,
) {
    let async_transaction_builders = Vec::new();
    let execution_data: Vec<TransactionBuilderExecutionData> = transaction_builder
        .sequence_combined()
        .map(|prepared_transaction| {
            TransactionBuilderExecutionData::new(
                prepared_transaction.into_sendable(),
                rpc_url.clone(),
                priority_fee_policy
                    .clone()
                    .map_or(PriorityFeePolicy::default(), |policy| policy),
            )
        })
        .collect();
    info!(
        "Enqueuing transaction sequence: {:?}",
        execution_data
            .iter()
            .map(|builder| &builder.tx_uuid)
            .collect::<Vec<_>>()
    );
    if let Err(err) = tx_transaction.send(async_transaction_builders).await {
        error!(
            "Failed to pass the sequence of async transaction builders through the channel: {err}"
        );
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

pub fn spawn_transaction_simulator(
    transaction_executor: Arc<TransactionExecutor>,
) -> (JoinHandle<()>, Sender<Vec<TransactionBuilderExecutionData>>) {
    info!("Spawning transaction simulator");
    let (tx_transaction, mut rx_transaction) =
        channel::<Vec<TransactionBuilderExecutionData>>(TRANSACTION_CHANNEL_SIZE);

    let handle = tokio::spawn(async move {
        while let Some(async_transaction_builders) = rx_transaction.recv().await {
            for async_transaction_builder in async_transaction_builders {
                let tx_uuid = async_transaction_builder.tx_uuid.clone();
                info!("Building the transaction: {}", tx_uuid);
                let priority_fee_configuration = async_transaction_builder
                    .priority_fee_policy
                    .iter_priority_fee_configuration()
                    .nth(1)
                    .unwrap();
                match async_transaction_builder
                    .build(priority_fee_configuration)
                    .await
                {
                    Ok(transaction) => {
                        info!(
                            "Simulating the transaction: {:?}",
                            transaction.signatures[0]
                        );
                        match transaction_executor
                            .simulate_transaction_provider
                            .simulate_transaction(&transaction)
                            .await
                        {
                            Ok(result) => {
                                info!("Successfully simulated the transaction: {result:?}")
                            }
                            Err(err) => {
                                error!("Failed to simulate the transaction: {err}")
                            }
                        };
                    }
                    Err(err) => {
                        error!("Failed to build the transaction: {tx_uuid} {err:?}")
                    }
                }
            }
        }

        info!("Exiting transaction simulator");
    });

    (handle, tx_transaction)
}

pub fn spawn_transaction_executor(
    transaction_executor: Arc<TransactionExecutor>,
    exit_on_error: bool,
) -> (JoinHandle<()>, Sender<Vec<TransactionBuilderExecutionData>>) {
    info!("Spawning transaction executor");
    let (tx_transaction, mut rx_transaction) =
        channel::<Vec<TransactionBuilderExecutionData>>(TRANSACTION_CHANNEL_SIZE);

    let handle = tokio::spawn(async move {
        let parallel_transaction_execution_throttle =
            Arc::new(Semaphore::new(TRANSACTIONS_IN_PARALLEL));

        while let Some(async_transaction_builders) = rx_transaction.recv().await {
            let tx_uuids: Vec<_> = async_transaction_builders
                .iter()
                .map(|builder| builder.tx_uuid.clone())
                .collect();
            info!("Received request to execute transaction sequence, waiting for execution permit: {tx_uuids:?}");

            // Limit how many transaction in parallel are executed at most
            let execution_permit = parallel_transaction_execution_throttle
                .clone()
                .acquire_owned()
                .await
                .unwrap();
            info!("Execution permit acquired: {tx_uuids:?}");

            // Clone variables that need to be moved to the task
            let transaction_executor = transaction_executor.clone();

            tokio::spawn(async move {
                match execute_transactions_in_sequence(
                    transaction_executor,
                    async_transaction_builders,
                )
                .await
                {
                    Ok(_) => info!("Successfully executed the sequence: {tx_uuids:?}"),
                    Err(err) => {
                        error!("Failed to executed the sequence: {tx_uuids:?} {err}");
                        if exit_on_error {
                            panic!("Exiting the program after transaction failure")
                        }
                    }
                };

                drop(execution_permit); // the drop is here to move the ownership of the permit to this task
            });
        }

        // Consuming all permits makes sure that all tasks have finished
        let _ = parallel_transaction_execution_throttle
            .acquire_many_owned(TRANSACTIONS_IN_PARALLEL as u32)
            .await
            .unwrap();

        info!("Exiting transaction executor");
    });

    (handle, tx_transaction)
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

pub fn spawn_transaction_handler(
    simulate: bool,
    transaction_executor: Arc<TransactionExecutor>,
    exit_on_error: bool,
) -> (JoinHandle<()>, Sender<Vec<TransactionBuilderExecutionData>>) {
    if simulate {
        spawn_transaction_simulator(transaction_executor)
    } else {
        spawn_transaction_executor(transaction_executor, exit_on_error)
    }
}
