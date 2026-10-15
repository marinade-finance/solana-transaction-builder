mod chain;

use std::sync::Arc;

use chain::Chain;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_keypair::Keypair;
use solana_message::VersionedMessage;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_system_interface::instruction::transfer;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_builder::{Group, MessageVersion, group};
use solana_transaction_builder_executor::{GroupStatus, RunConfig, RunReport, run};
use solana_transaction_executor::FeePolicy;

const ALL: [MessageVersion; 2] = [MessageVersion::Legacy, MessageVersion::V1];
const SOL: u64 = 1_000_000_000;
const AMOUNT: u64 = 1_000_000;

struct World {
    chain: Arc<Chain>,
    payer: Arc<Keypair>,
}

fn world() -> World {
    let chain = Chain::new();
    let payer = Arc::new(Keypair::new());
    chain.airdrop(&payer.pubkey(), 10 * SOL);
    World { chain, payer }
}

impl World {
    fn pay(&self, id: usize) -> (Group, Pubkey) {
        let to = Pubkey::new_unique();
        let group = group(
            id.to_string(),
            vec![transfer(&self.payer.pubkey(), &to, AMOUNT)],
        )
        .unwrap();
        (group, to)
    }

    async fn run(&self, groups: Vec<Group>, config: &RunConfig) -> RunReport {
        let signer: Arc<dyn Signer + Send + Sync> = self.payer.clone();
        run(
            &self.chain.executor(),
            &self.payer.pubkey(),
            groups,
            &[signer],
            config,
        )
        .await
    }

    fn sent(&self) -> Vec<VersionedTransaction> {
        self.chain.sent.lock().unwrap().clone()
    }
}

fn landed(report: &RunReport) -> usize {
    report
        .groups
        .iter()
        .filter(|outcome| matches!(outcome.status, GroupStatus::Landed { .. }))
        .count()
}

fn budget_instruction(transaction: &VersionedTransaction, tag: u8) -> Option<Vec<u8>> {
    let keys = transaction.message.static_account_keys();
    transaction
        .message
        .instructions()
        .iter()
        .filter(|instruction| {
            keys[usize::from(instruction.program_id_index)] == solana_compute_budget_interface::ID
        })
        .find(|instruction| instruction.data[0] == tag)
        .map(|instruction| instruction.data[1..].to_vec())
}

fn compute_unit_limit(transaction: &VersionedTransaction) -> Option<u32> {
    match &transaction.message {
        VersionedMessage::V1(message) => message.config.compute_unit_limit,
        _ => budget_instruction(transaction, 2)
            .map(|value| u32::from_le_bytes(value.try_into().unwrap())),
    }
}

/// Legacy: micro-lamports per compute unit; v1: the lamport total.
fn priority_fee(transaction: &VersionedTransaction) -> Option<u64> {
    match &transaction.message {
        VersionedMessage::V1(message) => message.config.priority_fee,
        _ => budget_instruction(transaction, 3)
            .map(|value| u64::from_le_bytes(value.try_into().unwrap())),
    }
}

fn heap(transaction: &VersionedTransaction) -> Option<u32> {
    match &transaction.message {
        VersionedMessage::V1(message) => message.config.heap_size,
        _ => budget_instruction(transaction, 1)
            .map(|value| u32::from_le_bytes(value.try_into().unwrap())),
    }
}

#[tokio::test]
async fn three_groups_land_in_one_transaction() {
    for version in ALL {
        let world = world();
        let (groups, recipients): (Vec<Group>, Vec<Pubkey>) =
            (0..3).map(|id| world.pay(id)).unzip();

        let report = world.run(groups, &RunConfig::new(version)).await;

        assert_eq!(landed(&report), 3, "{version:?} {report:?}");
        assert_eq!(report.transactions.len(), 1, "{version:?}");
        assert_eq!(world.sent().len(), 1, "{version:?}");
        for recipient in recipients {
            assert_eq!(world.chain.balance(&recipient), AMOUNT, "{version:?}");
        }
    }
}

#[tokio::test]
async fn a_group_that_fails_alone_is_reported_and_the_rest_land() {
    for version in ALL {
        let world = world();
        let poor = Keypair::new();
        let broke = group(
            "broke",
            vec![transfer(&poor.pubkey(), &Pubkey::new_unique(), AMOUNT)],
        )
        .unwrap();
        let groups = vec![world.pay(0).0, broke, world.pay(1).0];

        let report = world.run(groups, &RunConfig::new(version)).await;

        let failed = report
            .groups
            .iter()
            .find(|outcome| outcome.id == "broke")
            .unwrap();
        assert!(
            matches!(failed.status, GroupStatus::Failed { .. }),
            "{version:?}"
        );
        assert_eq!(landed(&report), 2, "{version:?} {report:?}");
        assert_eq!(
            report.transactions[0].group_ids,
            vec!["0", "1"],
            "{version:?}"
        );
    }
}

#[tokio::test]
async fn the_compute_limit_is_the_simulated_units_with_the_margin() {
    for version in ALL {
        let world = world();
        let config = RunConfig::new(version);

        let report = world
            .run(vec![world.pay(0).0, world.pay(1).0], &config)
            .await;

        let sent = &report.transactions[0];
        assert!(sent.compute_units_consumed > 0, "{version:?}");
        assert_eq!(
            u64::from(sent.compute_unit_limit),
            config.limits.with_margin(sent.compute_units_consumed),
            "{version:?}"
        );
        assert!(sent.compute_unit_limit <= config.limits.max_compute_units);
        assert_eq!(
            compute_unit_limit(&world.sent()[0]),
            Some(sent.compute_unit_limit)
        );
    }
}

#[tokio::test]
async fn the_default_first_send_pays_no_priority_fee() {
    for version in ALL {
        let world = world();

        let report = world
            .run(vec![world.pay(0).0], &RunConfig::new(version))
            .await;

        assert_eq!(landed(&report), 1, "{version:?}");
        assert_eq!(priority_fee(&world.sent()[0]), None, "{version:?}");
        assert_eq!(report.transactions[0].priority_fee_lamports, 0);
    }
}

#[tokio::test]
async fn a_set_start_rate_is_paid_in_the_versions_unit() {
    for version in ALL {
        let world = world();
        let config = RunConfig {
            fee: FeePolicy {
                start_micro_lamports_per_cu: 5_000,
                ..FeePolicy::default()
            },
            ..RunConfig::new(version)
        };

        let report = world.run(vec![world.pay(0).0], &config).await;

        assert_eq!(landed(&report), 1, "{version:?} {report:?}");
        let sent = &report.transactions[0];
        let expected = match version {
            MessageVersion::Legacy => 5_000,
            MessageVersion::V1 => (u64::from(sent.compute_unit_limit) * 5_000).div_ceil(1_000_000),
        };
        assert_eq!(
            priority_fee(&world.sent()[0]),
            Some(expected),
            "{version:?}"
        );
        assert_eq!(
            sent.priority_fee_lamports,
            (u64::from(sent.compute_unit_limit) * 5_000).div_ceil(1_000_000)
        );
    }
}

#[tokio::test]
async fn a_heap_request_lands_with_its_heap() {
    for version in ALL {
        let world = world();
        let heavy = group(
            "heavy",
            vec![
                ComputeBudgetInstruction::request_heap_frame(64 * 1024),
                transfer(&world.payer.pubkey(), &Pubkey::new_unique(), AMOUNT),
            ],
        )
        .unwrap();

        let report = world
            .run(vec![heavy, world.pay(1).0], &RunConfig::new(version))
            .await;

        assert_eq!(landed(&report), 2, "{version:?} {report:?}");
        assert_eq!(heap(&world.sent()[0]), Some(64 * 1024), "{version:?}");
    }
}

#[tokio::test]
async fn a_dry_run_plans_and_sends_nothing() {
    for version in ALL {
        let world = world();
        let config = RunConfig {
            dry_run: true,
            ..RunConfig::new(version)
        };

        let report = world
            .run(vec![world.pay(0).0, world.pay(1).0], &config)
            .await;

        assert!(
            report
                .groups
                .iter()
                .all(|outcome| outcome.status == GroupStatus::Planned),
            "{version:?}"
        );
        assert_eq!(report.transactions.len(), 1);
        assert_eq!(report.transactions[0].signature, None);
        assert!(report.transactions[0].bytes > 0);
        assert!(world.sent().is_empty(), "{version:?}");
    }
}

#[tokio::test]
async fn groups_over_the_byte_cap_land_in_several_transactions() {
    for version in ALL {
        let world = world();
        let (groups, recipients): (Vec<Group>, Vec<Pubkey>) =
            (0..70).map(|id| world.pay(id)).unzip();

        let report = world.run(groups, &RunConfig::new(version)).await;

        assert_eq!(landed(&report), 70, "{version:?}");
        assert!(report.transactions.len() > 1, "{version:?}");
        assert_eq!(world.sent().len(), report.transactions.len(), "{version:?}");
        for recipient in recipients {
            assert_eq!(world.chain.balance(&recipient), AMOUNT, "{version:?}");
        }
    }
}

fn status<'a>(report: &'a RunReport, id: &str) -> &'a GroupStatus {
    &report
        .groups
        .iter()
        .find(|outcome| outcome.id == id)
        .unwrap()
        .status
}

#[tokio::test]
async fn a_batch_that_lands_with_an_error_is_retried_group_by_group() {
    for version in ALL {
        let world = world();
        let (good, first) = world.pay(0);
        let (bad, poisoned) = world.pay(1);
        let (other, last) = world.pay(2);
        *world.chain.poison.lock().unwrap() = Some(poisoned);

        let report = world
            .run(vec![good, bad, other], &RunConfig::new(version))
            .await;

        assert!(
            matches!(status(&report, "0"), GroupStatus::Landed { .. }),
            "{version:?} {report:?}"
        );
        assert!(matches!(status(&report, "2"), GroupStatus::Landed { .. }));
        assert!(
            matches!(
                status(&report, "1"),
                GroupStatus::Failed { signature: Some(_), error, .. } if error.contains("custom program error: 0x4d")
            ),
            "{version:?} {report:?}"
        );
        assert_eq!(report.groups.len(), 3, "{version:?}");
        assert_eq!(report.transactions.len(), 4, "{version:?}");
        assert_eq!(world.chain.balance(&first), AMOUNT);
        assert_eq!(world.chain.balance(&last), AMOUNT);
        assert_eq!(world.chain.balance(&poisoned), 0);
    }
}

#[tokio::test]
async fn a_single_group_that_lands_with_an_error_fails_with_its_signature() {
    for version in ALL {
        let world = world();
        let (bad, poisoned) = world.pay(0);
        *world.chain.poison.lock().unwrap() = Some(poisoned);

        let report = world.run(vec![bad], &RunConfig::new(version)).await;

        let sent = world.sent()[0].signatures[0];
        assert!(
            matches!(status(&report, "0"), GroupStatus::Failed { signature: Some(signature), .. } if *signature == sent),
            "{version:?} {report:?}"
        );
        assert_eq!(world.sent().len(), 1, "{version:?}");
    }
}

#[tokio::test]
async fn a_batch_the_chain_never_takes_did_not_land_after_every_attempt() {
    for version in ALL {
        let world = world();
        world
            .chain
            .drop_sends
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let report = world
            .run(
                vec![world.pay(0).0, world.pay(1).0],
                &RunConfig::new(version),
            )
            .await;

        assert_eq!(
            status(&report, "0"),
            &GroupStatus::NotLanded { attempts: 3 },
            "{version:?} {report:?}"
        );
        assert_eq!(
            status(&report, "1"),
            &GroupStatus::NotLanded { attempts: 3 }
        );
        assert_eq!(world.sent().len(), 3, "{version:?}");
    }
}

#[tokio::test]
async fn an_unknown_batch_is_not_retried() {
    for version in ALL {
        let world = world();
        world
            .chain
            .drop_sends
            .store(true, std::sync::atomic::Ordering::SeqCst);
        world
            .chain
            .blockhash_check_fails
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let report = world
            .run(
                vec![world.pay(0).0, world.pay(1).0],
                &RunConfig::new(version),
            )
            .await;

        let sent = world.sent()[0].signatures[0];
        for id in ["0", "1"] {
            assert!(
                matches!(status(&report, id), GroupStatus::Unknown { signatures, .. } if signatures == &vec![sent]),
                "{version:?} {report:?}"
            );
        }
        assert_eq!(world.sent().len(), 1, "{version:?}");
        assert_eq!(report.transactions.len(), 1, "{version:?}");
    }
}
