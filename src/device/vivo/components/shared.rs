use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::{runtime::Handle, sync::oneshot};

use crate::{
    anyhow_site,
    device::vivo::{VivoDevice, transport::vscp::VscpMessage},
    ecs::access::with_device_component_mut,
};

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
                log::debug!("vivo request slot receiver dropped before fulfillment");
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
                log::debug!("vivo request slot receiver dropped before failure");
            }
        }
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

pub trait HasVivoRequestContext {
    fn owner_id(&self) -> &str;
    fn tk_handle(&self) -> &Handle;
}

pub trait VivoRequestExt: HasVivoRequestContext {
    fn send_vivo_message(&self, message: VscpMessage, log_ctx: &'static str) -> anyhow::Result<()> {
        let owner_id = self.owner_id().to_string();
        let send_parts = with_device_component_mut::<VivoDevice, _, _>(owner_id, move |dev| {
            dev.transport_send_parts(message)
        })
        .map_err(|err| anyhow_site!("{log_ctx}: failed to prepare vivo send: {err:?}"))?;

        let (sender, packets) = send_parts
            .map_err(|err| anyhow_site!("{log_ctx}: failed to encode vivo send: {err:?}"))?;

        self.tk_handle()
            .block_on(async move { (sender)(packets).await })
            .map_err(|err| anyhow_site!("{log_ctx}: failed to send vivo message: {err:?}"))
    }
}

impl<T> VivoRequestExt for T where T: HasVivoRequestContext {}
