use bevy_ecs::{component::Component, entity::Entity, world::World};

use crate::ecs::runtime::Runtime;

#[derive(Debug)]
pub enum EcsAccessError {
    DeviceNotFound { id: String },
    ComponentMissing { id: String, component: &'static str },
}

pub fn with_device_world<R, F>(owner_id: String, f: F) -> Result<R, EcsAccessError>
where
    F: FnOnce(&mut World, Entity) -> Result<R, EcsAccessError> + Send + 'static,
    R: Send + 'static,
{
    crate::asyncrt::universal_block_on(|| async move {
        crate::ecs::with_rt_mut(move |rt: &mut Runtime| {
            rt.with_device_mut(&owner_id, |world, entity| f(world, entity))
                .ok_or_else(|| EcsAccessError::DeviceNotFound {
                    id: owner_id.clone(),
                })?
        })
        .await
    })
}

pub fn with_device_component_mut<T, R, F>(owner_id: String, f: F) -> Result<R, EcsAccessError>
where
    T: Component + 'static,
    F: FnOnce(&mut T) -> R + Send + 'static,
    R: Send + 'static,
{
    with_device_world(owner_id.clone(), move |world, entity| {
        let mut comp =
            world
                .get_mut::<T>(entity)
                .ok_or_else(|| EcsAccessError::ComponentMissing {
                    id: owner_id.clone(),
                    component: std::any::type_name::<T>(),
                })?;
        Ok(f(&mut comp))
    })
}
