//! yousj-js · Phase 7：Promise / 微任务。
//!
//! - `JsPromise`：三态状态机（pending → fulfilled / rejected），反应列表。
//! - `Microtask`：微任务队列项。主脚本结束后先排空微任务，再跑宏任务；
//!   每个宏任务执行后也排空微任务（标准事件循环语义）。
//! - 反应（`then`/`catch`/`finally` 回调、`Promise.all`/`race` 计数、
//!   async/await 的顶层解包）以数据形式挂在 promise 上，由解释器的
//!   drain 循环执行——`Native` 函数经 `NativeCtx.microtasks` 入队，
//!   无需解释器访问权。
//! - 刻意简化：thenable 同化只认 `Value::Promise`（普通对象的 `.then`
//!   不触发同化）；`Promise.all`/`race` 只接受数组。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::value::Value;
use crate::value::AsyncGenRef;

/// Promise 引用（解释器与用户代码共享所有权）。
pub type PromiseRef = Rc<RefCell<JsPromise>>;

/// `Promise.all` 共享状态引用（phase 12：环断开遍历用）。
pub type AllStateRef = Rc<RefCell<AllState>>;

/// Promise 状态。
#[derive(Debug, Clone)]
pub enum PromiseState {
    Pending,
    Fulfilled(Value),
    Rejected(Value),
}

/// 挂在 promise 上的反应。settle 时按注册顺序转成微任务。
#[derive(Debug, Clone)]
pub struct Reaction {
    pub kind: ReactionKind,
    /// `then` 链的下游 promise（all/race/async-unwrap 不用它，为 None）。
    pub next: Option<PromiseRef>,
    /// 源 promise（拒绝转发时从 unhandled 移除用）。
    pub source: PromiseRef,
}

#[derive(Debug, Clone)]
pub enum ReactionKind {
    /// 普通 `then(onF, onR)` / `catch` / `finally` 回调。
    Handler {
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        is_finally: bool,
    },
    /// `Promise.all` 的第 `index` 个输入。
    All {
        state: Rc<RefCell<AllState>>,
        index: usize,
    },
    /// `Promise.race`：第一个 settle 的赢。
    Race {
        result: PromiseRef,
    },
    /// async 函数顶层：把 desugar 链的完成值解包到外层 promise。
    AsyncUnwrap {
        outer: PromiseRef,
    },
    /// phase 15：async 生成器体完成 → 通知生成器（resolve/reject 挂起的请求）。
    AsyncGenDone {
        agen: AsyncGenRef,
    },
}

/// `Promise.all` 的共享计数器。
#[derive(Debug)]
pub struct AllState {
    pub values: Vec<Option<Value>>,
    pub remaining: usize,
    pub result: PromiseRef,
    /// 已有输入拒绝（或已完成）：后续 settle 直接忽略。
    pub done: bool,
}

/// Promise 本体。
#[derive(Debug)]
pub struct JsPromise {
    pub state: PromiseState,
    pub reactions: Vec<Reaction>,
    /// 是否挂过拒绝处理器（unhandled rejection 报告用）。
    pub handled: bool,
}

impl JsPromise {
    pub fn pending() -> PromiseRef {
        Rc::new(RefCell::new(JsPromise {
            state: PromiseState::Pending,
            reactions: Vec::new(),
            handled: false,
        }))
    }

    pub fn resolved(value: Value) -> PromiseRef {
        Rc::new(RefCell::new(JsPromise {
            state: PromiseState::Fulfilled(value),
            reactions: Vec::new(),
            handled: false,
        }))
    }

    pub fn rejected(reason: Value) -> PromiseRef {
        Rc::new(RefCell::new(JsPromise {
            state: PromiseState::Rejected(reason),
            reactions: Vec::new(),
            handled: false,
        }))
    }

    pub fn is_pending(p: &PromiseRef) -> bool {
        matches!(p.borrow().state, PromiseState::Pending)
    }
}

/// 微任务队列项。
#[derive(Debug, Clone)]
pub enum Microtask {
    /// 执行一个反应（settle 时由 `settle_promise` 入队）。
    Dispatch {
        reaction: Reaction,
        value: Value,
        rejected: bool,
    },
    /// `queueMicrotask(cb)` 的普通回调。
    Call {
        callback: Value,
    },
}

/// 微任务队列上限（防 `then` 自增殖；单个任务仍受 STEP_LIMIT 约束）。
pub const MAX_MICROTASKS: usize = 100_000;

/// `new Promise(executor)` 传给 executor 的 resolve/reject 函数值。
/// 解释器在 `call_value` 中直接处理（需要入队微任务，`Native` 做不到）。
#[derive(Debug, Clone)]
pub struct PromiseSettler {
    pub promise: PromiseRef,
    pub reject: bool,
}

pub type PromiseSettlerRef = Rc<PromiseSettler>;

impl PromiseSettler {
    pub fn resolve(promise: PromiseRef) -> PromiseSettlerRef {
        Rc::new(PromiseSettler {
            promise,
            reject: false,
        })
    }

    pub fn reject(promise: PromiseRef) -> PromiseSettlerRef {
        Rc::new(PromiseSettler {
            promise,
            reject: true,
        })
    }
}

/// 把反应挂到 promise 上。若已 settle，立即把 dispatch 排进微任务。
/// 返回该 promise 当前是否已是 rejected 且尚无拒绝处理器（unhandled 判定用）。
pub fn attach_reaction(
    p: &PromiseRef,
    r: Reaction,
    queue: &mut VecDeque<Microtask>,
) -> bool {
    // 有拒绝处理器的 Handler，或会传播拒绝的 All/Race/AsyncUnwrap，
    // 都视为"已处理"（拒绝被观测到了；下游 promise 另行记账）。
    let handles_rejection = match &r.kind {
        ReactionKind::Handler {
            on_rejected: Some(_),
            ..
        } => true,
        ReactionKind::All { .. } | ReactionKind::Race { .. } => true,
        // AsyncUnwrap：把命运交给 outer；outer 会另行记账。
        // 但此处不标记 handled —— 由调用方在 adopt 时处理。
        // （为简单起见，AsyncUnwrap 也视为已处理，因为 outer 会承接。）
        ReactionKind::AsyncUnwrap { .. } => true,
        _ => false,
    };
    if handles_rejection {
        p.borrow_mut().handled = true;
    }
    let settled = match &p.borrow().state {
        PromiseState::Pending => None,
        PromiseState::Fulfilled(v) => Some((false, v.clone())),
        PromiseState::Rejected(r) => Some((true, r.clone())),
    };
    let mut b = p.borrow_mut();
    b.reactions.push(r);
    if let Some((rejected, value)) = settled {
        let r = b.reactions.pop().expect("just pushed");
        drop(b);
        queue.push_back(Microtask::Dispatch {
            reaction: r,
            value,
            rejected,
        });
        // 已 settle 且是拒绝：看这次挂的反应能不能处理它。
        if rejected {
            return !b_handled(&p);
        }
        return false;
    }
    false
}

fn b_handled(p: &PromiseRef) -> bool {
    p.borrow().handled
}

/// settle 一个 promise（只认第一次；后续调用静默忽略，符合规范）。
/// 反应按注册顺序转成微任务。返回 `(是否为拒绝, 是否无人处理)`。
pub fn settle_promise(
    p: &PromiseRef,
    rejected: bool,
    value: Value,
    queue: &mut VecDeque<Microtask>,
) -> (bool, bool) {
    let reactions = {
        let mut b = p.borrow_mut();
        if !matches!(b.state, PromiseState::Pending) {
            return (false, false);
        }
        b.state = if rejected {
            PromiseState::Rejected(value.clone())
        } else {
            PromiseState::Fulfilled(value.clone())
        };
        std::mem::take(&mut b.reactions)
    };
    // 拒绝且没有任何 on_rejected 反应 → 候选 unhandled。
    let unhandled = rejected
        && !reactions.iter().any(|r| {
            matches!(
                &r.kind,
                ReactionKind::Handler {
                    on_rejected: Some(_),
                    ..
                }
            )
        });
    for r in reactions {
        queue.push_back(Microtask::Dispatch {
            reaction: r,
            value: value.clone(),
            rejected,
        });
    }
    (rejected, unhandled)
}
