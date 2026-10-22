//! A document store that fails on demand, for the error paths.
//!
//! Compiled only for tests; the happy paths use `MemoryStorage` directly.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use tinystoragedrivers_core::{
    Capabilities, CollectionSpec, DocumentStore, Filter, Page, Precondition, Query, Sort,
    StorageError, Version, Versioned, WriteOptions, async_trait,
};

type Result<T> = std::result::Result<T, StorageError>;

/// What to break.
#[derive(Debug, Default, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Faults {
    pub(crate) ensure: bool,
    pub(crate) get: bool,
    pub(crate) put: bool,
    pub(crate) delete: bool,
    pub(crate) query: bool,
}

/// Delegates to `inner`, failing the operations named in `faults`.
///
/// `conflicts` forces that many version-checked writes to report a conflict
/// before they reach the inner store (`usize::MAX` means always).
#[derive(Debug)]
pub(crate) struct Faulty {
    pub(crate) inner: Arc<dyn DocumentStore>,
    pub(crate) faults: Faults,
    pub(crate) conflicts: AtomicUsize,
}

impl Faulty {
    pub(crate) fn new(inner: Arc<dyn DocumentStore>, faults: Faults) -> Arc<Self> {
        Arc::new(Self {
            inner,
            faults,
            conflicts: AtomicUsize::new(0),
        })
    }

    pub(crate) fn with_conflicts(inner: Arc<dyn DocumentStore>, conflicts: usize) -> Arc<Self> {
        Arc::new(Self {
            inner,
            faults: Faults::default(),
            conflicts: AtomicUsize::new(conflicts),
        })
    }
}

fn boom(what: &str) -> StorageError {
    StorageError::backend(format!("injected {what} failure"))
}

#[async_trait]
impl DocumentStore for Faulty {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn ensure_collection(&self, spec: &CollectionSpec) -> Result<()> {
        if self.faults.ensure {
            return Err(boom("ensure"));
        }
        self.inner.ensure_collection(spec).await
    }

    async fn get(&self, collection: &str, id: &str) -> Result<Option<Versioned<Value>>> {
        if self.faults.get {
            return Err(boom("get"));
        }
        self.inner.get(collection, id).await
    }

    async fn put(
        &self,
        collection: &str,
        id: &str,
        doc: Value,
        precondition: Precondition,
    ) -> Result<Version> {
        self.put_with(collection, id, doc, precondition, WriteOptions::default())
            .await
    }

    async fn put_with(
        &self,
        collection: &str,
        id: &str,
        doc: Value,
        precondition: Precondition,
        options: WriteOptions,
    ) -> Result<Version> {
        if self.faults.put {
            return Err(boom("put"));
        }
        if matches!(precondition, Precondition::Version(_)) {
            let left = self.conflicts.load(Ordering::SeqCst);
            if left > 0 {
                if left != usize::MAX {
                    self.conflicts.store(left - 1, Ordering::SeqCst);
                }
                return Err(StorageError::conflict("injected conflict"));
            }
        }
        self.inner
            .put_with(collection, id, doc, precondition, options)
            .await
    }

    async fn delete(&self, collection: &str, id: &str, precondition: Precondition) -> Result<bool> {
        if self.faults.delete {
            return Err(boom("delete"));
        }
        self.inner.delete(collection, id, precondition).await
    }

    async fn query(&self, collection: &str, query: &Query) -> Result<Page<Versioned<Value>>> {
        if self.faults.query {
            return Err(boom("query"));
        }
        self.inner.query(collection, query).await
    }

    async fn count(&self, collection: &str, filter: &Filter) -> Result<u64> {
        self.inner.count(collection, filter).await
    }

    async fn delete_where(&self, collection: &str, filter: &Filter) -> Result<u64> {
        self.inner.delete_where(collection, filter).await
    }

    async fn claim(
        &self,
        collection: &str,
        filter: &Filter,
        sort: &[Sort],
        patch: &Value,
    ) -> Result<Option<Versioned<Value>>> {
        self.inner.claim(collection, filter, sort, patch).await
    }

    async fn drop_collection(&self, collection: &str) -> Result<()> {
        self.inner.drop_collection(collection).await
    }
}
