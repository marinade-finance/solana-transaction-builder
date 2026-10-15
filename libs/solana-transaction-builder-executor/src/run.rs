use std::{collections::HashMap, sync::Arc};

use futures_util::stream;
use log::{info, warn};
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_builder::{
    Batch, Budget, DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT, Group, Limits, MessageVersion, compile,
    pack, sign, wire_len,
};
use solana_transaction_executor::{
    Execution, FeePolicy, NotLanded, Outcome, Simulation, TransactionExecutor,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunConfig {
    pub version: MessageVersion,
    pub limits: Limits,
    pub fee: FeePolicy,
    pub dry_run: bool,
}

impl RunConfig {
    pub fn new(version: MessageVersion) -> Self {
        Self {
            version,
            limits: Limits::of(version),
            fee: FeePolicy::default(),
            dry_run: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupStatus {
    Landed {
        signature: Signature,
        slot: u64,
    },
    Failed {
        error: String,
        logs: Vec<String>,
        signature: Option<Signature>,
    },
    /// Every attempt's blockhash expired and none landed; safe to plan again.
    NotLanded {
        attempts: u32,
    },
    /// An attempt may still land; planning the group again could run it twice.
    Unknown {
        reason: String,
        signatures: Vec<Signature>,
    },
    Planned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupOutcome {
    pub id: String,
    pub status: GroupStatus,
}

/// The first attempt's budget; a retry escalates the rate from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxReport {
    pub group_ids: Vec<String>,
    pub bytes: usize,
    pub addresses: usize,
    pub compute_units_consumed: u64,
    pub compute_unit_limit: u32,
    pub loaded_accounts_data_size_limit: u32,
    pub micro_lamports_per_cu: u64,
    pub priority_fee_lamports: u64,
    pub attempts: u32,
    pub signature: Option<Signature>,
    pub execution: Option<Execution>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    pub groups: Vec<GroupOutcome>,
    pub transactions: Vec<TxReport>,
}

impl RunReport {
    fn set(&mut self, ids: &[String], status: GroupStatus) {
        self.groups.extend(ids.iter().map(|id| GroupOutcome {
            id: id.clone(),
            status: status.clone(),
        }));
    }
}

struct Failure {
    error: String,
    logs: Vec<String>,
}

impl From<Failure> for GroupStatus {
    fn from(failure: Failure) -> Self {
        GroupStatus::Failed {
            error: failure.error,
            logs: failure.logs,
            signature: None,
        }
    }
}

/// A batch whose failure is proven, in simulation or on chain, goes one group per transaction.
pub async fn run(
    executor: &TransactionExecutor,
    payer: &Pubkey,
    groups: Vec<Group>,
    signers: &[Arc<dyn Signer + Send + Sync>],
    config: &RunConfig,
) -> RunReport {
    let mut report = RunReport::default();
    let mut passing = Vec::new();
    for mut group in groups {
        match simulate(executor, payer, &alone(&group), config).await {
            Ok(simulation) => {
                group.compute_units = simulation
                    .units_consumed
                    .and_then(|units| u32::try_from(units).ok());
                passing.push(group);
            }
            Err(failure) => report.set(&[group.id], failure.into()),
        }
    }
    let (batches, errors) = pack(payer, &passing, config.version, &config.limits);
    for error in errors {
        let id = match &error {
            solana_transaction_builder::PackError::GroupTooLarge { id, .. }
            | solana_transaction_builder::PackError::Budget { id, .. } => id.clone(),
        };
        report.set(
            &[id],
            GroupStatus::Failed {
                error: error.to_string(),
                logs: vec![],
                signature: None,
            },
        );
    }
    let by_id: HashMap<String, Group> = passing
        .into_iter()
        .map(|group| (group.id.clone(), group))
        .collect();
    for batch in batches {
        let split = match simulate(executor, payer, &batch, config).await {
            Ok(simulation) => {
                send(
                    executor,
                    payer,
                    &batch,
                    &simulation,
                    signers,
                    config,
                    &mut report,
                )
                .await
            }
            Err(failure) if batch.group_ids.len() == 1 => {
                report.set(&batch.group_ids, failure.into());
                false
            }
            Err(failure) => {
                warn!(
                    "Batch {:?} failed together in simulation ({})",
                    batch.group_ids, failure.error
                );
                true
            }
        };
        if !split {
            continue;
        }
        info!("Sending the groups of {:?} one by one", batch.group_ids);
        for id in &batch.group_ids {
            let single = alone(&by_id[id]);
            match simulate(executor, payer, &single, config).await {
                Ok(simulation) => {
                    send(
                        executor,
                        payer,
                        &single,
                        &simulation,
                        signers,
                        config,
                        &mut report,
                    )
                    .await;
                }
                Err(failure) => report.set(&single.group_ids, failure.into()),
            }
        }
    }
    report
}

fn alone(group: &Group) -> Batch {
    Batch {
        group_ids: vec![group.id.clone()],
        instructions: group.instructions.clone(),
        heap_bytes: group.heap_bytes,
        compute_units: u64::from(group.compute_units.unwrap_or(0)),
    }
}

async fn simulate(
    executor: &TransactionExecutor,
    payer: &Pubkey,
    batch: &Batch,
    config: &RunConfig,
) -> Result<Simulation, Failure> {
    let budget = Budget {
        compute_unit_limit: Some(config.limits.max_compute_units),
        // A legacy price is an instruction that costs units, so the measured limit must cover it.
        micro_lamports_per_cu: Some(1),
        loaded_accounts_data_size_limit: Some(DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT),
        heap_bytes: batch.heap_bytes,
    };
    let transaction = compile(
        payer,
        &batch.instructions,
        Hash::default(),
        config.version,
        &budget,
    )
    .map_err(|error| Failure {
        error: error.to_string(),
        logs: vec![],
    })?;
    let simulation = executor
        .simulate(&transaction)
        .await
        .map_err(|error| Failure {
            error: format!("Simulation could not run: {error}"),
            logs: vec![],
        })?;
    match simulation.error {
        Some(error) => Err(Failure {
            error: error.to_string(),
            logs: simulation.logs,
        }),
        None => Ok(simulation),
    }
}

/// Reports the batch, or returns true when its groups should be sent one by one instead.
async fn send(
    executor: &TransactionExecutor,
    payer: &Pubkey,
    batch: &Batch,
    simulation: &Simulation,
    signers: &[Arc<dyn Signer + Send + Sync>],
    config: &RunConfig,
    report: &mut RunReport,
) -> bool {
    let limits = &config.limits;
    let consumed = simulation.units_consumed.unwrap_or(0);
    let compute_unit_limit = u32::try_from(limits.with_margin(consumed))
        .unwrap_or(u32::MAX)
        .min(limits.max_compute_units);
    let loaded_accounts_data_size_limit = simulation
        .loaded_accounts_data_size
        .map(|bytes| limits.with_margin(u64::from(bytes)))
        .and_then(|bytes| u32::try_from(bytes).ok())
        .map_or(DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT, |bytes| {
            bytes.min(DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT)
        });
    let budget = |rate: u64| Budget {
        compute_unit_limit: Some(compute_unit_limit),
        micro_lamports_per_cu: Some(rate),
        loaded_accounts_data_size_limit: Some(loaded_accounts_data_size_limit),
        heap_bytes: batch.heap_bytes,
    };
    let first = config.fee.first();
    let planned = match compile(
        payer,
        &batch.instructions,
        Hash::default(),
        config.version,
        &budget(first),
    ) {
        Ok(transaction) => transaction,
        Err(error) => {
            report.set(
                &batch.group_ids,
                GroupStatus::Failed {
                    error: error.to_string(),
                    logs: vec![],
                    signature: None,
                },
            );
            return false;
        }
    };
    let mut tx_report = TxReport {
        group_ids: batch.group_ids.clone(),
        bytes: wire_len(&planned),
        addresses: planned.message.static_account_keys().len(),
        compute_units_consumed: consumed,
        compute_unit_limit,
        loaded_accounts_data_size_limit,
        micro_lamports_per_cu: first,
        priority_fee_lamports: budget(first).priority_fee_lamports(),
        attempts: 0,
        signature: None,
        execution: None,
    };
    if config.dry_run {
        report.set(&batch.group_ids, GroupStatus::Planned);
        report.transactions.push(tx_report);
        return false;
    }

    let writable = writable_accounts(payer, batch);
    let budget_ref = &budget;
    let writable_ref = &writable;
    let attempts_stream = stream::unfold(None, |previous: Option<u64>| async move {
        let rate = match previous {
            None => first,
            Some(previous) => {
                executor
                    .next_rate(&config.fee, previous, writable_ref)
                    .await
            }
        };
        let built = build(
            executor,
            payer,
            batch,
            &budget_ref(rate),
            signers,
            config.version,
        )
        .await;
        Some((built, Some(rate)))
    });
    let execution = executor.execute_transaction(attempts_stream).await;
    tx_report.attempts = u32::try_from(execution.attempts.len()).unwrap_or(u32::MAX);
    let alone = batch.group_ids.len() == 1;
    let mut split = false;
    let status = match &execution.outcome {
        Outcome::Landed { signature, slot } => {
            info!("Batch {:?} landed as {signature}", batch.group_ids);
            tx_report.signature = Some(*signature);
            Some(GroupStatus::Landed {
                signature: *signature,
                slot: *slot,
            })
        }
        Outcome::LandedWithError {
            signature, error, ..
        } => {
            tx_report.signature = Some(*signature);
            split = !alone;
            alone.then(|| GroupStatus::Failed {
                error: error.to_string(),
                logs: vec![],
                signature: Some(*signature),
            })
        }
        Outcome::NotLanded(NotLanded::Refused { error, logs }) => {
            split = !alone;
            alone.then(|| GroupStatus::Failed {
                error: error.to_string(),
                logs: logs.clone(),
                signature: None,
            })
        }
        Outcome::NotLanded(NotLanded::NotSent { error }) => Some(GroupStatus::Failed {
            error: error.clone(),
            logs: vec![],
            signature: None,
        }),
        Outcome::NotLanded(NotLanded::Expired) => Some(GroupStatus::NotLanded {
            attempts: tx_report.attempts,
        }),
        Outcome::Unknown { reason } => Some(GroupStatus::Unknown {
            reason: reason.clone(),
            signatures: execution
                .attempts
                .iter()
                .filter(|attempt| attempt.sent)
                .map(|attempt| attempt.signature)
                .collect(),
        }),
    };
    if let Some(status) = status {
        report.set(&batch.group_ids, status);
    }
    tx_report.execution = Some(execution);
    report.transactions.push(tx_report);
    split
}

async fn build(
    executor: &TransactionExecutor,
    payer: &Pubkey,
    batch: &Batch,
    budget: &Budget,
    signers: &[Arc<dyn Signer + Send + Sync>],
    version: MessageVersion,
) -> anyhow::Result<VersionedTransaction> {
    let blockhash = executor.latest_blockhash().await?;
    let mut transaction = compile(payer, &batch.instructions, blockhash, version, budget)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let required = usize::from(transaction.message.header().num_required_signatures);
    let required_keys = transaction.message.static_account_keys()[..required].to_vec();
    let own: Vec<&dyn Signer> = signers
        .iter()
        .map(|signer| signer.as_ref() as &dyn Signer)
        .filter(|signer| required_keys.contains(&signer.pubkey()))
        .collect();
    sign(&mut transaction, &own).map_err(|error| anyhow::anyhow!("{error}"))?;
    if let Some(slot) = transaction
        .signatures
        .iter()
        .position(|signature| *signature == Signature::default())
    {
        anyhow::bail!("No signer given for {}", required_keys[slot]);
    }
    Ok(transaction)
}

fn writable_accounts(payer: &Pubkey, batch: &Batch) -> Vec<Pubkey> {
    let mut writable = vec![*payer];
    for account in batch
        .instructions
        .iter()
        .flat_map(|instruction| &instruction.accounts)
    {
        if account.is_writable && !writable.contains(&account.pubkey) {
            writable.push(account.pubkey);
        }
    }
    writable
}
