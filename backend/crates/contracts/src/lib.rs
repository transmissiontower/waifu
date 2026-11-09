//! gRPC contracts for every Waifu service. The `.proto` files in `proto/` are the
//! single source of truth; any language can implement them (see docs/CUSTOM_SERVICE.md).
#![allow(clippy::all)]
pub mod v1 {
    tonic::include_proto!("waifu.v1");
}
pub use v1::*;

/// gRPC metadata key used to propagate the trace id between services.
pub const TRACE_HEADER: &str = "x-trace-id";
