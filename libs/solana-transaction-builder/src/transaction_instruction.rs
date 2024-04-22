use borsh::{BorshDeserialize, BorshSerialize};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

// Set of struct wrappers that can be used to deserialize instruction.
// For marinade client it's the base64 format which is used in multisig like SPL Governance.

#[derive(Debug, Clone, BorshDeserialize, BorshSerialize)]
pub struct TransactionInstruction {
    // Target program to execute against.
    pub program_id: Pubkey,
    // Accounts required for the transaction.
    pub accounts: Vec<TransactionAccount>,
    // Instruction data for the transaction.
    pub data: Vec<u8>,
}

impl From<&TransactionInstruction> for Instruction {
    fn from(tx: &TransactionInstruction) -> Instruction {
        Instruction {
            program_id: tx.program_id,
            accounts: tx.accounts.iter().map(AccountMeta::from).collect(),
            data: tx.data.clone(),
        }
    }
}

#[derive(Debug, BorshSerialize, BorshDeserialize, Clone)]
pub struct TransactionAccount {
    pub pubkey: Pubkey,
    pub is_signer: bool,
    pub is_writable: bool,
}

impl From<&TransactionAccount> for AccountMeta {
    fn from(account: &TransactionAccount) -> AccountMeta {
        match account.is_writable {
            false => AccountMeta::new_readonly(account.pubkey, account.is_signer),
            true => AccountMeta::new(account.pubkey, account.is_signer),
        }
    }
}

impl From<&AccountMeta> for TransactionAccount {
    fn from(account_meta: &AccountMeta) -> TransactionAccount {
        TransactionAccount {
            pubkey: account_meta.pubkey,
            is_signer: account_meta.is_signer,
            is_writable: account_meta.is_writable,
        }
    }
}

pub fn print_base64(instructions: &Vec<Instruction>) -> anyhow::Result<()> {
    for instruction in instructions {
        let transaction_instruction = TransactionInstruction {
            program_id: instruction.program_id,
            accounts: instruction
                .accounts
                .iter()
                .map(TransactionAccount::from)
                .collect(),
            data: instruction.data.clone(),
        };
        println!(
            "program: {}\n  {}",
            instruction.program_id,
            base64::encode(transaction_instruction.try_to_vec()?)
        );
    }
    Ok(())
}
