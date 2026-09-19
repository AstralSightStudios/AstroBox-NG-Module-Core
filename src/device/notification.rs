use anyhow::{Result, anyhow, ensure};

pub use super::xiaomi::components::notification::{LiveActivity, NotificationContent};
use super::xiaomi::{
    XiaomiDevice,
    components::{auth::AuthComponent, notification::NotificationSystem},
};

pub async fn send(addr: String, source: String, content: NotificationContent) -> Result<()> {
    with_system(addr, move |system, device| {
        system.send(device, &source, content)
    })
    .await
}

pub async fn remove(addr: String, source: String, id: u32) -> Result<()> {
    with_system(addr, move |system, device| {
        system.remove(device, &source, id)
    })
    .await
}

async fn with_system(
    addr: String,
    f: impl FnOnce(&NotificationSystem, &mut XiaomiDevice) -> Result<()> + Send + 'static,
) -> Result<()> {
    crate::ecs::with_rt_mut(move |rt| {
        let entity = rt
            .device_entity(&addr)
            .ok_or_else(|| anyhow!("device not connected"))?;
        let world = rt.world_mut();
        ensure!(
            world.get::<XiaomiDevice>(entity).is_some(),
            "notifications are not supported for this device protocol"
        );
        ensure!(
            world
                .get::<AuthComponent>(entity)
                .is_some_and(|auth| auth.is_authed),
            "device is not authenticated"
        );
        let mut query = world.query::<(&NotificationSystem, &mut XiaomiDevice)>();
        let (system, mut device) = query
            .get_mut(world, entity)
            .map_err(|_| anyhow!("notification system not available"))?;
        f(system, &mut device)
    })
    .await
}
