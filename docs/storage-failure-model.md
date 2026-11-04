# Storage failure model

What the `cce-storage` engine promises today, under which assumptions, and how it is tested.

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
2. Write the history entry to `content.cce` and the new tree node to a fresh page of `meta.cce`.
   No page referenced by any publication record is modified.
3. Sync `content.cce`, then `meta.cce`.
4. Write the publication record for generation `g+1` into slot `(g+1) mod 2`.
5. Sync `meta.cce`.
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

Before the recovered state is used, its cursors are checked against the files (the root lies below
the next unallocated page, the metadata file holds every allocated page, the content file holds the
committed content length), and both files are synced. The previous owner may have exited after
writing a publication record but before syncing it; that record could still be visible in the
operating system's cache, and syncing it at open keeps a later retry from acknowledging a commit that
is not yet durable.

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

## Failure combinations the tests explore

The simulator tests run the production engine with a simulated I/O adapter. They explore:

- **A crash at every operation of a commit.** In a store with one committed transaction, a second
  commit (staging included) is cut after each of its mutating operations, from none to all. Each
  crashed image is recovered under 14 choices of which unsynced writes survive: none, all, and 12
  seeded subsets. In a seeded subset each unsynced write independently survives or is lost, a
  surviving write longer than one sector may keep only a whole number of its first 512-byte
  sectors, and surviving writes are applied in shuffled order. Recovery must produce the whole
  old or the whole new state, and an acknowledged commit must survive.
- **A crash at every operation of store creation**, with no unsynced writes surviving and with all
  of them surviving. Opening then yields the complete empty store or an explicit refusal; it never
  initializes a store.
- **Injected I/O errors at chosen operations:** the content and metadata syncs of the data barrier,
  the sync of the publication record, each mutating operation of opening, the first read of
  opening, and a read while streaming content.
- **Lossy sync failures:** a failed sync may discard the file's unsynced writes, as an operating
  system may when write-back fails, so a later successful sync cannot make them durable. This is
  injected at the content and metadata syncs of the data barrier and at the publication sync.
- **Faults during recovery:** every crashed image of the commit sweep above, under all 14 survival
  choices, is opened with an I/O error at each mutating operation of opening, with a failed first
  read, and with a crash after each number of those operations followed by loss of unsynced
  writes. The faulty open must leave the durable bytes unchanged; a clean open must then give the
  same state and attempt outcomes as an uninterrupted recovery of that image, and opening again
  must leave the store files unchanged.
- **Damaged durable bytes** in specific places: a newest publication record with a bad checksum or
  torn after any number of bytes, both publication records damaged, the newest root page damaged,
  a content file shorter than the newest record claims, and a damaged content segment.

Separately, real processes are killed and the store is reopened by a fresh process (process exit,
not power loss); one scenario run on the filesystem adapter and on the simulator must issue the
same persistence operations in the same order; and on a real filesystem, a file size limit makes a
commit's write stop part-way and fail, as on a full disk, which must fail the commit explicitly.

When a case of a cut sweep fails (the commit sweeps, the store-creation sweep, or the
faults-during-recovery sweep), its crash image (durable bytes, and the bytes still cached after a
process exit), the operation trace and a label naming the cut and the survival choice (which of the
sampled subsets, or process exit) are written to a new directory under the system temporary
directory, named in the failure message. Passing runs write nothing there.
To re-open a saved case, copy `image.meta.cce.durable` and `image.content.cce.durable` (or the
`.cached` files, for the state a fresh process would see after a process exit) to `meta.cce` and
`content.cce` in an empty directory, create an empty `LOCK` file there, and open that directory.

The tests do **not** explore:

- crashes of several processes at once, or several processes writing one store (ownership admits a
  single owner; a second owner is refused);
- survival subsets beyond the 14 per cut: the seeded subsets are a sample, not an enumeration, and
  a surviving write is never cut except after a whole number of its 512-byte sectors;
- cut sweeps of larger stores: the general sweeps start from at most one committed transaction.
  Three more sweeps cut commits that reshape the tree, from small fixtures just before the change:
  one that splits a leaf below the root, one that splits the root branch (the tree grows a level,
  from a store of a few dozen commits) and one whose deletions shrink the root to a leaf. Deeper
  trees and larger stores are tested for reopening after power loss, but not cut at every
  operation;
- damage to durable bytes beyond the cases listed, such as random or multiple-location corruption,
  damaged interior tree nodes, or writes that are lost or misdirected after a successful sync;
- other errors of a real filesystem: `EINTR` inside the filesystem adapter, `ENOSPC` from a full
  disk, and a failed `fsync` are not provoked through the filesystem adapter; apart from the file
  size limit above, I/O errors are injected only in the simulator;
- a lossy failure of a directory sync: lossy failures are modelled for file syncs only.

## Store files on disk

The store directory is created with mode `0700` and its files with `0600`; the parent of every
directory the engine creates is synced. Store files must be regular files: a symlink or special file
in their place is refused before it is opened, and the opened handle must be the file at that path.

Reads stay within the committed content length and require a content record to match the requested
digest and length. Commits refuse to continue when the generation or page counter would overflow.

**Limitation:** checksums detect accidental damage, not deliberate forgery. A store crafted with
valid checksums but inconsistent structures is only partly validated today: publication cursors,
extent bounds and content digests are checked; every node reference must lie inside committed
metadata with an incarnation no newer than the current generation; and a traversal may load at most
as many nodes as the store has pages, so a shared or cyclic subtree fails instead of looping. Full
structural validation of hostile stores is not implemented yet.

## Concurrent views and conflict validation

A view records what it reads: each context read (including one that finds nothing) protects that
record, and each relationship query protects its whole queried range: one node, one direction and,
if given, one kind. The protection covers the whole range however much of it the caller consumed, and
it is registered before the first result is produced. Reads of retained history register no
protection: history is append-only, so a commit never changes an entry a view has read.

A commit from a view is validated against every logical write committed after the view began. If any
of them touched a protected record or range, the commit returns `Conflict`, naming the record, and
nothing is written. Writes that touch nothing the view observed do not conflict: two disjoint changes
prepared from the same basis both commit, and the later one is applied to the newest state, so
nothing the first commit wrote is lost. Preconditions on revisions and absence are
checked against the newest state as well.

The write sets of the most recent commits (4096 by default) are kept in memory for validation; the
window is bounded by number of commits, so its memory grows with the size of those commits' write sets. A view
older than that window is refused with an explicit `Conflict` saying the basis is too old; it is never
validated against incomplete history. Physical maintenance writes no logical keys and so never causes
a conflict.

## Commit ordering, admission and outcomes

Each store has one commit worker that validates, writes and publishes commits in order. `commit`
submits to it and waits; `submit` returns a ticket instead. An admitted commit runs to completion even
if its ticket or caller goes away, and until it resolves `outcome` reports `Pending` for its attempt.
The queue in front of the worker is bounded (64 by default); when it is full, `submit` and `commit`
fail with `Busy` without admitting anything, and reads continue normally.

`close` stops admission first (later `submit`/`commit` calls fail with a store-state error saying the
store is closing), then waits until every admitted commit has resolved, so no commit publishes after
`close` has returned, and no admitted commit still keeps the store owned. `close` waits as long as an
admitted commit takes: a commit blocked in I/O (for example a filesystem that never completes a
sync) keeps `close` waiting with it.

## Ownership

One process owns a store at a time, through an exclusive lock on `LOCK` taken at open and held until
the store handle and every view and content reader derived from it are dropped. A second opener gets
`Busy`. A read-write open creates `LOCK` if it is missing; creating a store creates it before the
data files.

### Read-only inspection open

`NativeStore::open_read_only` opens a store that no process owns for reading only. It takes the
lock on `LOCK` in shared mode and opens the data files without write access:

- While a process owns the store, a read-only open fails with `Busy`, as a read-write open does.
- While a read-only handle, or any view or content reader derived from it, is alive, read-write opens
  fail with `Busy`; further read-only opens succeed.
- It selects the committed state exactly as a read-write open does (both slots, the same validity
  and cursor checks, the same diagnostics, the same refusals), in memory only. It never writes,
  syncs, repairs, truncates or creates anything, including `LOCK`: without `LOCK` it refuses,
  because it could not keep an owner out.
- The handle offers views (records, relationships, history, content) and diagnostics. Staging,
  commits and attempt outcomes are not offered, and the engine refuses staging and commits on a
  read-only handle.

**Visible is not durable.** The previous owner may have exited after writing a publication record
but before syncing it. A read-only open sees that state, because the operating system still holds
it, but does not sync it, so a power loss before the next read-write open can still lose it. Only a
read-write open makes the recovered state durable. This is why a read-only handle does not answer
attempt outcomes.

The lock is an advisory `flock` lock: it excludes processes that use this engine, on local
filesystems. Network filesystems may not provide it.

## Readers and content integrity

A view reads the generation that was current when it began. Content streams one segment at a time
through a buffer of one segment. Each segment is read whole and checked against its SHA-256 before
any of its bytes are delivered, and the SHA-256 of the whole stream is checked before end of stream
is reported. After a failed check the reader returns an error naming the segment, and every later
read fails too; it never reports a complete stream.

Staging with a declared length and digest stops as soon as the stream exceeds the declared length
and compares length and digest at the end; a mismatch, a short stream or a source error produces no
staged content. Space written by a failed or abandoned staging lies beyond the committed content
length and is reused by later staging or after a restart.

## Not implemented yet

Space reuse, removal modes, and recovery from an uncertain outcome without reopening. (Concurrent
commit validation against views' observations is implemented; see "Concurrent views and conflict
validation" above.)
