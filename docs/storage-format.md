# Storage format (version 1)

This describes the bytes the `scientdb-storage` engine writes today. It changes with the code; a
change that alters how existing bytes are read must change the version and either migrate or refuse
older stores without modifying them.

## Store directory

| File | Contents |
|---|---|
| `meta.sdb` | Metadata pages: two publication slots, then tree nodes. |
| `content.sdb` | Content bytes and retained history entries, addressed by offset. |
| `LOCK` | Empty file used for the exclusive ownership lock (`flock`-style, held while open). |

All integers are little-endian unless stated. Rust memory layout is never written directly.

## Pages (`meta.sdb`)

Pages are 4096 bytes. Page `n` starts at byte `n * 4096`. Pages 0 and 1 are publication slots;
tree nodes start at page 2.

Node page header (48 bytes), followed by the payload and zero padding:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `SDBP` |
| 4 | 2 | format version (1) |
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

Leaf payload: `count: u32`, then `count` entries of `key_len: u16, key, value_len: u16, value`,
with keys strictly increasing.

**Current limitation:** a tree is one leaf. A commit whose resulting leaf exceeds the page payload
(4048 bytes) is rejected with an explicit capacity error and changes nothing.

## Publication records (slots 0 and 1)

Generation `g` is written to slot `g mod 2`, so the previous generation's record is never
overwritten by the next commit. Record layout (76 bytes, rest of the page zero):

| Field | Size |
|---|---|
| magic `SDBD` | 4 |
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
| `07` | content SHA-256 | extent of the content bytes |

An extent is `offset: u64, length: u64, sha256: [u8; 32]` into `content.sdb`.

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

The attempt fingerprint is SHA-256 over `"scientdb.attempt.v1\0"`, the attempt id (length-prefixed),
and that canonical encoding. Retrying an attempt with an identical logical request returns the
original receipt; any difference is rejected as a mismatch.

## Allocation

Tree pages and content extents are allocated by advancing cursors (`next unallocated page`, the
content length). Nothing is reused or freed yet. Content staged but not committed lies beyond the
committed content length and is ignored, and later overwritten, after a restart.
