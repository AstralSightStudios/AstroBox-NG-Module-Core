use bevy_ecs::{
    bundle::Bundle,
    component::Component,
    entity::Entity,
    world::{EntityWorldMut, World},
};
use std::collections::HashMap;

#[derive(Default)]
struct DeviceIndex {
    map: HashMap<String, Entity>,
}

// ECS运行时环境，相当于World
// 设备通过 bevy_ecs::World 托管，使用设备地址索引快速定位 Entity
pub struct Runtime {
    world: World,
    devices: DeviceIndex,
}

impl Runtime {
    pub fn new() -> Runtime {
        Runtime {
            world: World::new(),
            devices: DeviceIndex::default(),
        }
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    pub fn device_count(&self) -> usize {
        self.devices.map.len()
    }

    pub fn device_ids(&self) -> impl Iterator<Item = &String> {
        self.devices.map.keys()
    }

    pub fn spawn_device<B: Bundle>(&mut self, id: String, bundle: B) -> Entity {
        let entity = self.world.spawn(bundle).id();
        self.devices.map.insert(id, entity);
        entity
    }

    pub fn remove_device(&mut self, id: &str) -> Option<Entity> {
        let entity = self.devices.map.remove(id)?;
        let _ = self.world.despawn(entity);
        Some(entity)
    }

    pub fn device_entity(&self, id: &str) -> Option<Entity> {
        self.devices.map.get(id).copied()
    }

    pub fn device_entity_mut(&mut self, id: &str) -> Option<EntityWorldMut<'_>> {
        let entity = self.device_entity(id)?;
        Some(self.world.entity_mut(entity))
    }

    pub fn component_mut<T: Component>(
        &mut self,
        id: &str,
    ) -> Option<bevy_ecs::world::Mut<'_, T>> {
        let entity = self.device_entity(id)?;
        self.world.get_mut::<T>(entity)
    }

    pub fn component_ref<T: Component>(&self, id: &str) -> Option<&T> {
        let entity = self.device_entity(id)?;
        self.world.get::<T>(entity)
    }

    pub fn with_device_mut<R>(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut World, Entity) -> R,
    ) -> Option<R> {
        let entity = self.device_entity(id)?;
        Some(f(&mut self.world, entity))
    }

    pub fn with_world_mut<R>(&mut self, f: impl FnOnce(&mut World) -> R) -> R {
        f(&mut self.world)
    }
}
