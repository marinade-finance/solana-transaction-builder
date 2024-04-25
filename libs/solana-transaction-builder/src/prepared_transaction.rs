use crate::signature_builder::SignatureBuilder;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::SignerError;
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use std::rc::Rc;

pub trait SignedTransaction {
    fn signed_transaction(&self, recent_blockhash: Hash) -> Result<Transaction, SignerError>;
    fn signed_versioned_transaction(
        &self,
        recent_blockhash: Hash,
    ) -> Result<VersionedTransaction, SignerError> {
        let transaction = self.signed_transaction(recent_blockhash)?;
        Ok(VersionedTransaction::from(transaction))
    }
}

#[derive(Debug, Clone)]
pub struct PreparedTransaction {
    pub transaction: Transaction,
    pub signers: Vec<Rc<Keypair>>,
}

impl SignedTransaction for PreparedTransaction {
    fn signed_transaction(&self, recent_blockhash: Hash) -> Result<Transaction, SignerError> {
        let mut transaction = self.transaction.clone();
        transaction.try_sign(
            &self
                .signers
                .iter()
                .map(|arc| arc.as_ref())
                .collect::<Vec<_>>(),
            recent_blockhash,
        )?;
        Ok(transaction)
    }
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
}
