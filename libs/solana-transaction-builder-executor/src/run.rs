use std::{collections::HashMap, sync::Arc};

use futures_util::{StreamExt, stream};
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
    /// Batches sent at once; 0 counts as 1.
    pub max_in_flight: usize,
}

impl RunConfig {
    pub fn new(version: MessageVersion) -> Self {
        Self {
            version,
            limits: Limits::of(version),
            fee: FeePolicy::default(),
            dry_run: false,
            max_in_flight: 8,
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

    fn apply_sent(&mut self, ids: &[String], sent: Sent) -> bool {
        if let Some(status) = sent.status {
            self.set(ids, status);
        }
        self.transactions.extend(sent.transaction);
        sent.split
    }
}

struct Sent {
    split: bool,
    status: Option<GroupStatus>,
    transaction: Option<TxReport>,
}

fn failed(error: impl ToString, logs: Vec<String>) -> GroupStatus {
    GroupStatus::Failed {
        error: error.to_string(),
        logs,
        signature: None,
    }
}

/// Groups are packed first and each batch is simulated once. A batch that fails simulation has
/// its groups simulated alone: the failing ones are reported, the rest are packed again.
/// A batch whose failure is proven on chain goes one group per transaction.
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
    let mut signed = Vec::new();
    for group in groups {
        match missing_signer(payer, &group, &given) {
            Some(missing) => report.set(
                &[group.id().to_owned()],
                GroupStatus::NotSent {
                    reason: format!("missing signer {missing}"),
                },
            ),
            None => signed.push(group),
        }
    }
    let signed = drop_identical(signed, &mut report);
    let batches = pack_reporting(payer, &signed, config, &mut report);
    let by_id: HashMap<String, Group> = signed
        .into_iter()
        .map(|group| (group.id().to_owned(), group))
        .collect();
    let by_id = &by_id;
    let mut batch_reports: Vec<(usize, RunReport)> = stream::iter(batches.into_iter().enumerate())
        .map(|(index, batch)| async move {
            let batch_report = run_batch(executor, payer, batch, by_id, signers, config).await;
            (index, batch_report)
        })
        .buffer_unordered(config.max_in_flight.max(1))
        .collect()
        .await;
    batch_reports.sort_by_key(|(index, _)| *index);
    for (_, batch_report) in batch_reports {
        report.groups.extend(batch_report.groups);
        report.transactions.extend(batch_report.transactions);
    }
    report.groups.sort_by_key(|outcome| order[&outcome.id]);
    report
}

type InstructionsKey = Vec<(Pubkey, Vec<(Pubkey, bool, bool)>, Vec<u8>)>;

/// Parallel batches can share a blockhash, and identical instructions then are one transaction.
fn drop_identical(groups: Vec<Group>, report: &mut RunReport) -> Vec<Group> {
    let mut first: HashMap<InstructionsKey, String> = HashMap::new();
    let mut kept = Vec::new();
    for group in groups {
        let key: InstructionsKey = group
            .instructions()
            .iter()
            .map(|instruction| {
                let accounts = instruction
                    .accounts
                    .iter()
                    .map(|meta| (meta.pubkey, meta.is_signer, meta.is_writable))
                    .collect();
                (instruction.program_id, accounts, instruction.data.clone())
            })
            .collect();
        match first.get(&key) {
            Some(earlier) => report.set(
                &[group.id().to_owned()],
                GroupStatus::NotSent {
                    reason: format!("identical to group {earlier}"),
                },
            ),
            None => {
                first.insert(key, group.id().to_owned());
                kept.push(group);
            }
        }
    }
    kept
}

fn pack_reporting(
    payer: &Pubkey,
    groups: &[Group],
    config: &RunConfig,
    report: &mut RunReport,
) -> Vec<Batch> {
    let (batches, errors) = pack(payer, groups, config.version, &config.limits);
    for error in errors {
        let id = match &error {
            solana_transaction_builder::PackError::GroupTooLarge { id, .. }
            | solana_transaction_builder::PackError::Budget { id, .. }
            | solana_transaction_builder::PackError::Empty { id } => id.clone(),
        };
        report.set(&[id], failed(error, vec![]));
    }
    batches
}

async fn run_batch(
    executor: &TransactionExecutor,
    payer: &Pubkey,
    batch: Batch,
    by_id: &HashMap<String, Group>,
    signers: &[Arc<dyn Signer + Send + Sync>],
    config: &RunConfig,
) -> RunReport {
    let mut report = RunReport::default();
    match simulate(executor, payer, &batch, config).await {
        Ok(simulation) => {
            let sent = send(executor, payer, &batch, &simulation, signers, config, true).await;
            if report.apply_sent(&batch.group_ids, sent) {
                info!("Sending the groups of {:?} one by one", batch.group_ids);
                for id in &batch.group_ids {
                    let single = Batch::from(&by_id[id]);
                    match simulate(executor, payer, &single, config).await {
                        Ok(simulation) => {
                            let sent =
                                send(executor, payer, &single, &simulation, signers, config, true)
                                    .await;
                            report.apply_sent(&single.group_ids, sent);
                        }
                        Err(status) => report.set(&single.group_ids, status),
                    }
                }
            }
        }
        Err(GroupStatus::Failed { error, .. }) if batch.group_ids.len() > 1 => {
            warn!(
                "Batch {:?} failed in simulation ({error}); simulating its groups alone",
                batch.group_ids
            );
            isolate(executor, payer, &batch, by_id, signers, config, &mut report).await;
        }
        Err(status) => report.set(&batch.group_ids, status),
    }
    report
}

async fn isolate(
    executor: &TransactionExecutor,
    payer: &Pubkey,
    batch: &Batch,
    by_id: &HashMap<String, Group>,
    signers: &[Arc<dyn Signer + Send + Sync>],
    config: &RunConfig,
    report: &mut RunReport,
) {
    let mut passing = Vec::new();
    let mut held = HashMap::new();
    for id in &batch.group_ids {
        let group = &by_id[id];
        match simulate(executor, payer, &Batch::from(group), config).await {
            Ok(simulation) => {
                let units = simulation
                    .units_consumed
                    .and_then(|units| u32::try_from(units).ok());
                passing.push(group.clone().with_compute_units(units));
                held.insert(id.clone(), simulation);
            }
            Err(status) => report.set(std::slice::from_ref(id), status),
        }
    }
    let send_alone = async |id: &String, report: &mut RunReport| {
        let single = Batch::from(&by_id[id]);
        let sent = send(executor, payer, &single, &held[id], signers, config, false).await;
        report.apply_sent(&single.group_ids, sent);
    };
    for repacked in pack_reporting(payer, &passing, config, report) {
        if let [id] = repacked.group_ids.as_slice() {
            send_alone(id, report).await;
            continue;
        }
        let split = match simulate(executor, payer, &repacked, config).await {
            Ok(simulation) => {
                let sent = send(
                    executor,
                    payer,
                    &repacked,
                    &simulation,
                    signers,
                    config,
                    true,
                )
                .await;
                report.apply_sent(&repacked.group_ids, sent)
            }
            Err(GroupStatus::Failed { error, .. }) => {
                warn!(
                    "Batch {:?} failed together in simulation ({error})",
                    repacked.group_ids
                );
                true
            }
            Err(status) => {
                report.set(&repacked.group_ids, status);
                false
            }
        };
        if split {
            info!("Sending the groups of {:?} one by one", repacked.group_ids);
            for id in &repacked.group_ids {
                send_alone(id, report).await;
            }
        }
    }
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

/// The batch's status, or `split` to send its groups alone; `fresh`: `simulation` is still current.
async fn send(
    executor: &TransactionExecutor,
    payer: &Pubkey,
    batch: &Batch,
    simulation: &Simulation,
    signers: &[Arc<dyn Signer + Send + Sync>],
    config: &RunConfig,
    fresh: bool,
) -> Sent {
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
            return Sent {
                split: false,
                status: Some(failed(error, vec![])),
                transaction: None,
            };
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
        return Sent {
            split: false,
            status: Some(GroupStatus::Planned),
            transaction: Some(tx_report),
        };
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
                built == 0 && fresh,
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
    tx_report.execution = Some(execution);
    Sent {
        split,
        status,
        transaction: Some(tx_report),
    }
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
    simulated: bool,
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
        simulated,
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
