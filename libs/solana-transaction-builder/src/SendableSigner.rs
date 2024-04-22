use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Signature, SignerError};
use solana_sdk::signer::Signer;
use std::sync::{Arc, Mutex};

pub trait SendableSignerTrait: Signer + Send + Sized {}

#[derive(Debug)]
pub struct SendableSigner {
    pub signer: Mutex<Arc<dyn Signer>>,
}

impl Signer for SendableSigner {
    fn try_pubkey(&self) -> Result<Pubkey, SignerError> {
        let signer = self.signer.lock().unwrap();
        signer.try_pubkey()
    }

    fn try_sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        let mut signer = self.signer.lock().unwrap();
        signer.try_sign_message(message)
    }

    fn is_interactive(&self) -> bool {
        let signer = self.signer.lock().unwrap();
        signer.is_interactive()
    }
}

impl Clone for SendableSigner {
    fn clone(&self) -> Self {
        Self {
            signer: Mutex::new(
                self.signer
                    .lock()
                    .expect("Cannot lock singer mutex")
                    .clone(),
            ),
        }
    }
}
