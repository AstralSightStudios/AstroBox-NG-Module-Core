use pb::xiaomi::protocol::WearPacket;
use prost::Message;
use std::{
    any::TypeId,
    collections::HashMap,
    io::Cursor,
    sync::{OnceLock, RwLock},
};

use crate::device::xiaomi::packet::v2::layer2::{L2Channel, L2OpCode};
use crate::ecs::system::System;

// 收L2包的System扩展trait
pub trait XiaomiSystemExt: System {
    fn on_layer2_packet(&mut self, channel: L2Channel, opcode: L2OpCode, payload: &[u8]);
}

// 收PB包的System扩展trait，基于L2
pub trait L2PbExt: System {
    fn on_pb_packet(&mut self, payload: WearPacket);
}

// 默认L2转发on_pb_packet逻辑
impl<T> XiaomiSystemExt for T
where
    T: L2PbExt,
{
    fn on_layer2_packet(&mut self, channel: L2Channel, _opcode: L2OpCode, payload: &[u8]) {
        if channel == L2Channel::Pb {
            if let Ok(wp) = pb::xiaomi::protocol::WearPacket::decode(Cursor::new(&payload)) {
                self.on_pb_packet(wp);
            }
        }
    }
}

type OnL2PacketDispatcher = fn(sys: &mut dyn System, ch: L2Channel, op: L2OpCode, payload: &[u8]);

// 记录所有注册了该Ext的System
// 唐比Rust不能动态类型。
// TODO: 使用一些神秘第三方库并加上std的开盒功能也许可以替代这种傻逼写法，
static ON_L2_PACKET_DISPATCHERS: OnceLock<RwLock<HashMap<TypeId, OnL2PacketDispatcher>>> =
    OnceLock::new();

#[inline]
fn xiaomi_ext_on_l2packet_registry() -> &'static RwLock<HashMap<TypeId, OnL2PacketDispatcher>> {
    ON_L2_PACKET_DISPATCHERS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn make_xiaomi_ext_on_l2packet_dispatcher<T>() -> OnL2PacketDispatcher
where
    T: XiaomiSystemExt + 'static,
{
    fn inner<T: XiaomiSystemExt + 'static>(
        sys: &mut dyn System,
        ch: L2Channel,
        op: L2OpCode,
        payload: &[u8],
    ) {
        let any = sys.as_any_mut();
        let t = any
            .downcast_mut::<T>()
            .expect("TypeId matched but downcast failed");
        t.on_layer2_packet(ch, op, payload);
    }
    inner::<T>
}

pub fn register_xiaomi_system_ext_on_l2packet<T>()
where
    T: XiaomiSystemExt + 'static,
{
    let mut map = xiaomi_ext_on_l2packet_registry()
        .write()
        .expect("poisoned XiaomiSystemExt registry");
    map.insert(
        TypeId::of::<T>(),
        make_xiaomi_ext_on_l2packet_dispatcher::<T>(),
    );
}

pub fn try_invoke_xiaomi_system_ext_on_l2packet(
    sys: &mut dyn System,
    ch: L2Channel,
    op: L2OpCode,
    payload: &[u8],
) -> bool {
    let tid = sys.as_any().type_id();
    if let Some(d) = xiaomi_ext_on_l2packet_registry()
        .read()
        .expect("poisoned XiaomiSystemExt registry")
        .get(&tid)
    {
        (d)(sys, ch, op, payload);
        true
    } else {
        false
    }
}

pub fn is_xiaomi_system_ext(sys: &dyn System) -> bool {
    let tid = sys.as_any().type_id();
    xiaomi_ext_on_l2packet_registry()
        .read()
        .expect("poisoned XiaomiSystemExt registry")
        .contains_key(&tid)
}
