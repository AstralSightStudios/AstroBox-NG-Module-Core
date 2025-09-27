pub use std::time::Duration;

use std::future::Future;

#[cfg(target_arch = "wasm32")]
use futures::{
    executor,
    future::{AbortHandle, Abortable},
};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local;

#[cfg(target_arch = "wasm32")]
pub fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

pub fn universal_block_on<F, Fut, R>(f: F) -> R
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = R>,
{
    #[cfg(target_arch = "wasm32")]
    {
        executor::block_on(f())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::task::block_in_place(|| tokio::runtime::Runtime::new().unwrap().block_on(f()))
    }
}

#[cfg(target_arch = "wasm32")]
pub struct TaskHandle(AbortHandle);

#[cfg(target_arch = "wasm32")]
impl TaskHandle {
    pub fn abort(&self) {
        self.0.abort();
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub type TaskHandle = tokio::task::JoinHandle<()>;

#[cfg(target_arch = "wasm32")]
pub fn spawn<F>(fut: F) -> TaskHandle
where
    F: Future<Output = ()> + 'static,
{
    let (handle, reg) = AbortHandle::new_pair();
    spawn_local(async move {
        let fut = Abortable::new(fut, reg);
        let _ = fut.await;
    });
    TaskHandle(handle)
}

#[cfg(target_arch = "wasm32")]
pub fn spawn_with_handle<F>(fut: F, _handle: tokio::runtime::Handle) -> TaskHandle
where
    F: Future<Output = ()> + 'static,
{
    spawn(fut)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn<F>(fut: F) -> TaskHandle
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(fut)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_with_handle<F>(fut: F, handle: tokio::runtime::Handle) -> TaskHandle
where
    F: Future<Output = ()> + Send + 'static,
{
    handle.spawn(fut)
}

#[cfg(target_arch = "wasm32")]
pub async fn sleep(duration: Duration) {
    gloo_timers::future::sleep(duration).await;
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub struct TimeoutElapsed;

#[cfg(target_arch = "wasm32")]
impl std::fmt::Display for TimeoutElapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deadline has elapsed")
    }
}

#[cfg(target_arch = "wasm32")]
impl std::error::Error for TimeoutElapsed {}

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::error::Elapsed as TimeoutElapsed;

#[cfg(not(target_arch = "wasm32"))]
pub async fn timeout<F, T>(duration: Duration, fut: F) -> Result<T, TimeoutElapsed>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(duration, fut).await
}

#[cfg(target_arch = "wasm32")]
pub async fn timeout<F, T>(duration: Duration, fut: F) -> Result<T, TimeoutElapsed>
where
    F: std::future::Future<Output = T>,
{
    use futures::FutureExt;
    use futures::future::{Either, select};
    use futures::pin_mut;

    let sleep = gloo_timers::future::sleep(duration).fuse();
    pin_mut!(sleep);
    pin_mut!(fut);
    match select(fut, sleep).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right((_, _)) => Err(TimeoutElapsed),
    }
}
