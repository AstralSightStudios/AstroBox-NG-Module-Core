use anyhow::{Context, Result};
use pb::xiaomi::protocol;
use tokio::sync::oneshot;

use crate::{
    device::xiaomi::{XiaomiDevice, packet},
    ecs::{fastlane::FastLane, system::System},
};

pub struct RequestSlot<T> {
    waiter: Option<oneshot::Sender<Result<T>>>,
}

impl<T> RequestSlot<T> {
    pub fn new() -> Self {
        Self { waiter: None }
    }

    pub fn prepare(&mut self) -> oneshot::Receiver<Result<T>> {
        let (tx, rx) = oneshot::channel();
        self.waiter = Some(tx);
        rx
    }

    pub fn fulfill(&mut self, value: T) {
        self.fulfill_result(Ok(value));
    }

    pub fn fail(&mut self, err: anyhow::Error) {
        self.fulfill_result(Err(err));
    }

    fn fulfill_result(&mut self, result: Result<T>) {
        if let Some(tx) = self.waiter.take() {
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

pub trait SystemRequestExt {
    fn enqueue_pb_request(&mut self, packet: protocol::WearPacket, log_ctx: &'static str);
}

impl<T> SystemRequestExt for T
where
    T: System + 'static,
{
    fn enqueue_pb_request(&mut self, packet: protocol::WearPacket, log_ctx: &'static str) {
        let sys: &mut dyn System = self;

        FastLane::with_entity_mut::<(), _>(sys, move |ent| {
            let dev = ent.as_any_mut().downcast_mut::<XiaomiDevice>().unwrap();
            packet::enqueue_pb_packet(dev, packet, log_ctx);
            Ok(())
        })
        .unwrap();
    }
}
