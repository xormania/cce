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
| 4 | 2 | format version (4) |
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
| `07` | content SHA-256 | extent of the content's manifest |

An extent is `offset: u64, length: u64, sha256: [u8; 32]` into `content.cce`.

### Key and record limits

A key is 1 byte of prefix plus, per component, its length in bytes plus one more byte for each
zero byte (escaped as `00 FF`) plus the two-byte terminator. Keys longer than 1024 bytes are
refused, and so are key plus value longer than 1800 bytes.

- Context, relationship and attempt keys hold one identifier of at most 255 bytes: at most 513
  bytes even when every byte is zero, so they always fit.
- A relationship's outgoing and incoming index keys (`03`, `04`) hold its source, kind, target and
  id: 1 + Σ(length + zero bytes + 2). They fit only while the four identifiers' lengths plus their
  zero bytes total at most 1015 bytes. Four identifiers without zero bytes can total up to 1020, so
  long identifiers, and zero-rich ones much sooner, can exceed this. Such a relationship is refused
  before anything of its request is applied, with `InvalidRequest` naming the relationship, its key
  length and the key limit.
- A context record's value holds its body, so the largest accepted body is 1800 bytes minus the key
  (1 + id length + zero bytes in the id + 2) and the value's fixed fields. It shrinks by up to 255
  bytes for a zero-rich id; larger data belongs in content.

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

Tree pages and content extents are allocated by advancing cursors (`next unallocated page`, the
content length). Nothing is reused or freed yet. Content staged but not committed lies beyond the
committed content length and is ignored, and later overwritten, after a restart.
