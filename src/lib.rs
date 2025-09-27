pub mod asyncrt;
pub mod constants;
pub mod crypto;
pub mod device;
pub mod ecs;
pub mod logger;
pub mod tools;

pub fn init() {
    ecs::init_runtime_default();
}
