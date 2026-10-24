//! tinybox on the host's `tinystoragedrivers` ports.
//!
//! The backend (`SQLite`, `MongoDB`, files, memory) and the tenant scope are the
//! host's choice, made when it builds the [`DocumentStore`] handle. Nothing here
//! names a database.
//!
//! - [`DocumentBoxStore`]: box records as documents, behind
//!   [`tinybox_core::Store`].
//! - [`DocumentIndexStore`]: the jail registry's index as one document, behind
//!   [`tinybox_jail::IndexStore`].
//!
//! - [`storage_schema`]: defined in `tinybox_bus::storage`; the `box` owner's collections (`box.boxes`,
//!   `box.jail_index`) and the v1 to v2 steps. The host composes it into its
//!   registry, declares the collections, runs the migration, and grants the
//!   module exactly these items (`ScopeClass::Module`). The stores here never
//!   declare a collection themselves.
//!
//! Both traits are synchronous, so each store owns a [`Blocking`] bridge that
//! runs the port's futures on a dedicated runtime thread. That works from inside
//! or outside a tokio runtime.
//!
//! [`DocumentStore`]: tinystoragedrivers_core::DocumentStore
//! [`Blocking`]: tinystoragedrivers_core::Blocking

mod box_store;
mod index_store;
mod record;

pub use box_store::DocumentBoxStore;
pub use index_store::DocumentIndexStore;
pub use tinybox_bus::storage::{BOX_COLLECTION, JAIL_INDEX_COLLECTION, storage_schema};

#[cfg(test)]
#[path = "faulty_tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod faulty;
