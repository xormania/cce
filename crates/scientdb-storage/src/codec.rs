//! Explicit little-endian encoding with bounds-checked decoding.
//!
//! Every decoder validates lengths before slicing and never trusts a length field to size an
//! allocation beyond the bytes actually present.

use scientdb_kernel::storage::StorageError;

pub(crate) fn corrupt(what: impl Into<String>) -> StorageError {
    StorageError::Integrity(what.into())
}

#[derive(Default)]
pub(crate) struct Enc {
    pub buf: Vec<u8>,
}

impl Enc {
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn raw(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }
    /// u32 length prefix + bytes.
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(u32::try_from(v.len()).expect("field over 4 GiB"))
            .raw(v)
    }
}

pub(crate) struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
    what: &'static str,
}

impl<'a> Dec<'a> {
    pub fn new(buf: &'a [u8], what: &'static str) -> Self {
        Self { buf, pos: 0, what }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], StorageError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or_else(|| corrupt(format!("{}: truncated at {} (+{n})", self.what, self.pos)))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, StorageError> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, StorageError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }
    pub fn u32(&mut self) -> Result<u32, StorageError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    pub fn u64(&mut self) -> Result<u64, StorageError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], StorageError> {
        self.take(n)
    }
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], StorageError> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], StorageError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    /// Bounded element count: each element needs at least `min_elem` bytes.
    pub fn count(&mut self, min_elem: usize) -> Result<usize, StorageError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_elem.max(1)) > self.remaining() {
            return Err(corrupt(format!(
                "{}: count {n} exceeds remaining bytes",
                self.what
            )));
        }
        Ok(n)
    }
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub fn finish(self) -> Result<(), StorageError> {
        if self.pos != self.buf.len() {
            return Err(corrupt(format!(
                "{}: {} trailing bytes",
                self.what,
                self.remaining()
            )));
        }
        Ok(())
    }
}

/// Order-preserving encoding of a byte-string component: `0x00` is escaped as `00 FF` and the
/// component ends with `00 00`. Encoded tuples compare exactly like the tuples of raw components.
pub(crate) fn key_component(out: &mut Vec<u8>, c: &[u8]) {
    for &b in c {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

/// Splits an encoded key (after its one-byte prefix) back into components.
pub(crate) fn key_components(key: &[u8]) -> Result<Vec<Vec<u8>>, StorageError> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut i = 0;
    while i < key.len() {
        let b = key[i];
        if b == 0 {
            match key.get(i + 1) {
                Some(0xFF) => cur.push(0),
                Some(0) => out.push(std::mem::take(&mut cur)),
                _ => return Err(corrupt("key: bad escape")),
            }
            i += 2;
        } else {
            cur.push(b);
            i += 1;
        }
    }
    if !cur.is_empty() {
        return Err(corrupt("key: unterminated component"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(parts: &[&[u8]]) -> Vec<u8> {
        let mut k = Vec::new();
        for p in parts {
            key_component(&mut k, p);
        }
        k
    }

    #[test]
    fn key_encoding_preserves_tuple_order_including_prefixes_and_zero_bytes() {
        let tuples: Vec<Vec<&[u8]>> = vec![
            vec![b"A", b"x"],
            vec![b"A", b"x\x00"],
            vec![b"A", b"xy"],
            vec![b"AB", b""],
            vec![b"A\x00", b"z"],
            vec![b"", b"q"],
            vec![b"B", b"a"],
        ];
        for a in &tuples {
            for b in &tuples {
                assert_eq!(a.cmp(b), enc(a).cmp(&enc(b)), "{a:?} vs {b:?}");
            }
            assert_eq!(
                key_components(&enc(a)).unwrap(),
                a.iter().map(|p| p.to_vec()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn decoder_rejects_truncation_overlong_counts_and_trailing_bytes() {
        let mut e = Enc::default();
        e.u32(5).raw(b"abc");
        let mut d = Dec::new(&e.buf, "t");
        assert!(matches!(d.bytes(), Err(StorageError::Integrity(_))));
        let mut d = Dec::new(&[0xff, 0xff, 0xff, 0x7f], "t");
        assert!(d.count(1).is_err());
        let ok = [1u8, 0, 0, 0, 9];
        let mut d = Dec::new(&ok, "t");
        assert_eq!(d.count(1).unwrap(), 1);
        assert_eq!(d.u8().unwrap(), 9);
        assert!(d.finish().is_ok());
        let mut d = Dec::new(&[1, 2], "t");
        d.u8().unwrap();
        assert!(d.finish().is_err());
    }
}
