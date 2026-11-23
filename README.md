# solana-transaction-builder

Packs instruction groups into as few legacy or v1 transactions as their caps allow, sizes each
budget by simulation and sends through
[solana-transaction-executor](https://github.com/marinade-finance/solana-transaction-executor).
The `solana-to-4.x` line uses the Solana 4.x crates; `solana-3.0.x` keeps the old
`TransactionBuilder` API.

## Building blocks (`solana-transaction-builder`)

- **`Group`** — instructions that land in one transaction or not at all. `group()` lifts a
  caller's Compute Budget instructions: a heap request stays with the group, a limit or a price is
  dropped because the run sets them per transaction. A group with no other instruction is refused.
  `group()` is the only constructor; `with_heap` replaces the heap and checks it the same way.
- **`MessageVersion`** — `Legacy` or `V1`. `Limits::of` gives its caps: legacy 1,232 bytes and 128
  account locks; v1 4,096 bytes, 64 addresses and 12 signatures; both 64 instructions and 1.4M
  compute units, with a 10% margin.
- **`Budget`** — compute-unit limit, priority rate (micro-lamports per CU), loaded-accounts limit
  and heap. Legacy places them as Compute Budget instructions ahead of the caller's; v1 places them
  in the message config, pays the rate as a lamport total over the limit, and always sets the
  compute and loaded-accounts limits because v1 reads an unset one as 0.
- **`pack`** — fills batches in order until the next group would break one of the given `Limits`,
  measuring each candidate compiled with its widest budget. A group too large alone comes back as
  an error; its simulated units count against the compute cap without the margin.
- **`compile` / `sign`** — compile checks the version's caps before any key signs; sign fills only
  the given signers' slots and refuses a transaction whose signature slots do not match its header.

## A run (`solana-transaction-builder-executor::run`)

The groups must be independent: batches are simulated and sent in parallel, in no fixed order, so
a group that spends what another group creates may fail.

1. Refuse the whole run as `NotSent` when a group id repeats, and a group whose signer is not given
   as `NotSent`.
2. Pack the groups.
3. Simulate each batch and set its compute and loaded-accounts limits to the measured values plus
   the margin; without measured units the compute limit is the cap.
4. A batch that fails simulation has its groups simulated alone: a failing group is reported
   `Failed` with its logs, the rest are packed again with their measured units. A re-packed batch
   that fails again goes one group per transaction.
5. Send through the executor, up to `RunConfig::max_in_flight` batches at once (8 by default), at
   most `max_attempts` builds per batch. The first attempt is marked `simulated`, so the executor
   does not simulate it again; a re-signed attempt, and a group sent alone after its re-packed
   batch failed, are simulated before the send. The first attempt's real budget is never simulated
   and sends skip preflight, so a budget that fails at the real rate shows only on chain. Rate
   limit, rebroadcast and the block height cache are the executor's `TransactionExecutorConfig`
   knobs. A group whose instructions equal an earlier group's is `NotSent`: batches built in
   parallel can share a blockhash and be one transaction. A batch that still compiles to another
   batch's transaction goes one group per transaction, or is `NotSent` when it is a single group.
   The first attempt pays the fee policy's start rate, 0 by default, or the network's rate under
   `FeeStrategy::Dynamic`; a retry rebuilds with a fresh blockhash and the next rate.
6. A batch whose failure is proven, or for which nothing was sent, goes again one group per
   transaction. A batch that landed with an error splits only when the executor waits for each
   blockhash to expire before a retry. A batch that is `Unknown` is never sent again.

`RunReport` gives each group's status, in the order of the input groups, and each transaction's
bytes, addresses, consumed units, limits, rate, fee, attempts and the executor's whole `Execution`:

- **`Landed`** — the signature and its slot.
- **`Failed`** — the error and logs, with the landed signature when it failed on chain. Without the
  expiry wait, a failed landing after several sent attempts is `Unknown` instead.
- **`NotSent`** — no attempt left the process, for example an RPC call failed; the caller may plan
  the group again.
- **`NotLanded`** — every attempt expired unfound; the caller may plan the group again.
- **`Unknown`** — the sent signatures; the caller must not plan the group again until one of them
  is settled.
- **`Planned`** — a dry run.
