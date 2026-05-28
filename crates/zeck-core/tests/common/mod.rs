// Shared test helpers for the C2 integration tests under
// `crates/zeck-core/tests/`. See `regtest_harness.rs` for the regtest
// fixture; this module exists so cargo's test runner doesn't compile
// `common/` as its own integration target (it would, if `common/`
// contained a top-level `tests/common.rs`).

pub mod regtest_harness;
