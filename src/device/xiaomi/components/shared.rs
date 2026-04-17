use anyhow::{Context, Result};
use pb::xiaomi::protocol;
use tokio::sync::oneshot;

use crate::{
    device::xiaomi::{XiaomiDevice, packet},
    ecs::access::with_device_component_mut,
};
use parking_lot::Mutex;

pub struct RequestSlot<T> {
    waiters: Mutex<Vec<oneshot::Sender<Result<T>>>>,
}

impl<T> RequestSlot<T> {
    pub fn new() -> Self {
        Self {
            waiters: Mutex::new(Vec::new()),
        }
    }

    pub fn prepare(&mut self) -> (oneshot::Receiver<Result<T>>, bool) {
        let (tx, rx) = oneshot::channel();
        let mut waiters = self.waiters.lock();
        let should_enqueue = waiters.is_empty();
        waiters.push(tx);
        (rx, should_enqueue)
    }

    pub fn fulfill(&mut self, value: T)
    where
        T: Clone,
    {
        let mut waiters = self.take_waiters();
        for tx in waiters.drain(..) {
            if tx.send(Ok(value.clone())).is_err() {
                log::debug!("request slot receiver dropped before fulfillment");
            }
        }
    }

    pub fn fail(&mut self, err: anyhow::Error) {
        let mut waiters = self.take_waiters();
        if waiters.is_empty() {
            return;
        }

        let err_text = format!("{err:#}");
        for tx in waiters.drain(..) {
            if tx.send(Err(anyhow::Error::msg(err_text.clone()))).is_err() {
                log::debug!("request slot receiver dropped before failure");
            }
        }
    }

    pub fn clear(&mut self) {
        self.take_waiters();
    }

    fn take_waiters(&mut self) -> Vec<oneshot::Sender<Result<T>>> {
        std::mem::take(&mut *self.waiters.lock())
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
