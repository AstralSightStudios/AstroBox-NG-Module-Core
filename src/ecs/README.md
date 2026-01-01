# AstroBox Core ECS 说明文档

## 现状与设计
Core 已彻底切换为 `bevy_ecs`。当前 ECS 运行时由 `Runtime` 包裹 `bevy_ecs::World`，
并维护一张 `device_id -> Entity` 的索引表，用于快速定位设备实体。所有对 ECS 的访问
通过 `ecs::with_rt_mut` 串行切入运行时线程，避免跨线程锁竞争和死锁问题。

关键设计要点：
1. 设备实体以地址为主键索引，外部只需要 `device_id` 即可访问组件。
2. 组件/系统统一为 `bevy_ecs::Component`，不再区分 LogicComponent / System。
3. 所有 ECS 访问都包裹在闭包里，避免在运行时线程里 `await`。

## 文件结构
`runtime.rs` - Runtime（World + 设备索引）实现  
`access.rs` - 常用访问封装（如 `with_device_component_mut`）  
`graph.rs` - ECS 状态图输出（用于调试）

## 使用示例
```rust
// 初始化运行时
corelib::ecs::init_runtime_default();

// 访问指定设备的组件
let device_id = "AA:BB:CC:DD";
corelib::ecs::with_rt_mut(move |rt| {
    rt.with_device_mut(device_id, |world, entity| {
        let mut dev = world
            .get_mut::<corelib::device::xiaomi::XiaomiDevice>(entity)
            .expect("device missing");
        dev.sar.lock().check_timeouts_internal();
    });
}).await;
```
