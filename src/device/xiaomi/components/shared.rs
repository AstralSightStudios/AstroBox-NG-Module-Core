use anyhow::{Context, Result};
use pb::xiaomi::protocol;
use tokio::sync::oneshot;

use crate::{
    anyhow_site,
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

        // A cancelled caller drops its receiver, but the sender stays in this
        // slot until it is explicitly fulfilled or failed.  Do not let such a
        // closed sender make the next request look like a request already in
        // flight.
        waiters.retain(|waiter| !waiter.is_closed());
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
    /// Enqueue a packet without waiting for a device response.
    fn enqueue_pb_request(&mut self, packet: protocol::WearPacket, log_ctx: &'static str);

    /// Enqueue a packet and return an error when the device/component is no
    /// longer available.  Request/response paths should use this fallible
    /// variant and fail their waiter immediately.
    fn try_enqueue_pb_request(
        &mut self,
        packet: protocol::WearPacket,
        log_ctx: &'static str,
    ) -> anyhow::Result<()> {
        self.enqueue_pb_request(packet, log_ctx);
        Ok(())
    }
}

impl<T> SystemRequestExt for T
where
    T: HasOwnerId,
{
    fn enqueue_pb_request(&mut self, packet: protocol::WearPacket, log_ctx: &'static str) {
        if let Err(err) = self.try_enqueue_pb_request(packet, log_ctx) {
            log::warn!("[{log_ctx}] failed to enqueue Xiaomi packet: {err:#}");
        }
    }

    fn try_enqueue_pb_request(
        &mut self,
        packet: protocol::WearPacket,
        log_ctx: &'static str,
    ) -> anyhow::Result<()> {
        let owner_id = self.owner_id().to_string();
        with_device_component_mut::<XiaomiDevice, _, _>(owner_id, move |dev| {
            packet::cipher::enqueue_pb_packet(dev, packet, log_ctx);
        })
        .map(|_| ())
        .map_err(|err| anyhow_site!("{log_ctx}: failed to access Xiaomi device: {err:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::RequestSlot;

    #[test]
    fn dropped_receiver_does_not_block_next_request() {
        let mut slot = RequestSlot::<u32>::new();
        let (first, should_enqueue) = slot.prepare();
        assert!(should_enqueue);
        drop(first);

        let (mut second, should_enqueue) = slot.prepare();
        assert!(should_enqueue);
        slot.fulfill(42);
        assert_eq!(second.try_recv().unwrap().unwrap(), 42);
    }
}
