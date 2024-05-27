use log::error;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer, SignerError},
    signers::Signers,
    transaction::Transaction,
};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct SignatureBuilder(HashMap<Pubkey, Arc<Keypair>>);

impl SignatureBuilder {
    pub fn add_signer(&mut self, signer: Arc<Keypair>) -> Pubkey {
        let pubkey = signer.pubkey();
        self.0.insert(pubkey, signer);
        pubkey
    }

    pub fn new_signer(&mut self) -> Pubkey {
        let keypair = Arc::new(Keypair::new());
        self.add_signer(keypair)
    }

    pub fn contains_key(&self, key: &Pubkey) -> bool {
        self.0.contains_key(key)
    }

    pub fn get_signer(&self, key: &Pubkey) -> Option<Arc<Keypair>> {
        self.0.get(key).cloned()
    }

    pub fn signers(&self) -> Vec<Arc<Keypair>> {
        self.0.values().cloned().collect()
    }

    pub fn into_signers(self) -> Vec<Arc<Keypair>> {
        self.0.into_values().collect()
    }

    pub fn sign_transaction(&self, transaction: &mut Transaction) -> Result<(), SignerError> {
        let keys = transaction.message().account_keys
            [0..transaction.message().header.num_required_signatures as usize]
            .to_vec();
        let message = transaction.message_data();
        for (pos, key) in keys.into_iter().enumerate() {
            if let Some(keypair) = self.get_signer(&key) {
                transaction.signatures[pos] = keypair.try_sign_message(&message)?;
            } else {
                error!("sign_transaction: not enough signers, expected key: {}, available keys in builder: {:?}",
                    key, self.pubkeys());
                return Err(SignerError::NotEnoughSigners);
            }
        }
        Ok(())
    }

    pub fn signers_for_transaction(
        &self,
        transaction: &Transaction,
    ) -> Result<Vec<Arc<Keypair>>, Pubkey> {
        transaction.message().account_keys
            [0..transaction.message().header.num_required_signatures as usize]
            .iter()
            .map(|key| self.get_signer(key).ok_or(*key))
            .collect()
    }
}

impl Signers for SignatureBuilder {
    fn pubkeys(&self) -> Vec<Pubkey> {
        self.0.keys().cloned().collect()
    }

    fn try_pubkeys(&self) -> Result<Vec<Pubkey>, SignerError> {
        Ok(self.pubkeys())
    }

    fn sign_message(&self, message: &[u8]) -> Vec<Signature> {
        self.0
            .values()
            .map(|signer| signer.sign_message(message))
            .collect()
    }

    fn try_sign_message(&self, message: &[u8]) -> Result<Vec<Signature>, SignerError> {
        self.0
            .values()
            .map(|signer| signer.try_sign_message(message))
            .collect()
    }

    fn is_interactive(&self) -> bool {
        false
    }
}
