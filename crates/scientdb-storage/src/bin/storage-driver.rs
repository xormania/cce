//! Thin process driver for end-to-end tests.
//!
//! Reads a line-oriented script on stdin and calls the logical storage port of the native engine.
//! It holds no expected state and never touches store files directly; the parent test process
//! supplies inputs and checks outputs independently.
//!
//! Script commands (tokens separated by single spaces; bytes are lowercase hex, `-` = empty/none):
//!
//! ```text
//! probe ok|fail|hang                      harness self-check; does not touch storage
//! create <dir> | open <dir> [<cache-nodes>]
//! stats                                   prints instrumentation counters and tree height
//! stage <name> hex <bytes>                stage literal bytes
//! stage <name> gen <len> <seed>           stage a generated stream (see `gen_byte`)
//! put-context <id> <expect> <body> <content-name|->
//! remove-context <id> <expect>
//! put-rel <rel-id> <expect> <kind> <source> <target>
//! remove-rel <rel-id> <expect>
//! evidence <label> <bytes>
//! commit <attempt>                        submits the changes accumulated since the last commit
//! outcome <attempt>
//! dump <id>[,<id>...]                     prints the committed state reachable from those ids
//! read-content <digest> <len>             reads content by logical reference through a new view
//! hold                                    prints `holding` and blocks until killed
//! close
//! ```
//!
//! `<expect>` is `any`, `absent`, or `rev:<n>`. Every command answers with lines ending in `end`.

use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use scientdb_kernel::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, ContentBinding, ContentDigest, ContentRef,
    ContextId, Direction, Evidence, HistoryPosition, Precondition, ReadView, RelationshipId,
    RelationshipKind, RelationshipQuery, Revision, StagedContent, StorageEngine,
};
use scientdb_storage::{NativeStore, StoreOptions};

/// Deterministic store identity for a path: an explicit, reproducible input to `create`.
fn store_id(dir: &str) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(dir.as_bytes());
    d[..16].try_into().expect("16 bytes")
}

/// Byte `i` of the deterministic generated stream for `seed`. Mirrored independently by tests.
fn gen_byte(seed: u64, i: u64) -> u8 {
    let mut x = seed ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x & 0xff) as u8
}

struct GenStream {
    seed: u64,
    pos: u64,
    len: u64,
}

impl Read for GenStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min((self.len - self.pos) as usize);
        for (k, b) in buf[..n].iter_mut().enumerate() {
            *b = gen_byte(self.seed, self.pos + k as u64);
        }
        self.pos += n as u64;
        Ok(n)
    }
}

fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".into();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if s == "-" {
        return Ok(Vec::new());
    }
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd hex length: {s}"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn expect(s: &str) -> Result<Precondition, String> {
    match s {
        "any" => Ok(Precondition::Any),
        "absent" => Ok(Precondition::Absent),
        _ => s
            .strip_prefix("rev:")
            .and_then(|n| n.parse().ok())
            .map(|n| Precondition::AtRevision(Revision(n)))
            .ok_or_else(|| format!("bad precondition: {s}")),
    }
}

fn expect_token(p: &Precondition) -> String {
    match p {
        Precondition::Any => "any".into(),
        Precondition::Absent => "absent".into(),
        Precondition::AtRevision(r) => format!("rev:{}", r.0),
    }
}

fn change_tokens(c: &Change) -> String {
    match c {
        Change::PutContext {
            id,
            expect,
            body,
            content,
        } => format!(
            "put-context {} {} {} {}",
            hex(id.as_bytes()),
            expect_token(expect),
            hex(body),
            content.map_or("-".to_string(), |i| i.to_string())
        ),
        Change::RemoveContext { id, expect } => {
            format!(
                "remove-context {} {}",
                hex(id.as_bytes()),
                expect_token(expect)
            )
        }
        Change::PutRelationship {
            id,
            expect,
            kind,
            source,
            target,
        } => format!(
            "put-rel {} {} {} {} {}",
            hex(id.as_bytes()),
            expect_token(expect),
            hex(kind.as_bytes()),
            hex(source.as_bytes()),
            hex(target.as_bytes())
        ),
        Change::RemoveRelationship { id, expect } => {
            format!("remove-rel {} {}", hex(id.as_bytes()), expect_token(expect))
        }
    }
}

struct Driver {
    store: Option<NativeStore>,
    staged: HashMap<String, StagedContent>,
    pending: ChangeSet,
    out: io::Stdout,
}

impl Driver {
    fn say(&mut self, line: &str) {
        let mut o = self.out.lock();
        let _ = writeln!(o, "{line}");
        let _ = o.flush();
    }

    fn store(&self) -> Result<&NativeStore, String> {
        self.store
            .as_ref()
            .ok_or_else(|| "no store open".to_string())
    }

    fn run(&mut self, line: &str) -> Result<(), String> {
        let t: Vec<&str> = line.split(' ').collect();
        let id = |s: &str| ContextId::new(s.as_bytes().to_vec()).map_err(|e| e.to_string());
        match t.as_slice() {
            ["probe", "ok"] => self.say("probe ok"),
            ["probe", "fail"] => {
                self.say("probe failing");
                std::process::exit(3);
            }
            ["probe", "hang"] => {
                self.say("probe hanging");
                loop {
                    std::thread::park();
                }
            }
            ["create", dir] => {
                self.store = Some(
                    NativeStore::create(Path::new(dir), store_id(dir))
                        .map_err(|e| e.to_string())?,
                );
                self.say("ok created");
            }
            ["open", dir] => {
                self.store = Some(NativeStore::open(Path::new(dir)).map_err(|e| e.to_string())?);
                self.say("ok opened");
            }
            ["open", dir, cache] => {
                let opts = StoreOptions {
                    cache_nodes: cache.parse().map_err(|_| "bad cache size")?,
                };
                self.store =
                    Some(NativeStore::open_with(Path::new(dir), opts).map_err(|e| e.to_string())?);
                self.say("ok opened");
            }
            ["stats"] => {
                let s = self.store()?.stats().map_err(|e| e.to_string())?;
                self.say(&format!(
                    "stats height={} pages_written={} page_bytes={} content_bytes={} misses={} evictions={} cache={}",
                    s.tree_height, s.pages_written, s.page_bytes_written, s.content_bytes_written,
                    s.node_cache_misses, s.node_cache_evictions, s.node_cache_capacity
                ));
            }
            ["stage", name, "hex", bytes] => {
                let bytes = unhex(bytes)?;
                let s = self
                    .store()?
                    .stage_content(&mut &bytes[..])
                    .map_err(|e| e.to_string())?;
                self.say(&format!("ok staged {name} {}", s.content.len));
                self.staged.insert(name.to_string(), s);
            }
            ["stage", name, "gen", len, seed] => {
                let len: u64 = len.parse().map_err(|_| "bad len")?;
                let seed: u64 = seed.parse().map_err(|_| "bad seed")?;
                let mut g = GenStream { seed, pos: 0, len };
                let s = self
                    .store()?
                    .stage_content(&mut g)
                    .map_err(|e| e.to_string())?;
                self.say(&format!("ok staged {name} {}", s.content.len));
                self.staged.insert(name.to_string(), s);
            }
            ["put-context", cid, exp, body, content] => {
                let content = if *content == "-" {
                    None
                } else {
                    let s = self.staged.get(*content).ok_or("unknown staged content")?;
                    self.pending.content.push(ContentBinding::from(s));
                    Some(self.pending.content.len() - 1)
                };
                self.pending.changes.push(Change::PutContext {
                    id: id(cid)?,
                    expect: expect(exp)?,
                    body: unhex(body)?,
                    content,
                });
                self.say("ok");
            }
            ["remove-context", cid, exp] => {
                self.pending.changes.push(Change::RemoveContext {
                    id: id(cid)?,
                    expect: expect(exp)?,
                });
                self.say("ok");
            }
            ["put-rel", rid, exp, kind, src, dst] => {
                self.pending.changes.push(Change::PutRelationship {
                    id: RelationshipId::new(rid.as_bytes().to_vec()).map_err(|e| e.to_string())?,
                    expect: expect(exp)?,
                    kind: RelationshipKind::new(kind.as_bytes().to_vec())
                        .map_err(|e| e.to_string())?,
                    source: id(src)?,
                    target: id(dst)?,
                });
                self.say("ok");
            }
            ["remove-rel", rid, exp] => {
                self.pending.changes.push(Change::RemoveRelationship {
                    id: RelationshipId::new(rid.as_bytes().to_vec()).map_err(|e| e.to_string())?,
                    expect: expect(exp)?,
                });
                self.say("ok");
            }
            ["evidence", label, bytes] => {
                self.pending.evidence.push(Evidence {
                    label: unhex(label)?,
                    bytes: unhex(bytes)?,
                });
                self.say("ok");
            }
            ["commit", attempt] => {
                let attempt =
                    AttemptId::new(attempt.as_bytes().to_vec()).map_err(|e| e.to_string())?;
                let changes = std::mem::take(&mut self.pending);
                let store = self.store()?;
                let basis = store.begin().map_err(|e| e.to_string())?;
                match store.commit(attempt, basis, changes) {
                    Ok(r) => self.say(&format!("committed {}", r.position.0)),
                    Err(e) => self.say(&format!("commit-error {e:?}")),
                }
            }
            ["outcome", attempt] => {
                let attempt =
                    AttemptId::new(attempt.as_bytes().to_vec()).map_err(|e| e.to_string())?;
                match self.store()?.outcome(&attempt).map_err(|e| e.to_string())? {
                    AttemptOutcome::Committed(r) => {
                        self.say(&format!("outcome committed {}", r.position.0))
                    }
                    AttemptOutcome::Pending => self.say("outcome pending"),
                    AttemptOutcome::Absent => self.say("outcome absent"),
                    AttemptOutcome::Unknown => self.say("outcome unknown"),
                }
            }
            ["dump", ids] => self.dump(ids)?,
            ["read-content", digest, len] => {
                let d: [u8; 32] = unhex(digest)?
                    .try_into()
                    .map_err(|_| "digest must be 32 bytes")?;
                let c = ContentRef {
                    digest: ContentDigest(d),
                    len: len.parse().map_err(|_| "bad len")?,
                };
                let mut view = self.store()?.begin().map_err(|e| e.to_string())?;
                match view.content(&c) {
                    Ok(mut r) => {
                        let mut bytes = Vec::new();
                        r.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                        self.say(&format!("content {}", hex(&bytes)));
                    }
                    Err(e) => self.say(&format!("content-error {e:?}")),
                }
            }
            ["hold"] => {
                self.say("holding");
                loop {
                    std::thread::park();
                }
            }
            ["close"] => {
                let store = self.store.take().ok_or("no store open")?;
                store.close().map_err(|e| e.to_string())?;
                self.say("ok closed");
            }
            _ => return Err(format!("unknown command: {line}")),
        }
        Ok(())
    }

    fn dump(&mut self, ids: &str) -> Result<(), String> {
        let mut lines = Vec::new();
        let mut view = self.store()?.begin().map_err(|e| e.to_string())?;
        for raw in ids.split(',') {
            let cid = ContextId::new(raw.as_bytes().to_vec()).map_err(|e| e.to_string())?;
            match view.context(&cid).map_err(|e| e.to_string())? {
                None => lines.push(format!("context {raw} absent")),
                Some(rec) => {
                    let content = rec
                        .content
                        .map_or("- -".into(), |c| format!("{} {}", hex(&c.digest.0), c.len));
                    lines.push(format!(
                        "context {raw} rev:{} {} {content}",
                        rec.revision.0,
                        hex(&rec.body)
                    ));
                    if let Some(c) = rec.content {
                        let mut r = view.content(&c).map_err(|e| e.to_string())?;
                        let mut bytes = Vec::new();
                        r.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                        lines.push(format!("content {raw} {}", hex(&bytes)));
                    }
                }
            }
            for (dir, name) in [(Direction::Outgoing, "out"), (Direction::Incoming, "in")] {
                let q = RelationshipQuery {
                    from: cid.clone(),
                    direction: dir,
                    kind: None,
                };
                for rel in view.relationships(&q).map_err(|e| e.to_string())? {
                    let rel = rel.map_err(|e| e.to_string())?;
                    lines.push(format!(
                        "rel {name} {raw} {} {} {} {} rev:{}",
                        hex(rel.id.as_bytes()),
                        hex(rel.kind.as_bytes()),
                        hex(rel.source.as_bytes()),
                        hex(rel.target.as_bytes()),
                        rel.revision.0
                    ));
                }
            }
        }
        for entry in view
            .history(HistoryPosition(0))
            .map_err(|e| e.to_string())?
        {
            let e = entry.map_err(|e| e.to_string())?;
            let p = e.position.0;
            lines.push(format!("history {p} {}", hex(e.attempt.as_bytes())));
            for (i, ev) in e.evidence.iter().enumerate() {
                lines.push(format!(
                    "evidence {p} {i} {} {}",
                    hex(&ev.label),
                    hex(&ev.bytes)
                ));
            }
            for (i, c) in e.changes.iter().enumerate() {
                lines.push(format!("change {p} {i} {}", change_tokens(c)));
            }
        }
        for l in lines {
            self.say(&l);
        }
        Ok(())
    }
}

fn main() -> ExitCode {
    let mut d = Driver {
        store: None,
        staged: HashMap::new(),
        pending: ChangeSet::default(),
        out: io::stdout(),
    };
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else {
            return ExitCode::from(2);
        };
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match d.run(&line) {
            Ok(()) => d.say("end"),
            Err(e) => {
                d.say(&format!("error {e}"));
                d.say("end");
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::SUCCESS
}
