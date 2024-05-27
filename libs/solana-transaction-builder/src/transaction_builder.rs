use crate::prepared_transaction::PreparedTransaction;
use crate::signature_builder::SignatureBuilder;
use anyhow::anyhow;
use log::error;
use once_cell::sync::OnceCell;
use solana_sdk::signature::Keypair;
use solana_sdk::signers::Signers;
use solana_sdk::{
    instruction::Instruction, packet::PACKET_DATA_SIZE, pubkey::Pubkey, signature::Signer,
    transaction::Transaction,
};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum TransactionBuildError {
    #[error("Unknown signer ${0}")]
    UnknownSigner(Pubkey),
    #[error("Too big transaction")]
    TooBigTransaction,
}

#[derive(Debug, Clone)]
pub struct TransactionBuilder {
    fee_payer: Pubkey,
    signature_builder: SignatureBuilder, // invariant: has signers for all instructions
    // instruction pack contains a list of instruction with optional description to them
    instruction_packs: Vec<Vec<(Instruction, Option<String>)>>,
    current_instruction_pack: OnceCell<Vec<(Instruction, Option<String>)>>,
    max_transaction_size: usize,
}

impl TransactionBuilder {
    pub fn new(fee_payer: Arc<Keypair>, max_transaction_size: usize) -> Self {
        let mut signature_builder = SignatureBuilder::default();
        let builder = Self {
            fee_payer: signature_builder.add_signer(fee_payer),
            signature_builder,
            instruction_packs: Vec::new(),
            current_instruction_pack: OnceCell::new(),
            max_transaction_size,
        };
        builder.current_instruction_pack.set(Vec::new()).unwrap();
        builder
    }

    pub fn fee_payer(&self) -> Pubkey {
        self.fee_payer
    }

    pub fn get_signer(&self, key: &Pubkey) -> Option<Arc<Keypair>> {
        self.signature_builder.get_signer(key)
    }

    pub fn fee_payer_signer(&self) -> Arc<Keypair> {
        self.get_signer(&self.fee_payer()).unwrap()
    }

    ///constructor, limit size to a single transaction
    pub fn limited(fee_payer: Arc<Keypair>) -> Self {
        Self::new(fee_payer, PACKET_DATA_SIZE)
    }

    ///constructor, no size limit, can be split in many marinade-transactions
    pub fn unlimited(fee_payer: Arc<Keypair>) -> Self {
        Self::new(fee_payer, 0)
    }

    pub fn add_signer(&mut self, signer: Arc<Keypair>) -> Pubkey {
        self.signature_builder.add_signer(signer)
    }

    pub fn generate_signer(&mut self) -> Pubkey {
        self.signature_builder.new_signer()
    }

    pub fn add_signer_checked(&mut self, signer: &Arc<Keypair>) {
        if !self.signature_builder.contains_key(&signer.pubkey()) {
            self.add_signer(signer.clone());
        }
    }

    fn check_signers(&self, instruction: &Instruction) -> Result<(), TransactionBuildError> {
        for account in &instruction.accounts {
            if account.is_signer && !self.signature_builder.contains_key(&account.pubkey) {
                error!(
                    "Unknown signer {} in signature builder {:?}, instruction accounts: {:?}",
                    account.pubkey,
                    self.signature_builder.pubkeys(),
                    instruction.accounts
                );
                return Err(TransactionBuildError::UnknownSigner(account.pubkey));
            }
        }
        Ok(())
    }

    #[inline]
    pub fn finish_instruction_pack(&mut self) {
        self.instruction_packs.push(
            self.current_instruction_pack
                .take()
                .expect("Finish must be called when an instruction pack is defined"),
        );
        self.current_instruction_pack.set(Vec::new()).unwrap();
    }

    #[inline]
    pub fn abort_instruction_pack(&mut self) {
        self.current_instruction_pack
            .take()
            .expect("Abort must be called when an instruction pack is defined");
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.is_current_pack_empty() && self.instruction_packs.is_empty()
    }

    #[inline]
    fn is_current_pack_empty(&self) -> bool {
        if let Some(current_instruction_pack) = self.current_instruction_pack.get() {
            current_instruction_pack.is_empty()
        } else {
            true
        }
    }

    pub fn add_instructions<I>(&mut self, instructions: I) -> anyhow::Result<&mut Self>
    where
        I: IntoIterator<Item = Instruction>,
    {
        for instruction in instructions {
            self.add_instruction(instruction)?;
        }
        Ok(self)
    }

    pub fn add_instructions_with_description<I>(
        &mut self,
        instructions_with_description: I,
    ) -> anyhow::Result<&mut Self>
    where
        I: IntoIterator<Item = (Instruction, String)>,
    {
        for (instruction, description) in instructions_with_description {
            self.add_instruction_with_description(instruction, description)?;
        }
        Ok(self)
    }

    pub fn add_instruction(&mut self, instruction: Instruction) -> anyhow::Result<&mut Self> {
        self.add_instruction_internal(instruction, None)
    }

    pub fn add_instruction_with_description(
        &mut self,
        instruction: Instruction,
        description: String,
    ) -> anyhow::Result<&mut Self> {
        self.add_instruction_internal(instruction, Some(description))
    }

    fn add_instruction_internal(
        &mut self,
        instruction: Instruction,
        description: Option<String>,
    ) -> anyhow::Result<&mut Self> {
        self.check_signers(&instruction)?;
        let current = self.current_instruction_pack.get_mut().unwrap();

        current.push((instruction, description));
        let transaction_candidate = Transaction::new_with_payer(
            &current.iter().cloned().unzip::<_, _, Vec<_>, Vec<_>>().0,
            Some(&self.fee_payer),
        );
        let tx_size_candidate = bincode::serialize(&transaction_candidate).unwrap().len();
        if self.max_transaction_size > 0 && tx_size_candidate > self.max_transaction_size {
            // Transaction is too big to add new instruction, remove the last one
            current.pop();
            let transaction_current = bincode::serialize(&transaction_candidate).unwrap().len();
            let tx_size_current = bincode::serialize(&transaction_current).unwrap().len();
            error!(
                "add_instruction: too big transaction, tx size with added transaction: {}, original tx size: {},  max size: {}",
                tx_size_candidate,  tx_size_current, self.max_transaction_size);
            return Err(anyhow!(TransactionBuildError::TooBigTransaction));
        }

        Ok(self)
    }

    /// This method removes the transactions from the returned transaction pack from the builder.
    /// Next call returns the next pack of transactions.
    pub fn build_next(&mut self) -> Option<PreparedTransaction> {
        if !self.is_current_pack_empty() {
            self.finish_instruction_pack()
        }
        if self.is_empty() {
            return None;
        }
        if !self.instruction_packs.is_empty() {
            let (instructions, descriptions): (Vec<Instruction>, Vec<Option<String>>) =
                self.instruction_packs.remove(0).into_iter().unzip();
            let transaction = Transaction::new_with_payer(&instructions, Some(&self.fee_payer));
            Some(
                PreparedTransaction::new(transaction, &self.signature_builder, descriptions)
                    .expect("Signature keys must be checked when instruction added"),
            )
        } else {
            None
        }
    }

    pub fn build_one(&mut self) -> PreparedTransaction {
        if let Some(transaction) = self.build_next() {
            assert!(self.instruction_packs.is_empty());
            transaction
        } else {
            panic!("Is not single transaction");
        }
    }

    /// Next transaction from builder. It merges multiple transaction packs together (as much as fits into tx).
    /// This method removes the transactions from the returned transaction pack from the builder.
    /// Next call returns the next pack of transactions.
    pub fn build_next_combined(&mut self) -> Option<PreparedTransaction> {
        if !self.is_current_pack_empty() {
            self.finish_instruction_pack()
        }
        if self.instruction_packs.is_empty() {
            return None;
        }

        let (transaction, descriptions) = if self.max_transaction_size == 0 {
            let (instructions, descriptions): (Vec<Instruction>, Vec<Option<String>>) =
                self.instruction_packs.drain(..).flatten().unzip();
            (
                Transaction::new_with_payer(&instructions, Some(&self.fee_payer)),
                descriptions,
            )
        } else {
            // One pack must fit transaction anyway
            let (mut instructions, mut descriptions): (Vec<Instruction>, Vec<Option<String>>) =
                self.instruction_packs.remove(0).into_iter().unzip();
            let mut transaction = Transaction::new_with_payer(&instructions, Some(&self.fee_payer));
            while let Some(next_pack) = self.instruction_packs.first() {
                let (next_instructions, next_descriptions): (
                    Vec<Instruction>,
                    Vec<Option<String>>,
                ) = next_pack.iter().cloned().unzip();
                // Try to add next pack
                instructions.extend(next_instructions.into_iter());
                descriptions.extend(next_descriptions.into_iter());
                let transaction_candidate =
                    Transaction::new_with_payer(&instructions, Some(&self.fee_payer));

                if bincode::serialize(&transaction_candidate).unwrap().len()
                    <= self.max_transaction_size
                {
                    // Accept it
                    transaction = transaction_candidate;
                    // and move to the next pack
                    self.instruction_packs.remove(0);
                } else {
                    // Stop trying
                    break;
                }
            }
            (transaction, descriptions)
        };
        Some(
            PreparedTransaction::new(transaction, &self.signature_builder, descriptions)
                .expect("Signature keys must be checked when instruction added"),
        )
    }

    pub fn build_single_combined(&mut self) -> Option<PreparedTransaction> {
        if let Some(transaction) = self.build_next_combined() {
            assert!(self.is_empty(), "Not fit single transaction");
            Some(transaction)
        } else {
            None
        }
    }

    pub fn sequence(&mut self) -> Sequence {
        Sequence { builder: self }
    }

    pub fn sequence_combined(&mut self) -> CombinedSequence {
        CombinedSequence { builder: self }
    }

    pub fn fits_single_transaction(&self) -> bool {
        let instructions: Vec<Instruction> = self.instructions();
        let transaction = Transaction::new_with_payer(&instructions, Some(&self.fee_payer));
        bincode::serialize(&transaction).unwrap().len() <= self.max_transaction_size
    }

    pub fn instructions(&self) -> Vec<Instruction> {
        let (mut instructions, _): (Vec<Instruction>, Vec<_>) =
            self.instruction_packs.iter().flatten().cloned().unzip();
        if let Some(current_instructions) = self.current_instruction_pack.get() {
            instructions.extend(
                current_instructions
                    .iter()
                    .map(|(instr, _)| instr.clone())
                    .collect::<Vec<Instruction>>(),
            )
        }
        instructions
    }
}

pub struct Sequence<'a> {
    builder: &'a mut TransactionBuilder,
}

impl<'a> Iterator for Sequence<'a> {
    type Item = PreparedTransaction;

    fn next(&mut self) -> Option<Self::Item> {
        self.builder.build_next()
    }
}

pub struct CombinedSequence<'a> {
    builder: &'a mut TransactionBuilder,
}

impl<'a> Iterator for CombinedSequence<'a> {
    type Item = PreparedTransaction;

    fn next(&mut self) -> Option<Self::Item> {
        self.builder.build_next_combined()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::instruction::AccountMeta;
    use solana_sdk::signature::Keypair;

    #[test]
    fn test_add_signer() {
        let signer1 = Arc::new(Keypair::new());
        let signer2 = Arc::new(Keypair::new());
        let mut tx_builder = TransactionBuilder::limited(Arc::new(Keypair::new()));
        tx_builder.add_signer_checked(&signer1);
        tx_builder.add_signer_checked(&signer2);
        tx_builder.add_signer_checked(&signer1);
        assert_eq!(tx_builder.signature_builder.signers().len(), 3); // fee payer + 2 signers

        tx_builder.add_signer(signer1.clone());
        assert_eq!(tx_builder.signature_builder.signers().len(), 3);

        let ix = Instruction {
            program_id: Pubkey::default(),
            accounts: vec![
                AccountMeta {
                    is_signer: true,
                    is_writable: false,
                    pubkey: signer2.pubkey(),
                },
                AccountMeta {
                    is_signer: true,
                    is_writable: false,
                    pubkey: signer1.pubkey(),
                },
                AccountMeta {
                    is_signer: true,
                    is_writable: false,
                    pubkey: tx_builder.fee_payer,
                },
            ],
            data: vec![],
        };
        assert!(tx_builder.check_signers(&ix).is_ok());
    }

    #[test]
    fn is_sync_send_able() {
        fn do_stuff<T: Sync + Send>(_t: T) {}

        let keypair = Arc::new(Keypair::new());
        let tx_builder = TransactionBuilder::limited(keypair.clone());

        assert_eq!(tx_builder.signature_builder.signers().len(), 1_usize);
        assert!(tx_builder.signature_builder.contains_key(&keypair.pubkey()));

        do_stuff(tx_builder.signature_builder);
    }
}
