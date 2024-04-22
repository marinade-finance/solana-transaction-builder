use crate::signature_builder::SignatureBuilder;
use crate::SendableSigner::SendableSigner;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::{Signer, SignerError};
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use std::sync::Arc;

pub trait SignablePreparedTransaction {
    fn signed_transaction(&self, recent_blockhash: Hash) -> Result<Transaction, SignerError>;
    fn signed_versioned_transaction(
        &self,
        recent_blockhash: Hash,
    ) -> Result<VersionedTransaction, SignerError> {
        let transaction = self.signed_transaction(recent_blockhash)?;
        Ok(VersionedTransaction::from(transaction.clone()))
    }
}

#[derive(Debug, Clone)]
pub struct PreparedTransaction {
    pub transaction: Transaction,
    pub signers: Vec<Arc<dyn Signer>>,
}

impl SignablePreparedTransaction for PreparedTransaction {
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

#[derive(Debug, Clone)]
pub struct SendablePreparedTransaction {
    pub transaction: Transaction,
    pub sendable_signers: Vec<SendableSigner>,
}

impl SignablePreparedTransaction for SendablePreparedTransaction {
    fn signed_transaction(&self, recent_blockhash: Hash) -> Result<Transaction, SignerError> {
        let mut transaction = self.transaction.clone();
        let signers: Vec<Arc<dyn Signer>> = self
            .sendable_signers
            .iter()
            .map(|s| {
                s.signer.lock().map_or_else(
                    |e| panic!("get_signer: failed to lock signer: {:?}", e),
                    |s| s.clone().into(),
                )
            })
            .collect();
        transaction.try_sign(
            &signers.iter().map(|arc| arc.as_ref()).collect::<Vec<_>>(),
            recent_blockhash,
        )?;
        Ok(transaction)
    }
}

impl SendablePreparedTransaction {
    pub fn new(
        transaction: Transaction,
        signature_builder: &SignatureBuilder,
    ) -> Result<Self, Pubkey> {
        let signers = signature_builder
            .sendable_signers_for_transaction(&transaction)?
            .into_iter()
            .collect();
        Ok(Self {
            transaction,
            sendable_signers: signers,
        })
    }
}
