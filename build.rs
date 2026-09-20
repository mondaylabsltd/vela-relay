//! The docker shell's build script. The identity logic is shared with
//! `vela-relay-cf/build.rs` so both deployments answer `/version` the same way.
include!("build_info.rs");

fn main() {
    emit_build_info(".");
}
