use solana_message::v1;
use solana_packet::PACKET_DATA_SIZE;
use solana_transaction::sanitized::MAX_TX_ACCOUNT_LOCKS;

use crate::MessageVersion;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_addresses: usize,
    pub max_instructions: usize,
    pub max_signatures: usize,
    pub max_compute_units: u32,
    pub margin_bps: u32,
}

impl Limits {
    pub fn of(version: MessageVersion) -> Self {
        match version {
            MessageVersion::Legacy => Self {
                max_bytes: PACKET_DATA_SIZE,
                max_addresses: MAX_TX_ACCOUNT_LOCKS,
                max_instructions: usize::MAX,
                max_signatures: usize::from(u8::MAX),
                max_compute_units: crate::DEFAULT_COMPUTE_UNIT_LIMIT,
                margin_bps: 1_000,
            },
            MessageVersion::V1 => Self {
                max_bytes: v1::MAX_TRANSACTION_SIZE,
                max_addresses: usize::from(v1::MAX_ADDRESSES),
                max_instructions: usize::from(v1::MAX_INSTRUCTIONS),
                max_signatures: usize::from(v1::MAX_SIGNATURES),
                max_compute_units: crate::DEFAULT_COMPUTE_UNIT_LIMIT,
                margin_bps: 1_000,
            },
        }
    }

    pub fn with_margin(&self, units: u64) -> u64 {
        units
            .saturating_mul(10_000 + u64::from(self.margin_bps))
            .div_ceil(10_000)
    }
}
