pub mod asyncrt;
pub mod constants;
pub mod crypto;
pub mod device;
pub mod ecs;
#[macro_use]
pub mod error;
pub mod logger;
pub mod tools;

// 默认初始化函数，使用默认配置初始化ECS系统
pub fn init() {
    ecs::init_runtime_default();
}
