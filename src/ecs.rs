pub mod component;
pub mod entity;
pub mod fastlane;
pub mod logic_component;
pub mod runtime;
pub mod system;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use crate::ecs::runtime::Runtime;
    use once_cell::sync::OnceCell;
    use std::{cell::Cell, ptr, thread};
    use tokio::sync::oneshot;

    type Job = Box<dyn FnOnce(&mut Runtime) + Send + 'static>;

    static RT_TX: OnceCell<flume::Sender<Job>> = OnceCell::new();

    thread_local! {
        static IN_RT_THREAD: Cell<bool> = Cell::new(false);
        static RT_LOCAL_PTR: Cell<*mut Runtime> = Cell::new(ptr::null_mut());
    }

    pub fn init_runtime_with<F>(make_rt: F)
    where
        F: FnOnce() -> Runtime + Send + 'static,
    {
        let (tx, rx) = flume::unbounded::<Job>();
        let _ = RT_TX.set(tx);

        thread::spawn(move || {
            let mut rt = make_rt();

            IN_RT_THREAD.with(|flag| flag.set(true));
            RT_LOCAL_PTR.with(|cell| cell.set(&mut rt as *mut Runtime));

            while let Ok(job) = rx.recv() {
                job(&mut rt);
            }
        });

        log::info!("ECS Runtime initialization completed!");
    }

    pub fn init_runtime_default() {
        log::info!("Initializing ECS with default configuration...");
        init_runtime_with(Runtime::new);
    }

    pub async fn with_rt_mut<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Runtime) -> R + Send + 'static,
        R: Send + 'static,
    {
        let in_rt = IN_RT_THREAD.with(|flag| flag.get());
        if in_rt {
            return RT_LOCAL_PTR.with(|cell| {
                let ptr = cell.get();
                debug_assert!(!ptr.is_null(), "Runtime thread-local pointer not set");
                unsafe { f(&mut *ptr) }
            });
        }

        let tx = RT_TX
            .get()
            .expect("RT not initialized. Call ecs::init_runtime_* first.")
            .clone();

        let (ret_tx, ret_rx) = oneshot::channel::<R>();

        let job: Job = Box::new(move |rt: &mut Runtime| {
            let out = f(rt);
            let _ = ret_tx.send(out);
        });

        tx.send(job).expect("runtime thread has stopped");
        ret_rx.await.expect("runtime thread dropped the response")
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use crate::ecs::runtime::Runtime;
    use std::cell::RefCell;

    thread_local! {
        static RT: RefCell<Option<Runtime>> = RefCell::new(None);
    }

    pub fn init_runtime_with<F>(make_rt: F)
    where
        F: FnOnce() -> Runtime + 'static,
    {
        RT.with(|cell| {
            *cell.borrow_mut() = Some(make_rt());
        });

        log::info!("ECS Runtime initialization completed!");
    }

    pub fn init_runtime_default() {
        log::info!("Initializing ECS with default configuration...");
        init_runtime_with(Runtime::new);
    }

    pub async fn with_rt_mut<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Runtime) -> R + 'static,
        R: 'static,
    {
        RT.with(|cell| {
            // SAFETY: 因为WASM实际上他妈是个单线程环境，因此获取可变指针是完全安全的
            let ptr = cell.as_ptr();
            unsafe {
                let rt_opt = &mut *ptr;
                let rt = rt_opt
                    .as_mut()
                    .expect("RT not initialized. Call ecs::init_runtime_* first.");
                f(rt)
            }
        })
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm::*;
