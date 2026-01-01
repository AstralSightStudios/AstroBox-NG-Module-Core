use anyhow::{Context, Result};
use pb::xiaomi::protocol;
use tokio::sync::oneshot;

use crate::{
    device::xiaomi::{XiaomiDevice, packet},
    ecs::access::with_device_component_mut,
};
use parking_lot::Mutex;

pub struct RequestSlot<T> {
    waiter: Mutex<Option<oneshot::Sender<Result<T>>>>,
}

impl<T> RequestSlot<T> {
    pub fn new() -> Self {
        Self {
            waiter: Mutex::new(None),
        }
    }

    pub fn prepare(&mut self) -> oneshot::Receiver<Result<T>> {
        let (tx, rx) = oneshot::channel();
        *self.waiter.lock() = Some(tx);
        rx
    }

    pub fn fulfill(&mut self, value: T) {
        self.fulfill_result(Ok(value));
    }

    pub fn fail(&mut self, err: anyhow::Error) {
        self.fulfill_result(Err(err));
    }

    fn fulfill_result(&mut self, result: Result<T>) {
        if let Some(tx) = self.waiter.lock().take() {
            if tx.send(result).is_err() {
                log::debug!("request slot receiver dropped before fulfillment");
            }
        }
    }
}

pub async fn await_response<T>(
    rx: oneshot::Receiver<Result<T>>,
    err_ctx: &'static str,
) -> anyhow::Result<T>
where
    T: Send + 'static,
{
    let resp = rx.await.context(err_ctx)?;
    resp
}

pub trait HasOwnerId {
    fn owner_id(&self) -> &str;
}

pub trait SystemRequestExt: HasOwnerId {
    fn enqueue_pb_request(&mut self, packet: protocol::WearPacket, log_ctx: &'static str);
}

impl<T> SystemRequestExt for T
where
    T: HasOwnerId,
{
    fn enqueue_pb_request(&mut self, packet: protocol::WearPacket, log_ctx: &'static str) {
        let owner_id = self.owner_id().to_string();
        let _ = with_device_component_mut::<XiaomiDevice, _, _>(owner_id, move |dev| {
            packet::cipher::enqueue_pb_packet(dev, packet, log_ctx);
        });
    }
}
