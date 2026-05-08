use tokio::runtime::Handle;

use crate::device::vivo::VivoDevice;

pub fn on_packet(tk_handle: Handle, device_id: String, data: Vec<u8>) {
    crate::asyncrt::spawn_with_handle(
        async move {
            let messages = crate::ecs::with_rt_mut({
                let device_id = device_id.clone();
                move |rt| {
                    rt.with_device_mut(&device_id, |world, entity| {
                        let Some(dev) = world.get_mut::<VivoDevice>(entity) else {
                            return Ok(Vec::new());
                        };
                        dev.on_transport_data(&data)
                    })
                    .unwrap_or_else(|| Ok(Vec::new()))
                }
            })
            .await;

            let messages = match messages {
                Ok(messages) => messages,
                Err(err) => {
                    log::warn!("[VivoDevice] failed to decode transport data: {err}");
                    return;
                }
            };

            for message in messages {
                crate::ecs::with_rt_mut({
                    let device_id = device_id.clone();
                    move |rt| {
                        let _ = rt.with_device_mut(&device_id, |world, entity| {
                            crate::device::vivo::system::dispatch_vivo_system_ext_on_message(
                                world, entity, &message,
                            );
                        });
                    }
                })
                .await;
            }
        },
        tk_handle,
    );
}
