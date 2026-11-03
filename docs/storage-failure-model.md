# Storage failure model

What the `scientdb-storage` engine promises today, under which assumptions, and how it is tested.

## Platform assumptions

- Linux, with the store on a local filesystem whose `fsync` on a file makes that file's written
  data and size durable, and whose `fsync` on a directory makes its entries durable.
- Writes are positioned (`pwrite`-style); append mode is never used.
- Writes that were not followed by a successful `fsync` may be lost, partially persisted in
  512-byte sectors, or persisted out of order.
- Data that was durable does not change on its own. Media corruption is a separate fault class.
- A file's existence is durable only after its directory is synced.

## Commit protocol

1. Validate the attempt identity and the change's preconditions against the newest committed state.
2. Write the history entry to `content.sdb` and the new tree node to a fresh page of `meta.sdb`.
   No page referenced by any publication record is modified.
3. Sync `content.sdb`, then `meta.sdb`.
4. Write the publication record for generation `g+1` into slot `(g+1) mod 2`.
5. Sync `meta.sdb`.
6. Make the generation visible to new views and acknowledge the commit.

Store creation syncs the parent of every directory it had to create, then the store directory after
creating the files, and publishes generation 1 the same way.

## What an acknowledgment means

A successful `commit` returns only after step 5. Under the assumptions above, the whole change —
records, both relationship directions, content bindings, history, supplied evidence and the attempt
receipt — survives a crash, and no part of it appears without the rest.

## Recovery

Opening reads both slots. A slot is usable if its checksum, magic, version and slot position are
valid. Recovery selects the usable record with the highest generation; damaged slots are ignored and
reported through `NativeStore::recovery_diagnostics`. If neither slot is usable, opening fails and
the store is left untouched; the engine never initializes a new store over existing files. An
unsupported format version is refused without modification.

Cut points:

| Interruption | Result after reopening |
|---|---|
| Before the publication record write | The previous committed state. |
| After that write, before its sync completed | The previous or the new state, whichever record survived intact. |
| After acknowledgment | The new state. |

**Limitation:** a publication record damaged after it was acknowledged cannot always be told apart
from an interrupted write of that record. With two checksummed slots, recovery then falls back to
the older record and reports the damaged slot; it cannot prove whether the newer one was ever
acknowledged.

## Uncertain outcomes

If writing or syncing the publication record fails, the commit returns `Indeterminate`: the record
may or may not be durable. The store then refuses further writes and staging, and `outcome` reports
`Unknown`, until it is reopened and recovery establishes the committed state. A failure before
step 4 returns `Rejected`; nothing was published.

## Process exit versus loss of unsynced data

Killing the owning process leaves unsynced writes in the operating system's cache, so a
kill-and-reopen test exercises process exit, not power loss. Power loss is tested with the storage
simulator, which runs the production engine and models the assumptions above: it drops, keeps, tears
or reorders unsynced writes and forgets unsynced directory entries.

## Ownership

One process owns a store at a time, through an exclusive lock on `LOCK` taken at open and held until
the store handle and every view and content reader derived from it are dropped. A second opener gets
`Busy`.

## Readers and content integrity

A view reads the generation that was current when it began. Content is streamed in 64 KiB chunks;
its SHA-256 is checked when the stream ends. Bytes delivered before a mismatch are not verified, and
a mismatch fails the read instead of reporting a complete stream.

## Not implemented yet

Space reuse, removal modes, multi-level trees, concurrent commit validation beyond preconditions,
and recovery from an uncertain outcome without reopening.
