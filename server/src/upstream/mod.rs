//! Upstream routing: endpoint resolution, key pooling, circuit breaking.
//!
//! Re-exported here so callers depend on crate::upstream::KeyPool rather than
//! the concrete module path.

pub mod key_pool;

pub use key_pool::{KeyLease, KeyPool};
