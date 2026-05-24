#[cfg(all(not(target_arch = "wasm32"), feature = "xiaomi-network-stack"))]
mod native;
#[cfg(all(not(target_arch = "wasm32"), feature = "xiaomi-network-stack"))]
pub use native::*;
