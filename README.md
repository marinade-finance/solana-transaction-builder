# solana-transaction-builder

Packs instruction groups into as few legacy or v1 transactions as their caps allow, sizes each
budget by simulation and sends through
[solana-transaction-executor](https://github.com/marinade-finance/solana-transaction-executor).
The `solana-to-4.x` line uses the Solana 4.x crates; `solana-3.0.x` keeps the old
`TransactionBuilder` API.

## Building blocks (`solana-transaction-builder`)

- **`Group`** — instructions that land in one transaction or not at all. `group()` lifts a
  caller's Compute Budget instructions: a heap request stays with the group, a limit or a price is
  dropped because the run sets them per transaction.
- **`MessageVersion`** — `Legacy` or `V1`. `Limits::of` gives its caps: legacy 1,232 bytes and 128
  account locks; v1 4,096 bytes, 64 addresses, 64 instructions and 12 signatures; both 1.4M compute
  units, with a 10% margin.
- **`Budget`** — compute-unit limit, priority rate (micro-lamports per CU), loaded-accounts limit
  and heap. Legacy places them as Compute Budget instructions ahead of the caller's; v1 places them
  in the message config, pays the rate as a lamport total over the limit, and always sets the
  compute and loaded-accounts limits because v1 reads an unset one as 0.
- **`pack`** — fills batches in order until the next group would break a cap, measuring each
  candidate compiled with its widest budget. A group too large alone comes back as an error.
- **`compile` / `sign`** — compile checks the caps before any key signs; sign fills only the
  given signers' slots.

## A run (`solana-transaction-builder-executor::run`)

1. Simulate each group alone; a failing group is reported `Failed` with its logs.
2. Pack the rest.
3. Simulate each batch and set its compute and loaded-accounts limits to the measured values plus
   the margin. A batch that fails as a whole goes one group per transaction.
4. Send through the executor. The first attempt pays the fee policy's start rate, 0 by default; a
   retry rebuilds with a fresh blockhash and the next rate.

`RunReport` gives each group's status (`Landed`, `Failed`, `Unconfirmed` or `Planned` in a dry
run) and each transaction's bytes, addresses, consumed units, limits, rate, fee and attempts.
