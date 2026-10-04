//! yousj-js · Phase 7：`fetch` 宿主接口。
//!
//! 设计（与 `DomHost` 同构）：
//! - js-engine 保持零依赖，不直接做网络。`fetch(url)` 全局函数经
//!   `NativeCtx.fetch_host` 调用宿主；宿主同步返回结果，引擎把它包成
//!   已 settle 的 Promise（网络本身是同步跑的，对 JS 侧仍是异步 API）。
//! - 未绑定宿主（`bind_fetch` 未调用）时 `fetch()` 返回 rejected Promise
//!  （"fetch not implemented"），而不是抛错——与浏览器"网络失败走
//!   catch"的体感一致。
//! - `NullFetch`：显式默认实现，行为与未绑定一致，供嵌入方占位。

use std::rc::Rc;

/// fetch 宿主：同步取回一个 URL 的结果。
pub trait FetchHost {
    /// 成功时返回 `(status, body)`；失败时返回错误信息（进 rejected）。
    fn fetch(&self, url: &str) -> Result<FetchResponse, String>;
}

/// fetch 成功的结果。
#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub status: u16,
    pub body: String,
}

impl FetchResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        FetchResponse {
            status,
            body: body.into(),
        }
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// 默认空实现：永远返回"未实现"错误（→ rejected Promise）。
#[derive(Debug, Clone, Default)]
pub struct NullFetch;

impl FetchHost for NullFetch {
    fn fetch(&self, _url: &str) -> Result<FetchResponse, String> {
        Err("fetch not implemented".to_string())
    }
}

/// `Rc<dyn FetchHost>` 的便捷别名。
pub type FetchHostRef = Rc<dyn FetchHost>;
