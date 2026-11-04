//! Test composition for the private storage adapters. Scenario assertions stay in their suites.

use cce_core::storage::{AttemptId, ContextId, StorageError};

use crate::NativeStore;
use crate::engine::{Inner, Options};
use crate::format::StoreId;
use crate::io::Io;

#[path = "../tests/common/resources.rs"]
pub(crate) mod resources;

pub(crate) fn create_on(io: Box<dyn Io>, id: StoreId, options: Options) -> Result<NativeStore, StorageError> {
    Inner::create(io, id, options).map(|inner| NativeStore { inner })
}

pub(crate) fn open_on(io: Box<dyn Io>, options: Options) -> Result<NativeStore, StorageError> {
    Inner::open(io, options).map(|inner| NativeStore { inner })
}

pub(crate) fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

pub(crate) fn attempt(s: &str) -> AttemptId {
    AttemptId::new(s.as_bytes().to_vec()).unwrap()
}
