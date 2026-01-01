use std::{
    collections::HashMap,
    sync::{OnceLock, RwLock},
};

use tokio::runtime::Handle;

use crate::device::xiaomi::XiaomiDevice;

use super::{
    cipher::{SharedL2Cipher, ensure_l2_cipher},
    v2::{
        layer1::L1Packet,
        layer2::{L2Cipher, L2Packet},
    },
};

static RECV_BUFFERS: OnceLock<RwLock<HashMap<String, Vec<u8>>>> = OnceLock::new();

fn recv_buffer_registry() -> &'static RwLock<HashMap<String, Vec<u8>>> {
    RECV_BUFFERS.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn on_packet(tk_handle: Handle, device_id: String, data: Vec<u8>) {
    crate::asyncrt::spawn_with_handle(
        async move {
            let mut frames: Vec<Vec<u8>> = Vec::new();
            {
                let mut registry = recv_buffer_registry()
                    .write()
                    .expect("poisoned MiWear recv buffer registry");
                let buffer = registry.entry(device_id.clone()).or_insert_with(Vec::new);
                buffer.extend_from_slice(&data);

                let mut idx = 0usize;
                while idx + 8 <= buffer.len() {
                    if !(buffer[idx] == 0xa5 && buffer[idx + 1] == 0xa5) {
                        idx = idx.saturating_add(1);
                        continue;
                    }

                    let declared_len =
                        u16::from_le_bytes([buffer[idx + 4], buffer[idx + 5]]) as usize;
                    let total = 8 + declared_len;
                    if idx + total > buffer.len() {
                        break;
                    }

                    frames.push(buffer[idx..idx + total].to_vec());
                    idx += total;
                }

                if idx > 0 {
                    buffer.drain(0..idx);
                }

                let should_remove = buffer.is_empty();
                if should_remove {
                    let _ = buffer;
                    registry.remove(&device_id);
                }
            }

            if frames.is_empty() {
                return;
            }

            let sar_version = crate::ecs::with_rt_mut({
                let device_id_clone = device_id.clone();
                move |rt| rt.component_ref::<XiaomiDevice>(&device_id_clone).map(|dev| dev.sar_version)
            })
            .await;
            let shared_cipher: Option<SharedL2Cipher> = match sar_version {
                Some(version) => ensure_l2_cipher(&device_id, version).await,
                None => None,
            };

            for frame in frames {
                let l1 = match L1Packet::from_bytes(&frame) {
                    Ok(p) => p,
                    Err(err) => {
                        log::warn!("Decode L1 Packet Err: {}", err.to_string());
                        continue;
                    }
                };

                let deliver_up = crate::ecs::with_rt_mut({
                    let device_id_lookup = device_id.clone();
                    let l1_clone = l1.clone();
                    move |rt| {
                        rt.with_device_mut(&device_id_lookup, |world, entity| {
                            if let Some(dev) = world.get_mut::<XiaomiDevice>(entity) {
                                if dev.sar_version == 2 {
                                    return dev.sar.lock().on_l1_packet(&l1_clone);
                                }
                            }
                            false
                        })
                        .unwrap_or(false)
                    }
                })
                .await;

                if deliver_up {
                    let cipher_ref = shared_cipher.as_ref().map(|c| c.as_ref() as &dyn L2Cipher);
                    if let Ok(l2p) = L2Packet::from_l1(&l1, cipher_ref) {
                        let ch = l2p.channel;
                        let op = l2p.opcode;
                        let payload = l2p.payload;

                        crate::ecs::with_rt_mut({
                            let device_id_dispatch = device_id.clone();
                            move |rt| {
                                let _ = rt.with_device_mut(&device_id_dispatch, |world, entity| {
                                    if let Some(dev) = world.get::<XiaomiDevice>(entity) {
                                        if dev.sar_version == 2 {
                                            let _ = crate::device::xiaomi::system::dispatch_xiaomi_system_ext_on_l2packet(
                                                world,
                                                entity,
                                                ch,
                                                op,
                                                &payload,
                                            );
                                        }
                                    }
                                });
                            }
                        })
                        .await;
                    }
                }
            }
        },
        tk_handle,
    );
}

pub fn clear_recv_buffer(device_id: &str) {
    match recv_buffer_registry().write() {
        Ok(mut registry) => {
            registry.remove(device_id);
        }
        Err(poisoned) => {
            let mut registry = poisoned.into_inner();
            registry.remove(device_id);
        }
    }
}
