use anchor_client::RequestBuilder;
use anyhow::anyhow;
use log::error;
use solana_sdk::signer::Signer;
use solana_transaction_builder::TransactionBuilder;
use std::ops::Deref;

pub fn add_instructions_to_builder_from_anchor<C: Deref<Target = impl Signer> + Clone>(
    transaction_builder: &mut TransactionBuilder,
    request_builder: RequestBuilder<C>,
) -> anyhow::Result<()> {
    let instructions = request_builder.instructions().map_err(|e| {
        error!(
            "add_instructions_from_anchor_builder: error building instructions: {:?}",
            e
        );
        anyhow!(e)
    })?;
    transaction_builder.add_instructions(instructions)?;
    transaction_builder.finish_instruction_pack();
    Ok(())
}
