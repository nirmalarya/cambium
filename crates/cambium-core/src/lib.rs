// `p![..]` builds the parameter Vec used by both the Vec and slice APIs of the Db layer.
#![allow(clippy::useless_vec)]
//! cambium-core: the Cambium server engine. Changesets in a write-ahead log,
//! content in a blob store, derived indexes (manifests, blame) computed on demand.
pub mod audit;
pub mod backup;
pub mod blob;
pub mod db;
pub mod derived;
pub mod engine;
pub mod gitimport;
pub mod limits;
pub mod log;
pub mod migrate;
pub mod review;
pub mod s3;
pub mod search;
pub mod text;
pub mod tokens;
pub mod tree;
pub use engine::{AclRule, Authz, Changeset, Engine, Error, FileChange, Perm, Principal};
