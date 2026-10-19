use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use crate::{Limits, MessageVersion, TxError, compile, valid_heap, widest_budget, wire_len};

const REQUEST_HEAP_FRAME: u8 = 1;
const SET_COMPUTE_UNIT_LIMIT: u8 = 2;
const SET_COMPUTE_UNIT_PRICE: u8 = 3;
const SET_LOADED_ACCOUNTS_DATA_SIZE_LIMIT: u8 = 4;

/// Instructions that land in one transaction or not at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub instructions: Vec<Instruction>,
    pub heap_bytes: Option<u32>,
    pub compute_units: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    pub group_ids: Vec<String>,
    pub instructions: Vec<Instruction>,
    pub heap_bytes: Option<u32>,
    pub compute_units: u64,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum PackError {
    #[error("Group {id} does not fit a transaction alone: {reason}")]
    GroupTooLarge { id: String, reason: String },
    #[error("Group {id} has an unusable Compute Budget instruction: {reason}")]
    Budget { id: String, reason: String },
    #[error("Group {id} has no instruction besides Compute Budget ones")]
    Empty { id: String },
}

/// A group's own limit and price are dropped: simulation and the fee policy set them per batch.
pub fn group(id: impl Into<String>, instructions: Vec<Instruction>) -> Result<Group, PackError> {
    let id = id.into();
    let refuse = |reason: String| PackError::Budget {
        id: id.clone(),
        reason,
    };
    let mut kept = Vec::with_capacity(instructions.len());
    let mut heap_bytes = None;
    let mut seen = Vec::new();
    for instruction in instructions {
        if instruction.program_id != solana_compute_budget_interface::ID {
            kept.push(instruction);
            continue;
        }
        let Some((&tag, value)) = instruction.data.split_first() else {
            return Err(refuse("an empty Compute Budget instruction".into()));
        };
        if seen.contains(&tag) {
            return Err(refuse(format!(
                "Compute Budget instruction {tag} appears twice"
            )));
        }
        seen.push(tag);
        let expected = match tag {
            REQUEST_HEAP_FRAME | SET_COMPUTE_UNIT_LIMIT | SET_LOADED_ACCOUNTS_DATA_SIZE_LIMIT => 4,
            SET_COMPUTE_UNIT_PRICE => 8,
            _ => return Err(refuse(format!("unknown Compute Budget instruction {tag}"))),
        };
        if value.len() != expected {
            return Err(refuse(format!(
                "Compute Budget instruction {tag} carries {} bytes, not {expected}",
                value.len()
            )));
        }
        if tag == REQUEST_HEAP_FRAME {
            let bytes = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
            if !valid_heap(bytes) {
                return Err(refuse(format!("a heap of {bytes} bytes is not valid")));
            }
            heap_bytes = Some(bytes);
        }
    }
    if kept.is_empty() {
        return Err(PackError::Empty { id });
    }
    Ok(Group {
        id,
        instructions: kept,
        heap_bytes,
        compute_units: None,
    })
}

/// Greedy and order-keeping; a group too large alone becomes an error and the rest still pack.
pub fn pack(
    payer: &Pubkey,
    groups: &[Group],
    version: MessageVersion,
    limits: &Limits,
) -> (Vec<Batch>, Vec<PackError>) {
    let mut batches = Vec::new();
    let mut errors = Vec::new();
    let mut current: Option<Batch> = None;
    for group in groups {
        if let Some(batch) = &current {
            let joined = join(batch, group);
            if fits(payer, &joined, version, limits).is_ok() {
                current = Some(joined);
                continue;
            }
        }
        let alone = Batch::from(group);
        match fits(payer, &alone, version, limits) {
            Ok(()) => {
                if let Some(batch) = current.replace(alone) {
                    batches.push(batch);
                }
            }
            Err(reason) => errors.push(PackError::GroupTooLarge {
                id: group.id.clone(),
                reason,
            }),
        }
    }
    batches.extend(current);
    (batches, errors)
}

impl Batch {
    fn empty() -> Self {
        Self {
            group_ids: Vec::new(),
            instructions: Vec::new(),
            heap_bytes: None,
            compute_units: 0,
        }
    }
}

impl From<&Group> for Batch {
    fn from(group: &Group) -> Self {
        join(&Batch::empty(), group)
    }
}

fn join(batch: &Batch, group: &Group) -> Batch {
    Batch {
        group_ids: batch
            .group_ids
            .iter()
            .cloned()
            .chain([group.id.clone()])
            .collect(),
        instructions: batch
            .instructions
            .iter()
            .chain(&group.instructions)
            .cloned()
            .collect(),
        heap_bytes: batch.heap_bytes.max(group.heap_bytes),
        compute_units: batch.compute_units + u64::from(group.compute_units.unwrap_or(0)),
    }
}

fn fits(
    payer: &Pubkey,
    batch: &Batch,
    version: MessageVersion,
    limits: &Limits,
) -> Result<(), String> {
    let units = batch.compute_units;
    if units > u64::from(limits.max_compute_units) {
        return Err(format!(
            "{units} compute units, over the cap of {}",
            limits.max_compute_units
        ));
    }
    let transaction = compile(
        payer,
        &batch.instructions,
        Hash::default(),
        version,
        &widest_budget(limits, batch.heap_bytes),
    )
    .map_err(|error: TxError| error.to_string())?;
    let bytes = wire_len(&transaction);
    if bytes > limits.max_bytes {
        return Err(format!(
            "{bytes} bytes, over the cap of {}",
            limits.max_bytes
        ));
    }
    let addresses = transaction.message.static_account_keys().len();
    if addresses > limits.max_addresses {
        return Err(format!(
            "{addresses} addresses, over the cap of {}",
            limits.max_addresses
        ));
    }
    let instructions = transaction.message.instructions().len();
    if instructions > limits.max_instructions {
        return Err(format!(
            "{instructions} instructions, over the cap of {}",
            limits.max_instructions
        ));
    }
    let signatures = usize::from(transaction.message.header().num_required_signatures);
    if signatures > limits.max_signatures {
        return Err(format!(
            "{signatures} signatures, over the cap of {}",
            limits.max_signatures
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Budget;
    use solana_compute_budget_interface::ComputeBudgetInstruction;
    use solana_instruction::AccountMeta;

    fn noop(program: Pubkey, accounts: Vec<AccountMeta>, data: usize) -> Instruction {
        Instruction {
            program_id: program,
            accounts,
            data: vec![7; data],
        }
    }

    fn of(id: usize, instructions: Vec<Instruction>) -> Group {
        group(id.to_string(), instructions).unwrap()
    }

    fn ids(batches: &[Batch]) -> Vec<Vec<String>> {
        batches
            .iter()
            .map(|batch| batch.group_ids.clone())
            .collect()
    }

    /// Order kept, every batch within the caps, and none could take the next batch's first group.
    fn assert_greedy(
        payer: &Pubkey,
        groups: &[Group],
        version: MessageVersion,
        limits: &Limits,
        batches: &[Batch],
    ) {
        let packed: Vec<String> = batches
            .iter()
            .flat_map(|batch| batch.group_ids.clone())
            .collect();
        let given: Vec<String> = groups.iter().map(|group| group.id.clone()).collect();
        assert_eq!(packed, given);
        for batch in batches {
            assert_eq!(fits(payer, batch, version, limits), Ok(()));
        }
        for pair in batches.windows(2) {
            let next = groups
                .iter()
                .find(|group| group.id == pair[1].group_ids[0])
                .unwrap();
            assert!(fits(payer, &join(&pair[0], next), version, limits).is_err());
        }
    }

    fn pack_checked(groups: &[Group], version: MessageVersion) -> Vec<Batch> {
        let payer = Pubkey::new_unique();
        let limits = Limits::of(version);
        let (batches, errors) = pack(&payer, groups, version, &limits);
        assert_eq!(errors, vec![]);
        assert_greedy(&payer, groups, version, &limits, &batches);
        batches
    }

    #[test]
    fn a_batch_closes_before_the_byte_cap() {
        for version in [MessageVersion::Legacy, MessageVersion::V1] {
            let program = Pubkey::new_unique();
            let groups: Vec<Group> = (0..20)
                .map(|id| of(id, vec![noop(program, vec![], 300)]))
                .collect();
            assert!(pack_checked(&groups, version).len() > 1, "{version:?}");
        }
    }

    #[test]
    fn a_batch_closes_before_the_address_cap() {
        let program = Pubkey::new_unique();
        let groups: Vec<Group> = (0..10)
            .map(|id| {
                let accounts = (0..10)
                    .map(|_| AccountMeta::new_readonly(Pubkey::new_unique(), false))
                    .collect();
                of(id, vec![noop(program, accounts, 0)])
            })
            .collect();
        let batches = pack_checked(&groups, MessageVersion::V1);
        assert_eq!(
            ids(&batches).iter().map(Vec::len).collect::<Vec<_>>(),
            vec![6, 4]
        );
    }

    #[test]
    fn a_batch_closes_before_the_instruction_cap() {
        let program = Pubkey::new_unique();
        let groups: Vec<Group> = (0..10)
            .map(|id| of(id, (0..10).map(|_| noop(program, vec![], 0)).collect()))
            .collect();
        for version in [MessageVersion::Legacy, MessageVersion::V1] {
            let batches = pack_checked(&groups, version);
            assert_eq!(
                ids(&batches).iter().map(Vec::len).collect::<Vec<_>>(),
                vec![6, 4],
                "{version:?}"
            );
        }
    }

    #[test]
    fn a_batch_closes_before_the_v1_signature_cap() {
        let program = Pubkey::new_unique();
        let groups: Vec<Group> = (0..15)
            .map(|id| {
                let signer = AccountMeta::new_readonly(Pubkey::new_unique(), true);
                of(id, vec![noop(program, vec![signer], 0)])
            })
            .collect();
        let batches = pack_checked(&groups, MessageVersion::V1);
        assert_eq!(
            ids(&batches).iter().map(Vec::len).collect::<Vec<_>>(),
            vec![11, 4]
        );
    }

    #[test]
    fn a_batch_closes_before_the_callers_byte_and_address_caps() {
        let payer = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let sized: Vec<Group> = (0..4)
            .map(|id| of(id, vec![noop(program, vec![], 300)]))
            .collect();
        let narrow = Limits {
            max_bytes: 600,
            ..Limits::of(MessageVersion::V1)
        };
        let (batches, errors) = pack(&payer, &sized, MessageVersion::V1, &narrow);
        assert_eq!(errors, vec![]);
        assert_eq!(batches.len(), 4);
        assert_greedy(&payer, &sized, MessageVersion::V1, &narrow, &batches);

        let wide: Vec<Group> = (0..3)
            .map(|id| {
                let accounts = (0..10)
                    .map(|_| AccountMeta::new_readonly(Pubkey::new_unique(), false))
                    .collect();
                of(id, vec![noop(program, accounts, 0)])
            })
            .collect();
        let few = Limits {
            max_addresses: 15,
            ..Limits::of(MessageVersion::V1)
        };
        let (batches, errors) = pack(&payer, &wide, MessageVersion::V1, &few);
        assert_eq!(errors, vec![]);
        assert_eq!(batches.len(), 3);
        assert_greedy(&payer, &wide, MessageVersion::V1, &few, &batches);
    }

    #[test]
    fn a_group_within_the_compute_cap_packs_although_its_margin_crosses_it() {
        let payer = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let heavy = |id: usize, units: u32| Group {
            compute_units: Some(units),
            ..of(id, vec![noop(program, vec![], 0)])
        };
        let limits = Limits::of(MessageVersion::V1);

        let (batches, errors) = pack(&payer, &[heavy(0, 1_300_000)], MessageVersion::V1, &limits);
        assert_eq!(errors, vec![]);
        assert_eq!(ids(&batches), vec![vec!["0".to_string()]]);

        let (batches, errors) = pack(&payer, &[heavy(1, 1_400_001)], MessageVersion::V1, &limits);
        assert_eq!(batches, vec![]);
        assert!(matches!(
            errors.as_slice(),
            [PackError::GroupTooLarge { id, .. }] if id == "1"
        ));
    }

    #[test]
    fn a_batch_closes_before_the_compute_cap() {
        let program = Pubkey::new_unique();
        let groups: Vec<Group> = (0..5)
            .map(|id| Group {
                compute_units: Some(500_000),
                ..of(id, vec![noop(program, vec![], 0)])
            })
            .collect();
        let batches = pack_checked(&groups, MessageVersion::V1);
        assert_eq!(
            ids(&batches).iter().map(Vec::len).collect::<Vec<_>>(),
            vec![2, 2, 1]
        );
        assert_eq!(batches[0].compute_units, 1_000_000);
    }

    #[test]
    fn an_oversized_group_is_reported_and_the_others_still_pack() {
        let payer = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let groups = vec![
            of(0, vec![noop(program, vec![], 10)]),
            of(1, vec![noop(program, vec![], 5_000)]),
            of(2, vec![noop(program, vec![], 10)]),
        ];
        let limits = Limits::of(MessageVersion::V1);
        let (batches, errors) = pack(&payer, &groups, MessageVersion::V1, &limits);

        assert_eq!(ids(&batches), vec![vec!["0".to_string(), "2".to_string()]]);
        assert!(matches!(
            errors.as_slice(),
            [PackError::GroupTooLarge { id, .. }] if id == "1"
        ));
    }

    #[test]
    fn legacy_budget_instructions_count_toward_the_byte_cap() {
        let payer = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let pair = |data: usize| vec![noop(program, vec![], data), noop(program, vec![], data)];
        let fits_bare = |data: usize| {
            compile(
                &payer,
                &pair(data),
                Hash::default(),
                MessageVersion::Legacy,
                &Budget::default(),
            )
            .is_ok()
        };
        let data = (1..1_232).rev().find(|data| fits_bare(*data)).unwrap();
        let groups = vec![
            of(0, vec![noop(program, vec![], data)]),
            of(1, vec![noop(program, vec![], data)]),
        ];
        let limits = Limits::of(MessageVersion::Legacy);
        let (batches, errors) = pack(&payer, &groups, MessageVersion::Legacy, &limits);

        assert_eq!(errors, vec![]);
        assert_eq!(
            ids(&batches),
            vec![vec!["0".to_string()], vec!["1".to_string()]]
        );
    }

    #[test]
    fn a_group_keeps_its_heap_and_drops_its_limit_and_price() {
        let program = Pubkey::new_unique();
        let call = noop(program, vec![], 1);
        let lifted = group(
            "g",
            vec![
                ComputeBudgetInstruction::set_compute_unit_limit(10),
                ComputeBudgetInstruction::request_heap_frame(256 * 1024),
                call.clone(),
                ComputeBudgetInstruction::set_compute_unit_price(5),
                ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(1_000),
            ],
        )
        .unwrap();

        assert_eq!(lifted.instructions, vec![call]);
        assert_eq!(lifted.heap_bytes, Some(256 * 1024));
    }

    #[test]
    fn a_group_refuses_a_repeated_or_unknown_budget_instruction() {
        let repeated = group(
            "g",
            vec![
                ComputeBudgetInstruction::request_heap_frame(64 * 1024),
                ComputeBudgetInstruction::request_heap_frame(64 * 1024),
            ],
        );
        assert!(matches!(repeated, Err(PackError::Budget { .. })));

        let unknown = group(
            "g",
            vec![Instruction {
                program_id: solana_compute_budget_interface::ID,
                accounts: vec![],
                data: vec![9, 0, 0, 0, 0],
            }],
        );
        assert!(matches!(unknown, Err(PackError::Budget { .. })));

        let bad_heap = group(
            "g",
            vec![ComputeBudgetInstruction::request_heap_frame(1_000)],
        );
        assert!(matches!(bad_heap, Err(PackError::Budget { .. })));
    }

    #[test]
    fn a_group_with_nothing_but_budget_instructions_is_refused() {
        let empty = PackError::Empty { id: "g".into() };
        assert_eq!(group("g", vec![]), Err(empty.clone()));
        assert_eq!(
            group(
                "g",
                vec![
                    ComputeBudgetInstruction::set_compute_unit_limit(10),
                    ComputeBudgetInstruction::request_heap_frame(64 * 1024),
                ],
            ),
            Err(empty)
        );
    }

    #[test]
    fn a_batch_carries_the_largest_heap_of_its_groups() {
        let program = Pubkey::new_unique();
        let groups = vec![
            of(
                0,
                vec![
                    ComputeBudgetInstruction::request_heap_frame(64 * 1024),
                    noop(program, vec![], 1),
                ],
            ),
            of(1, vec![noop(program, vec![], 1)]),
            of(
                2,
                vec![
                    ComputeBudgetInstruction::request_heap_frame(128 * 1024),
                    noop(program, vec![], 1),
                ],
            ),
        ];
        let batches = pack_checked(&groups, MessageVersion::V1);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].heap_bytes, Some(128 * 1024));
    }
}
