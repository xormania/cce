//! Logical types and the storage port for CCE.
//!
//! This crate defines what storage must do, not how. It exposes no pages, files, offsets or
//! physical generations; implementations live in other crates and depend on this one.

pub mod storage;
