use std::collections::HashSet;

use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_message::{
    CompileError, Message as LegacyMessage, VersionedMessage,
    v1::{self, MessageError, TransactionConfig},
};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

use crate::{
    Budget, DEFAULT_COMPUTE_UNIT_LIMIT, DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT, Limits,
    MessageVersion, valid_heap,
};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum TxError {
    #[error("Invalid budget: {0}")]
    Budget(String),
    #[error("The instructions do not compile: {0}")]
    Compile(#[from] CompileError),
    #[error("Invalid v1 message: {0:?}")]
    Invalid(MessageError),
    #[error("The transaction names {count} addresses, over the cap of {max}")]
    TooManyAddresses { count: usize, max: usize },
    #[error("The transaction is {bytes} bytes, over the cap of {max}")]
    TooLarge { bytes: usize, max: usize },
    #[error("Malformed transaction: {0}")]
    Malformed(String),
    #[error("{0} is not a signer of the transaction")]
    NotASigner(Pubkey),
    #[error("{signer} failed to sign: {message}")]
    Signing { signer: Pubkey, message: String },
}

/// Default signature slots make the unsigned size the signed size, so caps hold before signing.
pub fn compile(
    payer: &Pubkey,
    instructions: &[Instruction],
    blockhash: Hash,
    version: MessageVersion,
    budget: &Budget,
) -> Result<VersionedTransaction, TxError> {
    if instructions
        .iter()
        .any(|instruction| instruction.program_id == solana_compute_budget_interface::ID)
    {
        return Err(TxError::Budget(
            "a Compute Budget instruction goes into the Budget, not the instructions".into(),
        ));
    }
    if let Some(heap) = budget.heap_bytes
        && !valid_heap(heap)
    {
        return Err(TxError::Budget(format!(
            "a heap of {heap} bytes is not a multiple of 1024 between 32 KiB and 256 KiB"
        )));
    }
    let limits = Limits::of(version);
    let message = match version {
        MessageVersion::Legacy => {
            let instructions: Vec<Instruction> = legacy_budget(budget)
                .into_iter()
                .chain(instructions.iter().cloned())
                .collect();
            let count = instructions
                .iter()
                .flat_map(|instruction| {
                    std::iter::once(&instruction.program_id)
                        .chain(instruction.accounts.iter().map(|account| &account.pubkey))
                })
                .chain([payer])
                .collect::<HashSet<_>>()
                .len();
            // The legacy compiler panics once its u8 key indexes overflow.
            if count > limits.max_addresses {
                return Err(TxError::TooManyAddresses {
                    count,
                    max: limits.max_addresses,
                });
            }
            VersionedMessage::Legacy(LegacyMessage::new_with_blockhash(
                &instructions,
                Some(payer),
                &blockhash,
            ))
        }
        MessageVersion::V1 => VersionedMessage::V1(v1::Message::try_compile_with_config(
            payer,
            instructions,
            blockhash,
            v1_config(budget),
        )?),
    };
    let count = message.static_account_keys().len();
    if count > limits.max_addresses {
        return Err(TxError::TooManyAddresses {
            count,
            max: limits.max_addresses,
        });
    }
    if let VersionedMessage::V1(message) = &message {
        message.validate().map_err(TxError::Invalid)?;
    }
    let transaction = VersionedTransaction {
        signatures: vec![
            Signature::default();
            usize::from(message.header().num_required_signatures)
        ],
        message,
    };
    let bytes = wire_len(&transaction);
    if bytes > limits.max_bytes {
        return Err(TxError::TooLarge {
            bytes,
            max: limits.max_bytes,
        });
    }
    Ok(transaction)
}

pub fn wire_len(transaction: &VersionedTransaction) -> usize {
    wincode::serialized_size(transaction)
        .ok()
        .and_then(|bytes| usize::try_from(bytes).ok())
        .unwrap_or(usize::MAX)
}

/// Fills the slots of `signers` only; the other required signers sign elsewhere.
pub fn sign(
    transaction: &mut VersionedTransaction,
    signers: &[&dyn Signer],
) -> Result<(), TxError> {
    let message = transaction.message.serialize();
    let required = usize::from(transaction.message.header().num_required_signatures);
    let keys = transaction.message.static_account_keys();
    if required > keys.len() || transaction.signatures.len() != required {
        return Err(TxError::Malformed(format!(
            "{} signature slots and {} keys for {required} required signers",
            transaction.signatures.len(),
            keys.len()
        )));
    }
    for signer in signers {
        let key = signer.pubkey();
        let slot = keys[..required]
            .iter()
            .position(|required| *required == key)
            .ok_or(TxError::NotASigner(key))?;
        transaction.signatures[slot] =
            signer
                .try_sign_message(&message)
                .map_err(|error| TxError::Signing {
                    signer: key,
                    message: error.to_string(),
                })?;
    }
    Ok(())
}

fn legacy_budget(budget: &Budget) -> Vec<Instruction> {
    let mut instructions = Vec::new();
    if let Some(limit) = budget.compute_unit_limit {
        instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(limit));
    }
    if let Some(rate) = budget.micro_lamports_per_cu.filter(|rate| *rate > 0) {
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(rate));
    }
    if let Some(limit) = budget.loaded_accounts_data_size_limit {
        instructions.push(ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(limit));
    }
    if let Some(heap) = budget.heap_bytes {
        instructions.push(ComputeBudgetInstruction::request_heap_frame(heap));
    }
    instructions
}

fn v1_config(budget: &Budget) -> TransactionConfig {
    let mut config = TransactionConfig::empty()
        .with_compute_unit_limit(
            budget
                .compute_unit_limit
                .unwrap_or(DEFAULT_COMPUTE_UNIT_LIMIT),
        )
        .with_loaded_accounts_data_size_limit(
            budget
                .loaded_accounts_data_size_limit
                .unwrap_or(DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT),
        );
    if let Some(heap) = budget.heap_bytes {
        config = config.with_heap_size(heap);
    }
    let fee = budget.priority_fee_lamports();
    if fee > 0 {
        config = config.with_priority_fee(fee);
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_instruction::AccountMeta;
    use solana_keypair::Keypair;
    use solana_system_interface::instruction::transfer;

    const ALL: [MessageVersion; 2] = [MessageVersion::Legacy, MessageVersion::V1];

    fn full_budget() -> Budget {
        Budget {
            compute_unit_limit: Some(200_000),
            micro_lamports_per_cu: Some(10_000),
            loaded_accounts_data_size_limit: Some(1_000_000),
            heap_bytes: Some(256 * 1024),
        }
    }

    fn noop(accounts: usize, data: usize) -> Instruction {
        Instruction {
            program_id: Pubkey::new_unique(),
            accounts: (0..accounts)
                .map(|_| AccountMeta::new_readonly(Pubkey::new_unique(), false))
                .collect(),
            data: vec![7; data],
        }
    }

    fn program_ids(transaction: &VersionedTransaction) -> Vec<Pubkey> {
        let keys = transaction.message.static_account_keys();
        transaction
            .message
            .instructions()
            .iter()
            .map(|instruction| keys[usize::from(instruction.program_id_index)])
            .collect()
    }

    fn config(transaction: &VersionedTransaction) -> TransactionConfig {
        match &transaction.message {
            VersionedMessage::V1(message) => message.config,
            other => panic!("expected a v1 message, got {other:?}"),
        }
    }

    #[test]
    fn the_unsigned_size_is_the_signed_size() {
        let payer = Keypair::new();
        let pay = transfer(&payer.pubkey(), &Pubkey::new_unique(), 1);
        for version in ALL {
            let mut transaction = compile(
                &payer.pubkey(),
                std::slice::from_ref(&pay),
                Hash::default(),
                version,
                &full_budget(),
            )
            .unwrap();
            let unsigned = wire_len(&transaction);
            sign(&mut transaction, &[&payer]).unwrap();
            assert_eq!(
                wincode::serialize(&transaction).unwrap().len(),
                unsigned,
                "{version:?}"
            );
        }
    }

    #[test]
    fn a_legacy_budget_is_four_compute_budget_instructions_ahead_of_the_callers() {
        let payer = Pubkey::new_unique();
        let pay = transfer(&payer, &Pubkey::new_unique(), 1);
        let transaction = compile(
            &payer,
            std::slice::from_ref(&pay),
            Hash::default(),
            MessageVersion::Legacy,
            &full_budget(),
        )
        .unwrap();

        assert_eq!(
            program_ids(&transaction),
            vec![
                solana_compute_budget_interface::ID,
                solana_compute_budget_interface::ID,
                solana_compute_budget_interface::ID,
                solana_compute_budget_interface::ID,
                pay.program_id,
            ]
        );
        let data: Vec<Vec<u8>> = transaction
            .message
            .instructions()
            .iter()
            .take(4)
            .map(|instruction| instruction.data.clone())
            .collect();
        assert_eq!(
            data,
            vec![
                ComputeBudgetInstruction::set_compute_unit_limit(200_000).data,
                ComputeBudgetInstruction::set_compute_unit_price(10_000).data,
                ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(1_000_000).data,
                ComputeBudgetInstruction::request_heap_frame(256 * 1024).data,
            ]
        );
    }

    #[test]
    fn a_legacy_budget_without_a_rate_carries_no_price() {
        let payer = Pubkey::new_unique();
        let transaction = compile(
            &payer,
            &[transfer(&payer, &Pubkey::new_unique(), 1)],
            Hash::default(),
            MessageVersion::Legacy,
            &Budget {
                micro_lamports_per_cu: Some(0),
                ..Budget::default()
            },
        )
        .unwrap();
        assert!(!program_ids(&transaction).contains(&solana_compute_budget_interface::ID));
    }

    #[test]
    fn a_v1_budget_is_the_message_config_and_no_instruction() {
        let payer = Pubkey::new_unique();
        let transaction = compile(
            &payer,
            &[transfer(&payer, &Pubkey::new_unique(), 1)],
            Hash::default(),
            MessageVersion::V1,
            &full_budget(),
        )
        .unwrap();

        let config = config(&transaction);
        assert_eq!(config.compute_unit_limit, Some(200_000));
        assert_eq!(config.priority_fee, Some(2_000));
        assert_eq!(config.loaded_accounts_data_size_limit, Some(1_000_000));
        assert_eq!(config.heap_size, Some(256 * 1024));
        assert!(
            !transaction
                .message
                .static_account_keys()
                .contains(&solana_compute_budget_interface::ID)
        );
    }

    #[test]
    fn a_v1_fee_is_the_rate_times_the_limit_rounded_up() {
        let budget = Budget {
            compute_unit_limit: Some(150_001),
            micro_lamports_per_cu: Some(3),
            ..Budget::default()
        };
        assert_eq!(budget.priority_fee_lamports(), 1);
        assert_eq!(Budget::default().priority_fee_lamports(), 0);
    }

    #[test]
    fn a_v1_transaction_always_carries_a_compute_and_a_loaded_accounts_limit() {
        let payer = Pubkey::new_unique();
        let transaction = compile(
            &payer,
            &[transfer(&payer, &Pubkey::new_unique(), 1)],
            Hash::default(),
            MessageVersion::V1,
            &Budget::default(),
        )
        .unwrap();

        let config = config(&transaction);
        assert_eq!(config.compute_unit_limit, Some(DEFAULT_COMPUTE_UNIT_LIMIT));
        assert_eq!(
            config.loaded_accounts_data_size_limit,
            Some(DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT)
        );
        assert_eq!(config.priority_fee, None);
        assert_eq!(config.heap_size, None);
    }

    #[test]
    fn a_heap_off_the_kib_grid_or_out_of_range_is_refused() {
        let payer = Pubkey::new_unique();
        for version in ALL {
            for heap in [33_000, 16 * 1024, 257 * 1024] {
                let result = compile(
                    &payer,
                    &[transfer(&payer, &Pubkey::new_unique(), 1)],
                    Hash::default(),
                    version,
                    &Budget {
                        heap_bytes: Some(heap),
                        ..Budget::default()
                    },
                );
                assert!(
                    matches!(result, Err(TxError::Budget(_))),
                    "{version:?} {heap}"
                );
            }
        }
    }

    #[test]
    fn a_compute_budget_instruction_among_the_callers_is_refused() {
        let payer = Pubkey::new_unique();
        for version in ALL {
            let result = compile(
                &payer,
                &[ComputeBudgetInstruction::set_compute_unit_limit(1)],
                Hash::default(),
                version,
                &Budget::default(),
            );
            assert!(matches!(result, Err(TxError::Budget(_))), "{version:?}");
        }
    }

    #[test]
    fn a_transaction_over_its_address_cap_is_refused() {
        let payer = Pubkey::new_unique();
        let v1 = compile(
            &payer,
            &[noop(64, 0)],
            Hash::default(),
            MessageVersion::V1,
            &Budget::default(),
        );
        assert_eq!(v1, Err(TxError::TooManyAddresses { count: 66, max: 64 }));

        let legacy = compile(
            &payer,
            &[noop(127, 0)],
            Hash::default(),
            MessageVersion::Legacy,
            &Budget::default(),
        );
        assert_eq!(
            legacy,
            Err(TxError::TooManyAddresses {
                count: 129,
                max: 128
            })
        );
    }

    #[test]
    fn a_legacy_account_set_over_the_key_index_range_is_refused_not_a_panic() {
        let payer = Pubkey::new_unique();
        let legacy = compile(
            &payer,
            &[noop(300, 0)],
            Hash::default(),
            MessageVersion::Legacy,
            &Budget::default(),
        );
        assert_eq!(
            legacy,
            Err(TxError::TooManyAddresses {
                count: 302,
                max: 128
            })
        );
    }

    #[test]
    fn signing_a_malformed_transaction_is_an_error_not_a_panic() {
        let payer = Keypair::new();
        let compiled = compile(
            &payer.pubkey(),
            &[transfer(&payer.pubkey(), &Pubkey::new_unique(), 1)],
            Hash::default(),
            MessageVersion::Legacy,
            &Budget::default(),
        )
        .unwrap();

        let mut no_slots = compiled.clone();
        no_slots.signatures.clear();
        assert!(matches!(
            sign(&mut no_slots, &[&payer]),
            Err(TxError::Malformed(_))
        ));

        let mut overclaimed = compiled;
        let VersionedMessage::Legacy(message) = &mut overclaimed.message else {
            unreachable!()
        };
        message.header.num_required_signatures = 9;
        overclaimed.signatures = vec![Signature::default(); 9];
        assert!(matches!(
            sign(&mut overclaimed, &[&payer]),
            Err(TxError::Malformed(_))
        ));
    }

    #[test]
    fn a_transaction_over_its_byte_cap_is_refused() {
        let payer = Pubkey::new_unique();
        let v1 = compile(
            &payer,
            &[noop(0, 4_096)],
            Hash::default(),
            MessageVersion::V1,
            &Budget::default(),
        );
        assert!(
            matches!(v1, Err(TxError::TooLarge { max: 4_096, .. })),
            "{v1:?}"
        );

        let legacy = compile(
            &payer,
            &[noop(0, 1_232)],
            Hash::default(),
            MessageVersion::Legacy,
            &Budget::default(),
        );
        assert!(
            matches!(legacy, Err(TxError::TooLarge { max: 1_232, .. })),
            "{legacy:?}"
        );
    }

    #[test]
    fn signing_fills_only_the_given_signers_slots() {
        let payer = Keypair::new();
        let other = Keypair::new();
        let stranger = Keypair::new();
        let instructions = [
            transfer(&payer.pubkey(), &Pubkey::new_unique(), 1),
            transfer(&other.pubkey(), &Pubkey::new_unique(), 1),
        ];
        for version in ALL {
            let mut transaction = compile(
                &payer.pubkey(),
                &instructions,
                Hash::default(),
                version,
                &Budget::default(),
            )
            .unwrap();

            assert_eq!(
                sign(&mut transaction, &[&stranger]),
                Err(TxError::NotASigner(stranger.pubkey()))
            );
            sign(&mut transaction, &[&payer]).unwrap();

            let message = transaction.message.serialize();
            assert!(transaction.signatures[0].verify(payer.pubkey().as_ref(), &message));
            assert_eq!(
                transaction.signatures[1],
                Signature::default(),
                "{version:?}"
            );
        }
    }
}
