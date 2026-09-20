//! The Cloudflare shell's build script.
//!
//! Without this the Worker had no build identity at all: `/version` read an
//! `option_env!` nothing ever set and answered `"build": "dev"` on every
//! deployment, live ones included.
include!("../build_info.rs");

fn main() {
    emit_build_info("..");
}
