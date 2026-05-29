//! Build script for `argos-core`.
//!
//! Compiles the vendored lightwalletd protos in `tests/proto/` when (and only
//! when) the `argos-network` feature is enabled. The generated code is used
//! by the `FakeLightwalletd` test fixture (`tests/common/fake_lightwalletd.rs`)
//! to stand up an in-process gRPC server that speaks the real lightwalletd
//! wire protocol.
//!
//! Production builds compile this script to a no-op: protos are not compiled
//! and nothing the fixture generates ships in a released Argos binary.

fn main() {
    // Cargo sets `CARGO_FEATURE_<NAME>` for every enabled feature; gating on
    // it keeps default builds free of the codegen step. (We can't conditionally
    // *omit* the `tonic-build` build-dependency itself — Cargo doesn't support
    // feature-gated `[build-dependencies]` — but the dep is a no-op when not
    // invoked, and it's already in the workspace lock graph as a transitive
    // build-dep of the `zcash_*` crates.)
    if std::env::var_os("CARGO_FEATURE_ARGOS_NETWORK").is_none() {
        return;
    }

    let proto_dir = "tests/proto";
    let protos = [
        "tests/proto/service.proto",
        "tests/proto/compact_formats.proto",
    ];

    for proto in &protos {
        println!("cargo:rerun-if-changed={proto}");
    }
    println!("cargo:rerun-if-changed={proto_dir}");

    // `build_client = false` — Argos's runtime uses the client stubs already
    // generated inside `zcash_client_backend::proto`; we only need the server
    // side here. `build_server = true` gives us the `CompactTxStreamer` trait
    // the FakeLightwalletd fixture implements.
    tonic_prost_build::configure()
        .build_client(false)
        .build_server(true)
        .compile_protos(&protos, &[proto_dir])
        .expect("compile vendored lightwalletd protos for argos-network fixture");
}
