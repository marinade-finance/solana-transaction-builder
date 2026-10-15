use solana_message::v1::{MAX_HEAP_SIZE, MIN_HEAP_SIZE};

use crate::Limits;

/// A v1 config reads an unset compute-unit limit as 0, so a v1 transaction always carries one.
pub const DEFAULT_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// A v1 config reads an unset loaded-accounts limit as 0, so a v1 transaction always carries one.
pub const DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT: u32 = 64 * 1024 * 1024;

const MICRO_LAMPORTS_PER_LAMPORT: u128 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageVersion {
    Legacy,
    V1,
}

/// The fee is a rate in both versions; v1 pays it as a lamport total over the limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    pub compute_unit_limit: Option<u32>,
    pub micro_lamports_per_cu: Option<u64>,
    pub loaded_accounts_data_size_limit: Option<u32>,
    pub heap_bytes: Option<u32>,
}

impl Budget {
    pub fn priority_fee_lamports(&self) -> u64 {
        let rate = u128::from(self.micro_lamports_per_cu.unwrap_or(0));
        let limit = u128::from(
            self.compute_unit_limit
                .unwrap_or(DEFAULT_COMPUTE_UNIT_LIMIT),
        );
        u64::try_from((rate * limit).div_ceil(MICRO_LAMPORTS_PER_LAMPORT)).unwrap_or(u64::MAX)
    }
}

/// The largest transaction a budget makes; a legacy price is an instruction that costs units.
pub fn widest_budget(limits: &Limits, heap_bytes: Option<u32>) -> Budget {
    Budget {
        compute_unit_limit: Some(limits.max_compute_units),
        micro_lamports_per_cu: Some(1),
        loaded_accounts_data_size_limit: Some(DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT),
        heap_bytes,
    }
}

pub fn valid_heap(bytes: u32) -> bool {
    bytes.is_multiple_of(1024) && (MIN_HEAP_SIZE..=MAX_HEAP_SIZE).contains(&bytes)
}
