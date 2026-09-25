//! Upstream routing: endpoint resolution, key pooling, circuit breaking.
//!
//! Re-exported here so callers depend on crate::upstream::KeyPool rather than
//! the concrete module path.

pub mod circuit_breaker;
pub mod client;
pub mod key_pool;

pub use circuit_breaker::CircuitBreaker;
pub use client::{parse_usage_from_sse, UpstreamClient, UpstreamError, UpstreamStream, Usage};
pub use key_pool::{KeyLease, KeyPool};
