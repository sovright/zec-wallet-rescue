// Shared test helpers for the C2 integration tests under
// `crates/zeck-core/tests/`. See `regtest_harness.rs` for the regtest
// fixture; this module exists so cargo's test runner doesn't compile
// `common/` as its own integration target (it would, if `common/`
// contained a top-level `tests/common.rs`).

pub mod regtest_harness;

// `FakeLightwalletd` is the in-process gRPC test fixture used by the bad-
// network C2 tests (R-N8, R-N9). It depends on tonic server codegen run by
// `build.rs`, which only fires under the `argos-network` feature, so it is
// also gated on that feature here.
#[cfg(feature = "argos-network")]
pub mod fake_lightwalletd;
