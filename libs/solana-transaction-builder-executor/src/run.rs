use std::{collections::HashMap, sync::Arc};

use futures_util::stream;
use log::{info, warn};
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction_builder::{
    Batch, Budget, DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT, Group, Limits, MessageVersion, compile,
    pack, sign, widest_budget, wire_len,
};
use solana_transaction_executor::{
    AttemptTransaction, Execution, FeePolicy, NotLanded, Outcome, Simulation, TransactionExecutor,
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
    /// No attempt left the process; safe to plan again.
    NotSent {
        reason: String,
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

fn failed(error: impl ToString, logs: Vec<String>) -> GroupStatus {
    GroupStatus::Failed {
        error: error.to_string(),
        logs,
        signature: None,
    }
}

/// Groups must be independent: each is simulated alone against the state before the run.
/// A batch whose failure is proven, in simulation or on chain, goes one group per transaction.
/// A repeated group id refuses the whole run; the report keeps the order of `groups`.
pub async fn run(
    executor: &TransactionExecutor,
    payer: &Pubkey,
    groups: Vec<Group>,
    signers: &[Arc<dyn Signer + Send + Sync>],
    config: &RunConfig,
) -> RunReport {
    let mut report = RunReport::default();
    let mut order = HashMap::new();
    for (index, group) in groups.iter().enumerate() {
        if order.insert(group.id().to_owned(), index).is_some() {
            let reason = format!("repeated group id {}", group.id());
            let ids: Vec<String> = groups.iter().map(|group| group.id().to_owned()).collect();
            report.set(&ids, GroupStatus::NotSent { reason });
            return report;
        }
    }
    let given: Vec<Pubkey> = signers.iter().map(|signer| signer.pubkey()).collect();
    let mut passing = Vec::new();
    for group in groups {
        if let Some(missing) = missing_signer(payer, &group, &given) {
            report.set(
                &[group.id().to_owned()],
                GroupStatus::NotSent {
                    reason: format!("missing signer {missing}"),
                },
            );
            continue;
        }
        match simulate(executor, payer, &Batch::from(&group), config).await {
            Ok(simulation) => passing.push(
                group.with_compute_units(
                    simulation
                        .units_consumed
                        .and_then(|units| u32::try_from(units).ok()),
                ),
            ),
            Err(status) => report.set(&[group.id().to_owned()], status),
        }
    }
    let (batches, errors) = pack(payer, &passing, config.version, &config.limits);
    for error in errors {
        let id = match &error {
            solana_transaction_builder::PackError::GroupTooLarge { id, .. }
            | solana_transaction_builder::PackError::Budget { id, .. }
            | solana_transaction_builder::PackError::Empty { id } => id.clone(),
        };
        report.set(&[id], failed(error, vec![]));
    }
    let by_id: HashMap<String, Group> = passing
        .into_iter()
        .map(|group| (group.id().to_owned(), group))
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
            Err(GroupStatus::Failed { error, .. }) if batch.group_ids.len() > 1 => {
                warn!(
                    "Batch {:?} failed together in simulation ({error})",
                    batch.group_ids
                );
                true
            }
            Err(status) => {
                report.set(&batch.group_ids, status);
                false
            }
        };
        if !split {
            continue;
        }
        info!("Sending the groups of {:?} one by one", batch.group_ids);
        for id in &batch.group_ids {
            let single = Batch::from(&by_id[id]);
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
                Err(status) => report.set(&single.group_ids, status),
            }
        }
    }
    report.groups.sort_by_key(|outcome| order[&outcome.id]);
    report
}

fn missing_signer(payer: &Pubkey, group: &Group, given: &[Pubkey]) -> Option<Pubkey> {
    std::iter::once(payer)
        .chain(
            group
                .instructions()
                .iter()
                .flat_map(|instruction| &instruction.accounts)
                .filter(|account| account.is_signer)
                .map(|account| &account.pubkey),
        )
        .find(|key| !given.contains(key))
        .copied()
}

async fn simulate(
    executor: &TransactionExecutor,
    payer: &Pubkey,
    batch: &Batch,
    config: &RunConfig,
) -> Result<Simulation, GroupStatus> {
    let transaction = compile(
        payer,
        &batch.instructions,
        Hash::default(),
        config.version,
        &widest_budget(&config.limits, batch.heap_bytes),
    )
    .map_err(|error| failed(error, vec![]))?;
    let simulation = executor.simulate(&transaction).await.map_err(|error| {
        warn!("Simulation of {:?} could not run: {error}", batch.group_ids);
        GroupStatus::NotSent {
            reason: "Simulation could not run".into(),
        }
    })?;
    match simulation.error {
        Some(error) => Err(failed(error, simulation.logs)),
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
    let compute_unit_limit =
        simulation
            .units_consumed
            .map_or(limits.max_compute_units, |consumed| {
                u32::try_from(limits.with_margin(consumed))
                    .unwrap_or(u32::MAX)
                    .min(limits.max_compute_units)
            });
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
    let writable = writable_accounts(payer, batch);
    let first = executor.first_rate(&config.fee, &writable).await;
    let planned = match compile(
        payer,
        &batch.instructions,
        Hash::default(),
        config.version,
        &budget(first),
    ) {
        Ok(transaction) => transaction,
        Err(error) => {
            report.set(&batch.group_ids, failed(error, vec![]));
            return false;
        }
    };
    let mut tx_report = TxReport {
        group_ids: batch.group_ids.clone(),
        bytes: wire_len(&planned),
        addresses: planned.message.static_account_keys().len(),
        compute_units_consumed: simulation.units_consumed.unwrap_or(0),
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

    let budget_ref = &budget;
    let writable_ref = &writable;
    let max_attempts = executor.transaction_executor_config.max_attempts;
    let attempts_stream = stream::unfold(
        (0, None),
        |(built, previous): (u32, Option<u64>)| async move {
            if built >= max_attempts {
                return None;
            }
            let rate = match previous {
                None => first,
                Some(previous) => {
                    executor
                        .next_rate(&config.fee, previous, writable_ref)
                        .await
                }
            };
            let attempt = build(
                executor,
                payer,
                batch,
                &budget_ref(rate),
                signers,
                config.version,
            )
            .await;
            Some((attempt, (built + 1, Some(rate))))
        },
    );
    let execution = executor.execute_transaction(attempts_stream).await;
    tx_report.attempts = u32::try_from(execution.attempts.len()).unwrap_or(u32::MAX);
    let alone = batch.group_ids.len() == 1;
    let sent = execution.attempts.iter().any(|attempt| attempt.sent);
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
            // Without the expiry wait, an earlier attempt may still be live and land after a split.
            let waited = executor
                .transaction_executor_config
                .wait_for_blockhash_expiry_before_retry;
            split = !alone && waited;
            let signatures = sent_signatures(&execution);
            if split {
                None
            } else if !waited && signatures.len() > 1 {
                Some(GroupStatus::Unknown {
                    reason: format!("{signature} failed with {error}; an earlier attempt may land"),
                    signatures,
                })
            } else {
                Some(GroupStatus::Failed {
                    error: error.to_string(),
                    logs: vec![],
                    signature: Some(*signature),
                })
            }
        }
        Outcome::NotLanded(NotLanded::Refused { error, logs }) => {
            split = !alone;
            alone.then(|| failed(error, logs.clone()))
        }
        Outcome::NotLanded(NotLanded::NotSent { error }) if !sent => {
            split = !alone;
            alone.then(|| GroupStatus::NotSent {
                reason: error.clone(),
            })
        }
        Outcome::NotLanded(NotLanded::NotSent { .. } | NotLanded::Expired) => {
            Some(GroupStatus::NotLanded {
                attempts: tx_report.attempts,
            })
        }
        Outcome::Unknown { reason } => Some(GroupStatus::Unknown {
            reason: reason.clone(),
            signatures: sent_signatures(&execution),
        }),
    };
    if let Some(status) = status {
        report.set(&batch.group_ids, status);
    }
    tx_report.execution = Some(execution);
    report.transactions.push(tx_report);
    split
}

fn sent_signatures(execution: &Execution) -> Vec<Signature> {
    execution
        .attempts
        .iter()
        .filter(|attempt| attempt.sent)
        .map(|attempt| attempt.signature)
        .collect()
}

async fn build(
    executor: &TransactionExecutor,
    payer: &Pubkey,
    batch: &Batch,
    budget: &Budget,
    signers: &[Arc<dyn Signer + Send + Sync>],
    version: MessageVersion,
) -> anyhow::Result<AttemptTransaction> {
    let (blockhash, last_valid_block_height) = executor.latest_blockhash().await?;
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
    Ok(AttemptTransaction {
        transaction,
        last_valid_block_height,
        sent_before: false,
    })
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
