//! The logical storage port.
//!
//! A trusted host reads a consistent basis through a [`ReadView`], prepares one complete
//! [`ChangeSet`], and submits it with a stable [`AttemptId`]. Storage records the supplied
//! changes and evidence atomically; it never evaluates whether a decision was correct.

use std::fmt;
use std::io::Read;

/// Maximum length in bytes of any logical identifier.
pub const MAX_ID_LEN: usize = 255;

macro_rules! logical_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Vec<u8>);

        impl $name {
            /// Creates an identifier from caller-supplied bytes.
            pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, StorageError> {
                let bytes = bytes.into();
                if bytes.is_empty() || bytes.len() > MAX_ID_LEN {
                    return Err(StorageError::InvalidRequest(format!(
                        "{} must be 1..={MAX_ID_LEN} bytes, got {}",
                        stringify!($name),
                        bytes.len()
                    )));
                }
                Ok(Self(bytes))
            }

            /// The identifier's bytes.
            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), String::from_utf8_lossy(&self.0))
            }
        }
    };
}

logical_id!(
    /// Logical identity of a context record. Never a physical address.
    ContextId
);
logical_id!(
    /// Logical identity of one recorded relationship.
    RelationshipId
);
logical_id!(
    /// Kind of a relationship. Storage attaches no semantics to kinds.
    RelationshipKind
);
logical_id!(
    /// Caller-chosen identity of one commit attempt, stable across retries.
    AttemptId
);

/// Logical revision of a record: increments on each committed change to that record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(pub u64);

/// Position in retained logical history. Unrelated to physical generations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HistoryPosition(pub u64);

/// SHA-256 digest of a content byte stream.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentDigest(pub [u8; 32]);

impl fmt::Debug for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ContentDigest(")?;
        for b in &self.0[..8] {
            write!(f, "{b:02x}")?;
        }
        f.write_str("…)")
    }
}

/// A logical reference to committed content: what the bytes are, not where they live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContentRef {
    pub digest: ContentDigest,
    pub len: u64,
}

/// Opaque decision evidence supplied by the host. Stored and returned byte-for-byte.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Evidence {
    /// Host-chosen label. Storage does not interpret it.
    pub label: Vec<u8>,
    pub bytes: Vec<u8>,
}

/// Expected logical state of a record or relationship when the change is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Precondition {
    /// No expectation beyond what the read basis observed.
    Any,
    /// The target must not exist.
    Absent,
    /// The target must exist at exactly this revision.
    AtRevision(Revision),
}

/// Content prepared by [`StorageEngine::stage_content`] and not yet accepted into custody.
///
/// The handle is bound to the store instance that produced it.
#[derive(Debug)]
pub struct StagedContent {
    /// Logical description of the staged bytes.
    pub content: ContentRef,
    /// Opaque token meaningful only to the issuing store instance.
    pub token: StagingToken,
}

/// Opaque, store-instance-bound staging token. Excluded from attempt fingerprints.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StagingToken(pub Vec<u8>);

/// One logical mutation. Names logical identities only; never pages or index entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Create or replace a context record.
    PutContext {
        id: ContextId,
        expect: Precondition,
        body: Vec<u8>,
        /// Index into [`ChangeSet::content`] of the staged content bound to this record.
        content: Option<usize>,
    },
    /// Remove a context record.
    RemoveContext { id: ContextId, expect: Precondition },
    /// Create or replace one relationship. Both directional access paths follow from it.
    PutRelationship {
        id: RelationshipId,
        expect: Precondition,
        kind: RelationshipKind,
        source: ContextId,
        target: ContextId,
    },
    /// Remove one relationship and its access paths.
    RemoveRelationship {
        id: RelationshipId,
        expect: Precondition,
    },
}

/// One complete logical change submitted atomically.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub changes: Vec<Change>,
    /// Content bound by this change set, in the order referenced by [`Change::PutContext`].
    pub content: Vec<ContentBinding>,
    /// Supplied decision evidence recorded with this change, in order.
    pub evidence: Vec<Evidence>,
    /// How this commit treats live views and content readers. Part of the request: an attempt
    /// retried with a different mode is a different request.
    pub removal: RemovalMode,
}

/// How a commit that removes or replaces records treats views and content readers that may still
/// read the state it replaces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RemovalMode {
    /// Existing views and content readers keep reading their state; storage reuses the space the
    /// commit retires only after they are gone.
    #[default]
    PreserveViews,
    /// Commit only while no other view or content reader of the store is live. Otherwise nothing
    /// is committed and the commit fails with [`CommitError::ReadersActive`]; once those readers
    /// are gone, the same request can be prepared and committed. The mode applies to every commit
    /// that carries it, whether or not it removes anything. The check is store-wide: it counts
    /// every live view and reader of the owning process, not only those that could reach a
    /// removed record (no other process can hold the store open while it is owned).
    RequireQuiescence,
}

/// A staged content item bound into a change set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentBinding {
    pub content: ContentRef,
    pub token: StagingToken,
}

impl From<&StagedContent> for ContentBinding {
    fn from(s: &StagedContent) -> Self {
        Self {
            content: s.content,
            token: s.token.clone(),
        }
    }
}

/// A committed context record as read through a view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextRecord {
    pub id: ContextId,
    pub revision: Revision,
    pub body: Vec<u8>,
    pub content: Option<ContentRef>,
}

/// A committed relationship.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Relationship {
    pub id: RelationshipId,
    pub kind: RelationshipKind,
    pub source: ContextId,
    pub target: ContextId,
    pub revision: Revision,
}

/// Direction of relationship navigation from a context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Relationships whose source is the given context.
    Outgoing,
    /// Relationships whose target is the given context.
    Incoming,
}

/// A relationship range query. An empty result is still an observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelationshipQuery {
    pub from: ContextId,
    pub direction: Direction,
    /// Restrict to one kind; `None` means all kinds.
    pub kind: Option<RelationshipKind>,
    /// Resume strictly after this position (exclusive); `None` starts at the beginning.
    pub after: Option<RelationshipPosition>,
}

/// A position in a relationship range, in result order: (kind, other endpoint, relationship id).
/// Use the last relationship returned to resume a partially consumed query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelationshipPosition {
    pub kind: RelationshipKind,
    /// The endpoint that is not `RelationshipQuery::from`.
    pub other: ContextId,
    pub id: RelationshipId,
}

/// One retained logical history entry: the record of one committed attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub position: HistoryPosition,
    pub attempt: AttemptId,
    pub evidence: Vec<Evidence>,
    pub changes: Vec<Change>,
    /// Content bound by this commit; [`Change::PutContext::content`] indexes into it.
    pub content: Vec<ContentRef>,
}

/// Historical receipt of a committed attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitReceipt {
    pub attempt: AttemptId,
    pub position: HistoryPosition,
}

/// Authoritative outcome of an attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    Committed(CommitReceipt),
    /// Admitted and not yet resolved; ask again.
    Pending,
    /// Not committed, within the supported receipt lifetime.
    Absent,
    /// Storage cannot currently establish the outcome (for example after an uncertain sync).
    Unknown,
}

/// Errors from storage operations. New kinds of error may be added.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorageError {
    /// The operation is not implemented yet.
    NotImplemented(&'static str),
    InvalidRequest(String),
    /// A store already exists, or a required store is missing.
    StoreState(String),
    /// Another process owns the store.
    Busy(String),
    /// The store's bytes are damaged or inconsistent.
    Integrity(String),
    /// The store uses an unsupported format.
    Unsupported(String),
    Io(String),
    /// The handle/view/token belongs to a different store or open instance.
    ForeignHandle,
    /// The content was committed once, but no record uses it any more and storage no longer keeps
    /// its bytes. Retained history may still name it by digest and length.
    ContentNotRetained,
    /// The request is valid, but a configured resource limit leaves no room for it now. Nothing
    /// was changed; it can succeed once the resource is released (the message says what releases
    /// it).
    ResourceExhausted(String),
    /// An open guarded by retained receipt information was refused: the store has another
    /// identity, or its published history position is below the retained minimum. The store's
    /// data was left untouched (a read-write open may have created the store's lock file).
    GuardRefused(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for StorageError {}

/// Errors from [`StorageEngine::commit`]. New kinds of error may be added.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommitError {
    /// A logical dependency changed; nothing committed.
    Conflict(String),
    /// The attempt identity was already used for a different request; nothing committed.
    AttemptMismatch,
    /// Nothing committed for the stated reason.
    Rejected(StorageError),
    /// The outcome cannot be established now; reconcile with [`StorageEngine::outcome`].
    Indeterminate(String),
    /// [`RemovalMode::RequireQuiescence`] and other views or content readers are live; nothing
    /// committed. The message says how many.
    ReadersActive(String),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for CommitError {}

/// A bounded reader over committed content.
pub trait ContentReader: Read + Send {}

/// A consistent view of one committed state. Holds whatever protection the engine needs.
pub trait ReadView: Send {
    /// Reads a context record; absence is an observation.
    fn context(&mut self, id: &ContextId) -> Result<Option<ContextRecord>, StorageError>;

    /// Relationships matching the query, ordered by (kind, other endpoint, relationship id).
    fn relationships(
        &mut self,
        query: &RelationshipQuery,
    ) -> Result<Box<dyn Iterator<Item = Result<Relationship, StorageError>> + '_>, StorageError>;

    /// Retained history from `start`, in position order.
    fn history(
        &mut self,
        start: HistoryPosition,
    ) -> Result<Box<dyn Iterator<Item = Result<HistoryEntry, StorageError>> + '_>, StorageError>;

    /// Opens a reader over committed content. The reader remains valid after the view drops.
    ///
    /// Content that no record in this view uses any more, and whose bytes storage no longer keeps,
    /// fails with [`StorageError::ContentNotRetained`] even though history may still name it.
    fn content(&mut self, content: &ContentRef) -> Result<Box<dyn ContentReader>, StorageError>;
}

/// The storage port implemented by native storage.
pub trait StorageEngine: Send + Sync {
    type View: ReadView;

    /// Begins a consistent view of the latest committed state.
    fn begin(&self) -> Result<Self::View, StorageError>;

    /// Streams content into staging. Staged content is not accepted context.
    fn stage_content(&self, bytes: &mut dyn Read) -> Result<StagedContent, StorageError>;

    /// Streams content into staging and checks it against the caller's declared length and digest.
    /// A stream that is longer, shorter or different fails without producing staged content.
    fn stage_content_expecting(
        &self,
        bytes: &mut dyn Read,
        expected: &ContentRef,
    ) -> Result<StagedContent, StorageError>;

    /// Gives up staged content that will not be committed, so its space can be reused. Consumes
    /// the handle. Content bound by an admitted commit cannot be discarded until that commit
    /// resolves; content already committed, already discarded, or staged by another store or open
    /// instance is refused without effect. Staged content that is neither committed nor
    /// discarded keeps its space until the store is reopened.
    fn discard_staged(&self, staged: StagedContent) -> Result<(), StorageError>;

    /// Atomically commits one complete change against the given basis.
    fn commit(
        &self,
        attempt: AttemptId,
        basis: Self::View,
        changes: ChangeSet,
    ) -> Result<CommitReceipt, CommitError>;

    /// The authoritative outcome of an attempt.
    fn outcome(&self, attempt: &AttemptId) -> Result<AttemptOutcome, StorageError>;

    /// Shuts down, reporting any failure to complete outstanding work.
    fn close(self) -> Result<(), StorageError>
    where
        Self: Sized;
}
