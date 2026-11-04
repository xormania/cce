# Storage format (version 1)

This describes the bytes the `cce-storage` engine writes today. It changes with the code; a
change that alters how existing bytes are read must change the version and either migrate or refuse
older stores without modifying them.

## Store directory

| File | Contents |
|---|---|
| `meta.cce` | Metadata pages: two publication slots, then tree nodes. |
| `content.cce` | Content bytes and retained history entries, addressed by offset. |
| `LOCK` | Empty file used for the exclusive ownership lock (`flock`-style, held while open). |

All integers are little-endian unless stated. Rust memory layout is never written directly.

## Pages (`meta.cce`)

Pages are 4096 bytes. Page `n` starts at byte `n * 4096`. Pages 0 and 1 are publication slots;
tree nodes start at page 2.

Node page header (48 bytes), followed by the payload and zero padding:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `CCEP` |
| 4 | 2 | format version (5) |
| 6 | 1 | page kind (1 = leaf) |
| 7 | 1 | reserved (0) |
| 8 | 16 | store identity |
| 24 | 8 | page number |
| 32 | 8 | incarnation (the generation that wrote the page) |
| 40 | 4 | payload length |
| 44 | 4 | CRC-32 (IEEE) of header (with this field zeroed) and payload |

A page is accepted only if magic, version, checksum, store identity, page number, incarnation and
kind all match what the referring structure expects. A page written to a reused location by a later
generation therefore cannot be mistaken for its predecessor.

Node payload (page kind 1), first byte is the node tag:

- **Leaf (tag 1):** `count: u32`, then `count` entries of `key_len: u16, key, value_len: u16, value`,
  keys strictly increasing.
- **Branch (tag 2):** `children: u32` (at least 2), child 0 as `page: u64, incarnation: u64`, then for
  each further child `sep_len: u16, separator, page: u64, incarnation: u64`. Separators are strictly
  increasing; child `i` holds keys `k` with `sep[i-1] <= k < sep[i]`.

The metadata tree is a copy-on-write B+tree. A commit copies only the nodes on the paths it touches,
writes each of them to a fresh page (children before parents) and shares every untouched subtree
with the previous generation. Nodes split by encoded size; the root grows when it splits. Deletion
removes empty nodes, merges a leaf under a quarter full into an adjacent sibling when the merged leaf
fits, and collapses a root branch with one child; there is no other minimum-occupancy rule. A key is
at most 1024 bytes and a key plus value at most 1800 bytes, so every leaf holds at least two
entries. Readers walk from the root with a cursor that keeps one root-to-leaf path and loads leaves
as it reaches them; trees deeper than 32 levels are rejected as damaged.

## Publication records (slots 0 and 1)

Generation `g` is written to slot `g mod 2`, so the previous generation's record is never
overwritten by the next commit. Record layout (76 bytes, rest of the page zero):

| Field | Size |
|---|---|
| magic `CCED` | 4 |
| format version | 2 |
| store identity | 16 |
| generation | 8 |
| root page, root incarnation | 8 + 8 |
| next unallocated page | 8 |
| committed content length | 8 |
| last history position | 8 |
| CRC-32 of the preceding fields | 4 |

## Logical key space

One ordered key space holds all record families. Keys are a prefix byte followed by components;
each component escapes `0x00` as `00 FF` and ends with `00 00`, so byte order of encoded keys equals
tuple order of the raw components.

| Prefix | Components | Value |
|---|---|---|
| `01` | context id | revision `u64`, body (`u32` length + bytes), content flag + extent |
| `02` | relationship id | revision `u64`, kind, source, target (each `u32` length + bytes) |
| `03` | source, kind, target, relationship id | empty — outgoing index |
| `04` | target, kind, source, relationship id | empty — incoming index |
| `05` | history position (`u64` big-endian) | extent of the history entry |
| `06` | attempt id | history position `u64`, request fingerprint (32 bytes) |
| `07` | content SHA-256 | `01`, extent of the content's manifest, number of records using it (`u64`); or `00`: committed once, no longer retained |
| `08` | retiring generation (`u64` big-endian), kind `00`, page (`u64` big-endian) | incarnation `u64` of the retired page |
| `08` | retiring generation (`u64` big-endian), kind `01`, offset (`u64` big-endian) | length `u64` of the retired content extent |

Retirement keys are fixed-width, not escaped components, so they sort by retiring generation.

An extent is `offset: u64, length: u64, sha256: [u8; 32]` into `content.cce`.

## Content

Content is stored as segments of at most 65536 bytes, each written to its own allocation in
`content.cce`, followed by a manifest:

| Field | Size |
|---|---|
| version (1) | 1 |
| SHA-256 of the whole stream | 32 |
| stream length | 8 |
| segment count | 4 |
| per segment: offset, length, SHA-256 | 8 + 8 + 32 |

The manifest is valid only if every segment is 1..65536 bytes and the lengths add up to the stream
length. Context records hold only the logical reference (SHA-256 and length); the physical layout is
found through the `07` record.

A record's revision is the history position of the commit that last changed it. Revisions therefore
strictly increase for an identity and are never reused, even after the record is removed and created
again. A record's key plus value is limited to 1800 bytes; larger data belongs in content.

## History entries and attempt fingerprints

A history entry is `position: u64`, attempt id, and the canonical request encoding: a version byte
(1), the changes in submitted order (tagged; identities, preconditions, bodies, content indexes as `u64`), the
bound content as `(sha256, length)` pairs, and the supplied evidence as `(label, bytes)` pairs.
Staging tokens and physical locations are not part of it. Every content index must refer to a
binding of the same request; a request with an out-of-range index is rejected before it is
fingerprinted. History entries return the bound content references with the changes.

The attempt fingerprint is SHA-256 over `"cce.attempt.v1\0"`, the attempt id (length-prefixed),
and that canonical encoding. Retrying an attempt with an identical logical request returns the
original receipt; any difference is rejected as a mismatch.

## Allocation

Staging places each segment and its manifest in the smallest retired content extent that fits,
else at the content length. It may reuse an extent retired at generation `r` only if `r` is at most
one generation before the current one (so neither publication slot reaches it while it is
written) and no live view or content reader pins a generation older than `r`. A staging holds the
extents it took until a commit binding it publishes, which deletes their retirement records; the
unused rest of a longer extent stays retired under its original generation. Extents held by a
staging that is not committed (pending, or not yet given up) are not handed out again in that
process; a failed staging returns them at once.

`discard_staged` gives up staged content that will not be committed. Retired extents the staging
took return to the pool at once, since their retirement records were never consumed; the extents
it appended are retired by the next commit, at that commit's generation. Content bound by an
admitted commit cannot be discarded until that commit resolves (`InvalidRequest`), and committed,
already discarded or foreign handles are refused (`ForeignHandle`) without effect.

A content entry counts the records that use the content. When a commit leaves it with none (the
last such record is removed or replaced), the entry becomes a removed entry and the commit retires
the content's manifest and segments (prefix `08`, kind `01`). A commit that binds a second staging
of content that is already committed, or binds content no change uses, retires that staging too:
no published state reaches it. Committing the same bytes again later indexes them afresh.

Content staged but neither committed nor discarded keeps its space for the life of the process.
After a restart nothing refers to it: staged bytes beyond the committed content length are
overwritten later, and retired extents a staging was using are still recorded as retired and
reused. Staged bytes that a later commit's content length came to cover, and the appended space of
a discarded staging whose retirement no commit published before a crash, are not reclaimed by
opening or committing. An explicit maintenance step (`NativeStore::maintain` with
`ReclaimContentHoles`) finds content-file space below the committed length that no published
structure reaches, no retirement record covers and no staging of the process holds, and retires
it in a new physical generation (no history is added); staging then reuses it under the rule above.

Tree pages are allocated from retired pages first, then by advancing the next unallocated page. A
commit producing generation `n` may reuse a page retired at generation `r` only if `r <= n - 2` and
no live view or content reader pins a generation older than `r`. The first condition keeps the page
unreachable from both publication slots while the commit is unpublished, so recovery, including a
fall back to the previous record, never follows a reference into a reused page. The second keeps it
unreachable from every live reader. A reused page is written with the new generation as its
incarnation; pages and cached nodes are identified by page and incarnation, so a stale reference
fails its check instead of reading the new contents. Reusing a page deletes its retirement record
in the same commit.

Each commit records, in the tree it publishes, every tree page it stopped referencing: the pages it
copied in order to change them, under prefix `08` with the new generation as the retiring
generation. Recording, consuming and returning those records change the tree in turn, which can
replace further pages and change how many pages the commit writes, so the commit repeats until a
round changes nothing (at most sixteen rounds, otherwise the commit is rejected). The
records therefore cover their own pages, and every page below the next unallocated page is either
reachable from the published root or recorded as retired. The records are part of the published
state, so recovery always finds the retirement records that match the state it selects.

Live views and content readers pin the generation they read. A content reader keeps its view's
pin after the view is dropped. Pins exist only in the owning process, and no other process can hold
the store while it is owned.
