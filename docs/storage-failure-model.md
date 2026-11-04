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
the store is left untouched; the engine never initializes a new store over existing files. A record
of an unsupported format version, or one that requires features this build does not implement, is
refused without modification, whatever the other slot holds: an older record does not make a store
this build cannot read readable.

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

### Receipt guard

A caller that keeps receipt information independently of the store (its identity, and the highest
history position of a commit receipt it retained) can pass it as `StoreOptions::guard`. Once the
publication record is selected, and before anything is synced or written, an open (read-write or
read-only) with a guard is refused with `StorageError::GuardRefused` if the store has another
identity or its published logical history position is below the guarded minimum. The store's data
is left untouched; a read-write open takes the store's lock first, and so may create its lock file
if it was missing. The check compares history positions, not generations, which also advance without new
history. It is a rollback floor only: a store at or above the minimum may still hold a different
history than the one the receipts came from. Without a guard, opening is unchanged.

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
  opening, a read while streaming content, and each operation of a commit that reuses retired
  tree pages, retires others and grows the metadata file. An injected error fails the whole
  operation; a write that stops part-way is provoked only by the file size limit below.
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
- **Arbitrary bytes into every decoder:** fuzz targets (in `crates/cce-storage/fuzz`, run on demand
  with a nightly toolchain, not in CI) feed arbitrary input to each decoder of stored bytes and to a
  read-only open that reads a whole store image through. The ordinary tests replay the committed
  seed inputs and every saved finding through the same entry points.

Separately, real processes are killed and the store is reopened by a fresh process (process exit,
not power loss); one scenario run on the filesystem adapter and on the simulator must issue the
same persistence operations in the same order; and on a real filesystem, a file size limit makes a
commit's write stop part-way and fail, as on a full disk, which must fail the commit explicitly:
once in the history write to the content file, and once in a metadata page write of a commit that
reuses retired pages but needs more than are retired. After the second, the same process keeps
committing within the limit, and a fresh process sees the failed commit absent and can retry it.

The filesystem adapter repeats a positional read or write interrupted by a signal and continues a
short one where it stopped. A read that reaches the end of the file early is reported as an
integrity error, and a write that makes no progress as an I/O error, rather than repeated. Any other
error, such as a full disk or a device error, is reported without retrying; bytes written before it
stay written. A failed sync is never retried, an interrupted one included: an incomplete sync is
exactly the uncertain case the engine fences on (a failed data barrier rejects the commit, a failed
publication sync leaves its outcome unknown, and both fence writes until reopen), and retrying it
could turn an unknown outcome into an apparent success. Tests exercise this
through a test-only seam under the adapter's file handles, on real files: every read and write
interrupted or cut to a few bytes, at the adapter and through a store that stages, commits, reads
and is reopened; a full disk after a short write, at the adapter and in a commit's metadata page
write (the commit is rejected, its attempt absent, and the store keeps committing); a device error
on read; transfers that make no progress; and an interrupted sync, at the adapter and at each sync
of a commit (each fails the commit and fences writes; a fresh open settles the outcome).

When a case of a cut sweep fails (the commit sweeps, the space-reuse sweeps, the store-creation
sweep, or the faults-during-recovery sweep), its crash image (durable bytes, and the bytes still cached after a
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
- errors produced by a real filesystem: apart from the file size limit above, interruptions, short
  transfers, a full disk and device errors are injected by the test seam under the adapter's file
  handles, not provoked in the filesystem, and a real `fsync` failure is not provoked at all.
  Opening files, file metadata and directory syncs are not under the seam;
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
metadata with an incarnation no newer than the current generation; every loaded node holds only
keys, separators and entries within the limits every write enforces, which splitting relies on; and
a traversal may load at most as many nodes as the store has pages, so a shared or cyclic subtree
fails instead of looping. Every committed extent, including a manifest a commit retires, is read
within its format maximum and committed content and verified in chunks before it is held whole. A
crafted extent whose length and SHA-256 match real committed bytes is indistinguishable from a
valid one and is read whole. The traversal bound is the page count the store itself records, so a
crafted store with a large page count and a sparse metadata file can still cost work in proportion
to that count, one page at a time. Full structural validation of hostile stores is not implemented
yet.

## Requests the format cannot hold

Identifiers are at most 255 bytes, but a relationship's index keys hold four of them, and records
are limited in size (see the format document's key and record limits). A request containing a
relationship whose index keys would be too long, or a record too large for metadata, is refused
with `InvalidRequest` that names the limit exceeded and nothing of the request is applied. For a
relationship this happens before any change of the request is applied, so it is the answer even
when an earlier change would fail too.

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

The lock belongs to the open lock file, and a child process keeps a copy of every open file from the
moment it is created. A child that starts another program drops the copy when it does; a child that
does not keeps it for its whole life. So right after a handle closes, an open of the same store can
fail with `Busy` while a child of this process, or of another process that had the store open,
still holds the copy: briefly while a child is being started, for as long as it runs if it never
starts another program. Exclusion is never weakened, and nothing is written. Retrying the open is
safe; the engine itself never retries.

## Integrity inspection

`NativeReadOnly::verify` and the `cce-verify <store-dir> [--json] [--scrub]` command inspect a store
through the read-only open. They never write, sync or repair; the store files are
byte-identical afterwards. Because the read-only open is refused while another process owns the
store, inspection cannot run against a live owner.

The structure level checks, for the current publication record:

- both publication slots, reporting what each holds; a valid record in the wrong slot is a finding;
- every metadata page reachable from the root decodes and belongs to this store, lies within
  committed metadata, has the incarnation its parent recorded (no newer than the generation), and
  is reached only once; all leaves are at the same depth; every key and separator lies within the
  range its parent assigns;
- every record's key family, key components and identifiers, and its value encoding, and that its
  key and the whole record are within the 1800-byte limits every writer enforces;
- every relationship has both directional index entries, and every index entry matches a
  relationship;
- every context's bound content has a content entry for content that is still retained, and each
  retained content entry counts exactly the contexts that bind it; content a history entry binds has
  a content entry, which may record content that is no longer retained; every extent lies within
  committed content;
- history positions are exactly 1 through the committed position, and each history entry's attempt
  record points back to it with the fingerprint of the request bytes the entry stores; every attempt
  record points at an entry;
- every retirement record decodes and is no newer than the state; a retired tree page is retired
  once, lies among the tree's pages and is not reachable; every tree page is reachable or retired;
  and the content-file extents the state uses or has retired (manifests, history entries, retired
  extents and, at the scrub level, segments) do not overlap. Space that a staging wrote but no commit
  bound is not accounted for: abandoning a staging leaks its space.

It reads the history entries but no content bytes, and holds every record for the cross-checks, so
its memory grows with the metadata. A relationship whose source or target context is absent, never
created or since removed, is valid and not a finding: storage enforces no referential integrity and
removes no relationship when a context goes, only the caller's preconditions and the consistency of
relationship records with both indexes.

The scrub level (`VerifyLevel::Scrub`, `--scrub`) also reads every committed content stream: each
retained content entry's manifest must describe the content its key names, every segment must match its
SHA-256, the whole stream must match the content's digest and length, and every context's binding
and every history entry's binding of retained content must match its content's manifest; content
that is no longer retained is never read. Manifests and segments are read through the verified
read, one at a time; the report counts the content bytes read.

`--json` prints one JSON object per line in a deterministic order: a summary, both slots, the
recovery diagnostics, each finding (`check`, `location`, `detail`), and a result line. Findings name
pages by number and records by key family and a hash prefix of the key; output never contains stored
bodies, content, evidence or identifiers. Exit status: 0 consistent, 1 inconsistencies found, 2 the
store could not be inspected (missing, owned, unsupported or unreadable) or the arguments are wrong.

## Removal and live readers

A commit chooses how it treats views and content readers that may still read the state it
replaces (`ChangeSet::removal`):

- `PreserveViews` (the default): existing views and readers keep reading their state; the space the
  commit retires is reused only after they are gone.
- `RequireQuiescence`: before writing anything, the commit counts the live views and content
  readers of the store. If there are any, it fails with `ReadersActive` (the message gives the
  count only) and nothing changes: no write or sync, no published state, the attempt stays absent,
  and its staged content stays bound to its handles for a retry. The request's own basis view is
  consumed when the commit is admitted and is not counted; a content reader opened from it before
  that still is. While such a commit runs, new views wait and then read the state it published (or,
  if it failed, the unchanged state); reads through existing handles are unaffected. Answering
  `outcome` and reporting statistics read a pinned current state too, so they also wait
  (except `outcome` for an attempt that is itself still pending, which answers at once). A
  quiescent commit held up in I/O therefore holds up those calls for as long as it lasts. Those
  walks are not views: a quiescent commit does not count them as live readers, although their pins
  still keep the space they read from being reused. The count is
  store-wide and covers this process only: no other process can hold the store open, read-write or
  read-only, while it is owned.

## Readers and content integrity

When the last record using some content is removed or replaced, retained history still names the
content by digest and length, but reading it through a view that began after that commit fails
with `ContentNotRetained`. It never returns other bytes. A view, or a content reader, that began
before the commit still reads the content whole: its space is retired, not reused, while it can
still be reached.

A view reads the generation that was current when it began. Content streams one segment at a time
through a buffer of one segment. Each segment is read whole and checked against its SHA-256 before
any of its bytes are delivered, and the SHA-256 of the whole stream is checked before end of stream
is reported. After a failed check the reader returns an error naming the segment, and every later
read fails too; it never reports a complete stream.

Staging with a declared length and digest stops as soon as the stream exceeds the declared length
and compares length and digest at the end; a mismatch, a short stream or a source error produces no
staged content. Space written by a failed or abandoned staging lies beyond the committed content
length and is reused by later staging or after a restart.

## Maintenance

Maintenance is explicit: `NativeStore::maintain(task, budget)` runs one step, ordered with commits
under the same writer lock, on the newest published state, and publishes its result as a new
physical generation. It writes no logical record, adds no history entry (the history position does
not move) and records nothing a view's reads could conflict with. Opening a store never runs
maintenance and never repairs anything; read-only handles refuse it. A step that finds nothing to do
publishes nothing. The budget bounds what one step moves or retires and so the size of what it
publishes; finding that work reads the published state's index of content, history and retired
space, which is proportional to the metadata (as for the integrity check).

- `ReclaimContentHoles` retires content-file space below the committed length that no published
  structure reaches, no retirement record covers, and no staging of this process holds or has
  discarded, within the budget.
- `RebuildRelationshipIndexes` removes every relationship index entry and writes both entries of
  each relationship record again, in one publication; the old index is not consulted. It runs only
  when asked, for example after an integrity check reports index findings. Its memory grows with the
  size of the indexes.
- `Relocate` moves committed content streams, highest first, into retired space below them: each
  segment and a new manifest are copied, one segment at a time, into retired extents eligible for
  reuse that lie entirely below the piece being moved, and the old manifest and segments are
  retired. A stream that could move but does not fit what remains of the content budget is skipped
  and smaller ones are still considered; a later call moves it. One larger than the whole budget
  is counted in the report and keeps the task incomplete until a call with a larger budget moves
  it. A stream with no retired space below it is not left work. The copy runs without the writer lock, on a
  pinned generation (so nothing it reads can be reused meanwhile), into extents reserved as
  staging reserves them (so no staging can take them). Publication then takes the writer lock and
  applies a move to the newest state only if that content still has the manifest the move copied;
  any other move is dropped, its reserved extents return to the pool, and its copied bytes remain
  in retired space. Commits made during the copy are therefore never overwritten. The same step
  then moves tree pages: under the writer lock, like a commit, it rewrites the highest pages (at
  most the page budget) and their paths from the root into retired pages numbered below all of
  them. If there are too few such pages it tries half as many, so it never writes a fresh page and
  never extends the metadata file. Each part publishes only if it moved something. The page part
  holds the writer lock throughout because it writes only metadata and at most the page budget,
  about what a commit writes; the content part copies up to the content budget of stream bytes,
  which is why it copies without the lock.

What a step holds in memory, and what it reads:

- Every step reads and writes at most one tree page or one content segment per operation, and in all
  reads no more metadata than the file's tree pages once over.
- `ReclaimContentHoles` reads no content but manifests. It scans the whole published state whatever
  its budget, holding every extent in use (manifests, segments, history entries, retirement records,
  stagings), so its memory grows with that count.
- `RebuildRelationshipIndexes` reads no content. It holds every index key it removes and every entry
  it writes, so its memory grows with the size of the indexes.
- `Relocate` reads only the manifests and the content it moves. It holds the manifest of every
  committed stream while it plans, and lists every tree page with one key below it for the page
  part, so its memory grows with the number of streams and pages.

The tests measure the per-operation bounds and the content read through the I/O the steps make;
they do not see data a step keeps between operations, so the memory bounds above are stated, not
measured.

Every maintenance publication passes the same data barrier as a commit, so a crash at any point of a
step, whether the process exits or power is lost, recovers either the state before the step or the
state after it. A relocation that publishes both parts may also stop between them, with its content
moved and its pages not; that is a whole state too.

## Not implemented yet

Recovery from an uncertain outcome without reopening, and shrinking the store files: maintenance
moves data into retired space nearer the start of each file but never truncates a file.
(Concurrent commit validation against views' observations is implemented; see "Concurrent views and
conflict validation" above.)
