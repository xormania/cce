//! The growing end-to-end story: host-supplied fixture inputs and the independent expected state.
//!
//! Expected values are computed here, in the test process, from the inputs alone. Nothing here is
//! written into or read from a store.

#![allow(dead_code)]

use std::collections::BTreeMap;

use super::{gen_bytes, hex, sha256};

pub struct Content {
    pub name: &'static str,
    pub bytes: Vec<u8>,
    /// Driver command that stages these bytes.
    pub stage: String,
}

pub struct ContextFixture {
    pub id: &'static str,
    pub body: &'static [u8],
    pub content: Option<usize>,
}

pub struct RelFixture {
    pub id: &'static str,
    pub kind: &'static str,
    pub source: &'static str,
    pub target: &'static str,
}

/// The first complete bundle: three contexts, two kinds, three relationships, small and large
/// content, and opaque evidence.
pub struct Bundle {
    pub attempt: &'static str,
    pub contents: Vec<Content>,
    pub contexts: Vec<ContextFixture>,
    pub rels: Vec<RelFixture>,
    pub evidence: Vec<(&'static [u8], &'static [u8])>,
}

/// Length of the generated large stream: well above any plausible single buffer.
pub const LARGE_LEN: u64 = (1 << 20) + 4099;
pub const LARGE_SEED: u64 = 0x5c1e_47db;

pub fn first_bundle() -> Bundle {
    let small = b"small payload for A".to_vec();
    Bundle {
        attempt: "attempt-0001",
        contents: vec![
            Content {
                name: "small",
                stage: format!("stage small hex {}", hex(&small)),
                bytes: small,
            },
            Content {
                name: "large",
                stage: format!("stage large gen {LARGE_LEN} {LARGE_SEED}"),
                bytes: gen_bytes(LARGE_SEED, LARGE_LEN),
            },
        ],
        contexts: vec![
            ContextFixture {
                id: "A",
                body: b"context A body",
                content: Some(0),
            },
            ContextFixture {
                id: "B",
                body: b"context B body",
                content: Some(1),
            },
            ContextFixture {
                id: "C",
                body: b"context C body",
                content: None,
            },
        ],
        rels: vec![
            RelFixture {
                id: "r1",
                kind: "depends",
                source: "A",
                target: "B",
            },
            RelFixture {
                id: "r2",
                kind: "cites",
                source: "A",
                target: "C",
            },
            RelFixture {
                id: "r3",
                kind: "depends",
                source: "C",
                target: "B",
            },
        ],
        evidence: vec![
            (
                b"decision",
                b"TEST-EVIDENCE: host says allow; storage must not interpret",
            ),
            (b"note", b"\x00\x01\xfe\xff opaque bytes"),
        ],
    }
}

impl Bundle {
    /// Driver commands that stage content and accumulate the change set (no commit).
    pub fn commands(&self) -> Vec<String> {
        let mut cmds: Vec<String> = self.contents.iter().map(|c| c.stage.clone()).collect();
        for c in &self.contexts {
            let content = c.content.map_or("-", |i| self.contents[i].name);
            cmds.push(format!(
                "put-context {} absent {} {content}",
                c.id,
                hex(c.body)
            ));
        }
        for r in &self.rels {
            cmds.push(format!(
                "put-rel {} absent {} {} {}",
                r.id, r.kind, r.source, r.target
            ));
        }
        for (label, bytes) in &self.evidence {
            cmds.push(format!("evidence {} {}", hex(label), hex(bytes)));
        }
        cmds
    }

    /// The exact `dump A,B,C` answer after this bundle commits at history position `pos`.
    pub fn expected_dump(&self, pos: u64) -> Vec<String> {
        let mut lines = Vec::new();
        for c in &self.contexts {
            match c.content {
                Some(i) => {
                    let b = &self.contents[i].bytes;
                    lines.push(format!(
                        "context {} rev:1 {} {} {}",
                        c.id,
                        hex(c.body),
                        hex(&sha256(b)),
                        b.len()
                    ));
                    lines.push(format!("content {} {}", c.id, hex(b)));
                }
                None => lines.push(format!("context {} rev:1 {} - -", c.id, hex(c.body))),
            }
            // Ordered by (kind, other endpoint, relationship id) per the port contract.
            for (dir, mine, other) in [("out", true, false), ("in", false, true)] {
                let mut sel: BTreeMap<(&str, &str, &str), &RelFixture> = BTreeMap::new();
                for r in &self.rels {
                    let (me, them) = if mine && !other {
                        (r.source, r.target)
                    } else {
                        (r.target, r.source)
                    };
                    if me == c.id {
                        sel.insert((r.kind, them, r.id), r);
                    }
                }
                for r in sel.values() {
                    lines.push(format!(
                        "rel {dir} {} {} {} {} {} rev:1",
                        c.id,
                        hex(r.id.as_bytes()),
                        hex(r.kind.as_bytes()),
                        hex(r.source.as_bytes()),
                        hex(r.target.as_bytes())
                    ));
                }
            }
        }
        lines.push(format!("history {pos} {}", hex(self.attempt.as_bytes())));
        for (i, (label, bytes)) in self.evidence.iter().enumerate() {
            lines.push(format!("evidence {pos} {i} {} {}", hex(label), hex(bytes)));
        }
        let mut i = 0;
        let mut content_idx = 0;
        for c in &self.contexts {
            let content = match c.content {
                Some(_) => {
                    content_idx += 1;
                    (content_idx - 1).to_string()
                }
                None => "-".into(),
            };
            lines.push(format!(
                "change {pos} {i} put-context {} absent {} {content}",
                hex(c.id.as_bytes()),
                hex(c.body)
            ));
            i += 1;
        }
        for r in &self.rels {
            lines.push(format!(
                "change {pos} {i} put-rel {} absent {} {} {}",
                hex(r.id.as_bytes()),
                hex(r.kind.as_bytes()),
                hex(r.source.as_bytes()),
                hex(r.target.as_bytes())
            ));
            i += 1;
        }
        lines
    }
}
