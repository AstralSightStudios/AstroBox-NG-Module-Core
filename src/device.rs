use crate::device::xiaomi::SendError;
use crate::device::xiaomi::XiaomiDevice;
use crate::device::xiaomi::components::auth::{AuthComponent, AuthSystem};
use crate::device::xiaomi::config::XiaomiDeviceConfig;
use crate::device::xiaomi::r#type::ConnectType;
use crate::ecs::component::Component;
use crate::ecs::entity::EntityExt;
use std::future::Future;
use tokio::runtime::Handle;

pub mod xiaomi;

pub async fn create_miwear_device<F, Fut>(
    tk_handle: Handle,
    name: String,
    addr: String,
    authkey: String,
    sar_version: u32,
    connect_type: ConnectType,
    force_android: bool,
    sender: F,
) where
    F: Fn(Vec<u8>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), SendError>> + Send + 'static,
{
    let name_for_auth = name.clone();
    let tk_handle_clone = tk_handle.clone();

    crate::ecs::with_rt_mut(move |rt| {
        let device_config = XiaomiDeviceConfig::default();
        let dev = XiaomiDevice::new(
            tk_handle_clone.clone(),
            name.clone(),
            addr,
            authkey,
            sar_version,
            connect_type,
            force_android,
            device_config,
            sender,
        );
        rt.add_entity(dev);
    })
    .await;

    let _ = crate::asyncrt::spawn_with_handle(
        async move {
            crate::ecs::with_rt_mut(move |rt| {
                if let Some(dev) = rt.find_entity_by_id_mut::<XiaomiDevice>(&name_for_auth) {
                    dev.get_component_as_mut::<AuthComponent>(AuthComponent::ID)
                        .unwrap()
                        .as_logic_component_mut()
                        .unwrap()
                        .system_mut()
                        .as_any_mut()
                        .downcast_mut::<AuthSystem>()
                        .unwrap()
                        .start_auth();
                }
            })
            .await;
        },
        tk_handle,
    );
}
