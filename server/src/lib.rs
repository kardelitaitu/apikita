pub mod abuse;
pub mod config;
pub mod db;
pub mod error;
pub mod ip_tracking;
pub mod money;
pub mod routes;
/// Test-only, because it compensates for a decision that belongs to the tests and
/// not to the binary: a test database is a temp file, so the money tests no longer
/// need a live server and no longer have to be `#[ignore]`d. Compiled out of every
/// non-test build.
#[cfg(test)]
mod test_support;
pub mod upstream;
