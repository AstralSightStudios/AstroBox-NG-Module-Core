pub mod access;
pub mod graph;
pub mod runtime;

pub use bevy_ecs::prelude::{Bundle, Component, Entity, World};

// 非WASM平台支持多线程，采用默认初始化方式
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use crate::ecs::runtime::Runtime;
    use once_cell::sync::OnceCell;
    use std::{cell::Cell, ptr, thread};
    use tokio::sync::oneshot;

    type Job = Box<dyn FnOnce(&mut Runtime) + Send + 'static>;

    // ECS Runtime 闭包任务发端
    static RT_TX: OnceCell<flume::Sender<Job>> = OnceCell::new();

    // 本地线程 ECS Runtime 指针数据，用于非跨线程状态下的零开销访问
    thread_local! {
        static IN_RT_THREAD: Cell<bool> = Cell::new(false);
        static RT_LOCAL_PTR: Cell<*mut Runtime> = Cell::new(ptr::null_mut());
    }

    /// 使用指定的初始化方法和栈大小初始化 ECS Runtime
    fn init_runtime_internal<F>(make_rt: F, stack_size: Option<usize>)
    where
        F: FnOnce() -> Runtime + Send + 'static,
    {
        let (tx, rx) = flume::unbounded::<Job>();
        let _ = RT_TX.set(tx);

        // 包装初始化任务
        let thread_job = move || {
            let mut rt = make_rt();

            IN_RT_THREAD.with(|flag| flag.set(true));
            RT_LOCAL_PTR.with(|cell| cell.set(&mut rt as *mut Runtime));

            while let Ok(job) = rx.recv() {
                job(&mut rt);
            }
        };

        let builder = thread::Builder::new().name("ecs-runtime".into());
        let builder = if let Some(size) = stack_size {
            builder.stack_size(size)
        } else {
            builder
        };

        // 将初始化任务spawn到ECS线程中
        builder
            .spawn(thread_job)
            .expect("Failed to spawn ECS runtime thread");

        log::info!("ECS Runtime initialization completed!");
    }

    pub fn init_runtime_with<F>(make_rt: F)
    where
        F: FnOnce() -> Runtime + Send + 'static,
    {
        init_runtime_internal(make_rt, None);
    }

    pub fn init_runtime_with_stack<F>(make_rt: F, stack_size: usize)
    where
        F: FnOnce() -> Runtime + Send + 'static,
    {
        log::info!(
            "Initializing ECS with custom stack size ({} bytes)...",
            stack_size
        );
        init_runtime_internal(make_rt, Some(stack_size));
    }

    pub fn init_runtime_default() {
        log::info!("Initializing ECS with default configuration...");
        init_runtime_with(Runtime::new);
    }

    pub fn init_runtime_default_with_stack(stack_size: usize) {
        log::info!(
            "Initializing ECS with default configuration and custom stack size ({} bytes)...",
            stack_size
        );
        init_runtime_with_stack(Runtime::new, stack_size);
    }

    /// 将闭包任务扔到ECS线程中执行
    pub async fn with_rt_mut<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Runtime) -> R + Send + 'static,
        R: Send + 'static,
    {
        // 如果调用方是ECS线程，则直接从thread_local里拿rt
        // 这样可以避免跨线程传递数据，减少性能开销
        let in_rt = IN_RT_THREAD.with(|flag| flag.get());
        if in_rt {
            return RT_LOCAL_PTR.with(|cell| {
                let ptr = cell.get();
                debug_assert!(!ptr.is_null(), "Runtime thread-local pointer not set");
                unsafe { f(&mut *ptr) }
            });
        }

        // 如果调用方不是ECS线程，则将闭包任务扔到ECS线程中执行
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

    pub fn in_rt_thread() -> bool {
        IN_RT_THREAD.with(|flag| flag.get())
    }

    pub fn try_with_rt_local_mut<F, R>(f: F) -> Option<R>
    where
        F: FnOnce(&mut Runtime) -> R,
    {
        if !in_rt_thread() {
            return None;
        }

        Some(RT_LOCAL_PTR.with(|cell| {
            let ptr = cell.get();
            debug_assert!(!ptr.is_null(), "Runtime thread-local pointer not set");
            unsafe { f(&mut *ptr) }
        }))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

// WASM平台不支持多线程，采用特殊初始化方式，所有RT访问转接到thread_local
// 由于WASM本身是单线程环境，该操作不会导致任何问题
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

    pub fn in_rt_thread() -> bool {
        RT.with(|cell| cell.borrow().is_some())
    }

    pub fn try_with_rt_local_mut<F, R>(f: F) -> Option<R>
    where
        F: FnOnce(&mut Runtime) -> R,
    {
        RT.with(|cell| {
            let ptr = cell.as_ptr();
            unsafe {
                let rt_opt = &mut *ptr;
                rt_opt.as_mut().map(f)
            }
        })
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm::*;
