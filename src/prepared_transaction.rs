use crate::signature_builder::SignatureBuilder;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::{Signer, SignerError};
use solana_sdk::transaction::Transaction;
use std::sync::Arc;

pub struct PreparedTransaction {
    pub transaction: Transaction,
    pub signers: Vec<Arc<dyn Signer>>,
}

impl PreparedTransaction {
    pub fn new(
        transaction: Transaction,
        signature_builder: &SignatureBuilder,
    ) -> Result<Self, Pubkey> {
        let signers = signature_builder.signers_for_transaction(&transaction)?;
        Ok(Self {
            transaction,
            signers,
        })
    }

    pub fn sign(&mut self, recent_blockhash: Hash) -> Result<&Transaction, SignerError> {
        self.transaction.try_sign(
            &self
                .signers
                .iter()
                .map(|arc| arc.as_ref())
                .collect::<Vec<_>>(),
            recent_blockhash,
        )?;
        Ok(&self.transaction)
    }

    pub fn into_signed(mut self, recent_blockhash: Hash) -> Result<Transaction, SignerError> {
        self.sign(recent_blockhash)?;
        Ok(self.transaction)
    }
}
