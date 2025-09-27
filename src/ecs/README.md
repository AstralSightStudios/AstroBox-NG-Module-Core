# AstroBox Core ECS说明文档

## 设计理念
这是一套类似游戏引擎的ECS实现方案，主要由`Runtime - Entity - Component + LogicComponent + System`组成。该系统具有这些核心设计理念：
1. 通过传入闭包的方法进行调用，最大程度上避免死锁，淡化锁的存在
2. 不允许在ECS线程内await，以防止任何阻塞现象
3. 使用方便，可快速进行横向扩展
4. All in mutable，拒绝readonly
5. 不使用任何自定义生命周期，防止陷入生命周期地狱

## 文件结构
`runtime.rs` - Runtime (World) 结构实现

`entity.rs` - Entity结构实现

`component.rs` - Component结构实现

`logic_component.rs` - LogicComponent结构实现

`system.rs` - System结构实现

`fastlane.rs` - 为LogicComponent和System编写的扩展，允许其快速访问管理自己的Entity

## 使用例
### 实现一个Entity
```rust
struct Fucker {
    id: String,
    comps: Vec<Box<dyn Component>>,
    systs: Vec<Box<dyn System>>,
}

impl Fucker {
    fn new(id: &str) -> Self {
        Self { id: id.into(), comps: vec![], systs: vec![] }
    }
}

// 由于Rust的语法特性，在这里你需要写一堆狗屎，我也没办法
impl Entity for Fucker {
    fn id(&self) -> &str { &self.id }
    fn components(&mut self) -> &mut Vec<Box<dyn Component>> { &mut self.comps }
    fn systems(&mut self) -> &mut Vec<Box<dyn System>> { &mut self.systs }
    fn as_any(&self) -> &dyn Any { self }
    fn as_any_mut(&mut self) -> &mut dyn Any { self }
}
```
### 实现一个Component
```rust
#[derive(Debug)]
struct Name {
    value: String,
    // 你可以在这里扩充你自己的神笔数据
    家庭住址: String,
    身份证号: String,
    owner: String,
}

impl Name {
    fn new(value: &str) -> Self {
        Self { value: value.into(), 
        身份证号: "421302xxxxxxxx0033".to_string, 家庭住址: "湖北省随州市曾都区", owner: None }
    }
}

impl Component for Name {
    fn id(&self) -> &str { "name" }
    fn as_any(&self) -> &dyn Any { self }
    fn as_any_mut(&mut self) -> &mut dyn Any { self }
    fn into_any(self: Box<Self>) -> Box<dyn Any> { self }
    fn set_owner(&mut self, entity_id: &str) { self.owner = entity_id.to_string(); }
    fn owner(&self) -> Option<&str> { Some(self.owner.as_str()) }
}
```
### 实现一个System
```rust
#[derive(Debug)]
struct ExampleSystem {
    value: i32,
    owner: String,
}

impl ExampleSystem {
    fn new(dmg: i32) -> Self {
        Self { dmg, owner: None }
    }
}

impl System for ExampleSystem {
    fn id(&self) -> &str { "example" }
    fn as_any(&self) -> &dyn Any { self }
    fn as_any_mut(&mut self) -> &mut dyn Any { self }
    fn into_any(self: Box<Self>) -> Box<dyn Any> { self }
    fn set_owner(&mut self, entity_id: &str) { self.owner = entity_id.to_string(); }
    fn owner(&self) -> Option<&str> { Some(self.owner.as_str()) }
}
```
### 为Entity添加Component
```rust
fucker.add_component(Box::new(Name::new("name", "彩虹哥")));
fucker.add_component(Box::new(Age::new("age", 100)));
```
### 为Entity添加System
```rust
fucker.add_system(Box::new(ExampleSystem::new("test", 30)));
```
### 使用FastLane从System修改父Entity的Component
```rust
let result = <dyn System>::with_component_mut::<Age, _, _>(&age_sys, "age", |h| {
    h.value -= 25;
}).await?;
```