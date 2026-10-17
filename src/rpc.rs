//! 按名字调用 core 的能力。
//!
//! 逻辑层（app 的 `applogic`，可能是热更新下发的 WASM）不能像原生代码那样把闭包塞进 ECS
//! 线程去操作组件，所以 core 把对外能力登记成「方法名 → 处理函数」，参数与结果都是 JSON：
//!
//! ```text
//! call("watchface.set_current", {"addr": "...", "id": "..."}, None).await
//! ```
//!
//! 需要进度的方法收到一个 [`Progress`] 回调，每条进度也是 JSON。
//!
//! 新增方法：写一个 `async fn(args: 某个 Deserialize 类型, progress: Option<Progress>) ->
//! anyhow::Result<某个 Serialize 类型>`，在 [`rpc_methods!`] 里登记一行。逻辑层随即可以经宿主
//! 能力 `core.call` 调用它，不需要写任何胶水。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::anyhow;
use serde_json::Value;

/// 进度回调：每次调用投递一条 JSON。
pub type Progress = Arc<dyn Fn(Value) + Send + Sync>;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type Handler = fn(Value, Option<Progress>) -> BoxFuture<anyhow::Result<Value>>;

macro_rules! rpc_methods {
    ( $( $name:literal => $f:path ; )* ) => {
        const METHODS: &[(&str, Handler)] = &[
            $( ($name, {
                fn entry(args: Value, progress: Option<Progress>) -> BoxFuture<anyhow::Result<Value>> {
                    Box::pin(async move {
                        let args = serde_json::from_value(args)
                            .map_err(|err| anyhow!("invalid arguments for `{}`: {err}", $name))?;
                        #[allow(clippy::let_unit_value)]
                        let output = $f(args, progress).await?;
                        Ok(serde_json::to_value(output)?)
                    })
                }
                entry as Handler
            }) ),*
        ];
    };
}

rpc_methods! {
    "data.request" => methods::data_request;
}

/// 全部方法名。
pub fn methods() -> Vec<&'static str> {
    METHODS.iter().map(|(name, _)| *name).collect()
}

/// 调用一个方法。
pub async fn call(method: &str, args: Value, progress: Option<Progress>) -> anyhow::Result<Value> {
    let handler = METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, handler)| *handler)
        .ok_or_else(|| anyhow!("unknown core method `{method}`"))?;
    handler(args, progress).await
}

mod methods {
    use serde::Deserialize;
    use serde_json::Value;

    use super::Progress;
    use crate::device::data::DeviceDataType;

    #[derive(Deserialize)]
    pub struct DataRequest {
        addr: String,
        r#type: DeviceDataType,
    }

    pub async fn data_request(args: DataRequest, _: Option<Progress>) -> anyhow::Result<Value> {
        crate::device::data::request_device_data_json(args.addr, args.r#type).await
    }
}
