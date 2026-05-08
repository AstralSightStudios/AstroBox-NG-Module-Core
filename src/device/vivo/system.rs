use std::{
    any::TypeId,
    collections::HashMap,
    sync::{OnceLock, RwLock},
};

use bevy_ecs::{component::Component, entity::Entity, world::World};

use crate::device::vivo::transport::vscp::VscpMessage;

pub trait VivoSystemExt: Component {
    fn on_vivo_message(&mut self, message: &VscpMessage);
}

type OnVivoMessageDispatcher = fn(world: &mut World, entity: Entity, message: &VscpMessage);

static ON_VIVO_MESSAGE_DISPATCHERS: OnceLock<RwLock<HashMap<TypeId, OnVivoMessageDispatcher>>> =
    OnceLock::new();

#[inline]
fn vivo_ext_on_message_registry() -> &'static RwLock<HashMap<TypeId, OnVivoMessageDispatcher>> {
    ON_VIVO_MESSAGE_DISPATCHERS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn make_vivo_ext_on_message_dispatcher<T>() -> OnVivoMessageDispatcher
where
    T: VivoSystemExt + Component + 'static,
{
    fn inner<T: VivoSystemExt + Component + 'static>(
        world: &mut World,
        entity: Entity,
        message: &VscpMessage,
    ) {
        if let Some(mut t) = world.get_mut::<T>(entity) {
            t.on_vivo_message(message);
        }
    }
    inner::<T>
}

pub fn register_vivo_system_ext_on_message<T>()
where
    T: VivoSystemExt + Component + 'static,
{
    let mut map = vivo_ext_on_message_registry()
        .write()
        .expect("poisoned VivoSystemExt registry");
    map.insert(
        TypeId::of::<T>(),
        make_vivo_ext_on_message_dispatcher::<T>(),
    );
}

pub fn dispatch_vivo_system_ext_on_message(
    world: &mut World,
    entity: Entity,
    message: &VscpMessage,
) -> bool {
    let map = vivo_ext_on_message_registry()
        .read()
        .expect("poisoned VivoSystemExt registry");
    if map.is_empty() {
        return false;
    }

    for dispatch in map.values() {
        dispatch(world, entity, message);
    }
    true
}
