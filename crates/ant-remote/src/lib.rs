//! Self-hosted remote execution for Ant (milestone M4, decision D-014).
//!
//! One crate, four roles: the [`protocol`] documents (v1), the
//! [`coordinator`] (blob store + job queue behind an authenticated HTTP API),
//! the [`worker`] (lease → materialize verified inputs → execute → upload
//! outputs → report), and the [`client`] (`ant remote run`). See
//! `docs/remote/protocol-v1.md` and `docs/remote/operations.md`.

pub mod auth;
pub mod blobs;
pub mod client;
pub mod coordinator;
pub mod gradle_cache;
pub mod http;
pub mod protocol;
pub mod ui;
pub mod worker;
