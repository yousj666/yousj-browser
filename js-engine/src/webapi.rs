//! yousj-js · Phase 13：Web API 补完。
//!
//! 设计（沿用 phase 10 `Intl` 的"标签对象"模式，**零新增 `Value` 变体**）：
//! - 实例是普通 `JsObject`，`tag` 标记类型（`"WebSocket"` / `"Worker"` /
//!   `"Storage"` / `"URL"` / `"USP"` / `"TextEncoder"` / `"TextDecoder"` /
//!   `"Blob"` / `"FormData"`），纯状态放 `__`-前缀自有属性；
//! - 纯方法挂在各自原型上做 `Native`（经 `ctx.this` 读 `__` 状态）；
//! - 需要解释器参与的方法（任务队列、回调执行、TypedArray 构造）走
//!   `try_host_method_vals` 的标签拦截（`ws_send` / `ws_close` /
//!   `worker_post_message` / `worker_terminate` / `*_for_each` / `te_encode`）；
//! - 网络与文件系统走宿主 trait（`WsHost`），引擎保持零依赖；
//!   未绑定宿主时给出清晰的异步错误事件，而非 panic。
//!
//! 已知简化（文档化）：
//! - `WebSocket` 握手是同步的（宿主同步返回连接）；真实网络唤醒需宿主
//!   在数据到达时调用 `Interpreter::pump_websockets()`（本文件提供）。
//! - `Worker` 在主线程内用子解释器模拟执行；`postMessage` 数据按引用传递
//!   （无结构化克隆）；暂不支持 `importScripts` / `close()` / 嵌套终止语义。
//! - `URL` 是 WHATWG 的子集实现：无 IDNA、无默认端口省略、无 `username` 特殊编码。
//! - `localStorage`/`sessionStorage` 支持具名属性读写；`delete` 操作符未挂钩
//!   （用 `removeItem`）。
//! - `setInterval`/`clearInterval` 挂在 `window` 上（与 `setTimeout` 一致）。

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use crate::interpreter::{ta_new, Interpreter, TaskKind};
use crate::object::BuiltinProtos;
use crate::promise::{settle_promise, JsPromise};
use crate::value::{
    type_err, FlowError, JsArrayBuffer, JsObject, NativeCtx, NativeFn, ObjectRef, TypedKind, Value,
};

// ---------------------------------------------------------------------------
// 1. WebSocket 宿主
// ---------------------------------------------------------------------------

/// Worker / WebSocket 事件投递用的跨解释器消息通道。
pub type InboxRef = Rc<RefCell<VecDeque<Value>>>;

/// WebSocket 连接句柄（宿主实现；`Rc<dyn WsConn>` 以便多 socket 共享宿主）。
pub type WsConnRef = Rc<dyn WsConn>;

/// `bind_websocket_host` 用的宿主 trait：同步完成握手（或返回清晰错误）。
pub trait WsHost {
    /// 成功返回连接；失败返回人类可读的错误（→ `error` + `close` 事件）。
    fn connect(&self, url: &str, protocols: &[String]) -> Result<WsConnRef, String>;
}

/// `Rc<dyn WsHost>` 便捷别名。
pub type WsHostRef = Rc<dyn WsHost>;

/// WebSocket 收到的事件（宿主 `poll()` 一次性吐出）。
#[derive(Debug, Clone)]
pub enum WsEvent {
    Message(WsMsg),
    Closed { code: u16, reason: String },
    Error(String),
}

/// 一条收到的消息。
#[derive(Debug, Clone)]
pub enum WsMsg {
    Text(String),
    Binary(Vec<u8>),
}

/// 宿主侧连接：同步发送 + 非阻塞收包 + 关闭。
pub trait WsConn {
    fn send_text(&self, data: &str) -> Result<(), String>;
    fn send_binary(&self, data: &[u8]) -> Result<(), String>;
    /// 吐出当前积压的事件（无则空 Vec）。
    fn poll(&self) -> Vec<WsEvent>;
    fn close(&self);
    fn is_open(&self) -> bool;
}

/// 默认空实现：永远连接失败（→ 清晰的 `error` 事件），不 panic。
#[derive(Debug, Default)]
pub struct NullWsHost;

impl WsHost for NullWsHost {
    fn connect(&self, _url: &str, _protocols: &[String]) -> Result<WsConnRef, String> {
        Err("WebSocket not implemented: no host bound (call bind_websocket_host)".to_string())
    }
}

// ---------------------------------------------------------------------------
// 2. Storage 持久化：文件格式为每行 `pct(key)\tpct(value)`，`pct` 见下。
//    存储本体放在实例对象的 `__s_data` 内部对象属性上（`Native` 可直接读写），
//    持久化路径放在 `__s_persist` 上；`bind` 见 `set_localstorage_path`。
// ---------------------------------------------------------------------------

/// 百分号编码（`%`/`\t`/`\n`/`\r`/`\\`）。
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            '\\' => out.push_str("%5C"),
            _ => out.push(c),
        }
    }
    out
}

fn pct_decode(s: &str) -> Result<String, ()> {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '%' {
            let h: String = it.by_ref().take(2).collect();
            if h.len() != 2 {
                return Err(());
            }
            let b = u8::from_str_radix(&h, 16).map_err(|_| ())?;
            out.push(b as char);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 3. URL 解析（WHATWG 子集）
// ---------------------------------------------------------------------------

/// 解析出的 URL 各组件。
#[derive(Debug, Clone, Default)]
pub struct ParsedUrl {
    pub protocol: String, // 含冒号，如 "https:"
    pub username: String,
    pub password: String,
    pub hostname: String, // 小写
    pub port: String,     // 不含冒号
    pub pathname: String, // 以 "/" 开头
    pub search: String,   // 含 "?"，无则 ""
    pub hash: String,     // 含 "#"，无则 ""
}

impl ParsedUrl {
    pub fn host(&self) -> String {
        if self.port.is_empty() {
            self.hostname.clone()
        } else {
            format!("{}:{}", self.hostname, self.port)
        }
    }

    pub fn href(&self) -> String {
        let mut s = format!("{}//", self.protocol);
        if !self.username.is_empty() || !self.password.is_empty() {
            s.push_str(&self.username);
            if !self.password.is_empty() {
                s.push(':');
                s.push_str(&self.password);
            }
            s.push('@');
        }
        s.push_str(&self.host());
        s.push_str(&self.pathname);
        s.push_str(&self.search);
        s.push_str(&self.hash);
        s
    }

    /// `protocol//host`；非特殊方案返回 "null"。
    pub fn origin(&self) -> String {
        match self.protocol.as_str() {
            "http:" | "https:" | "ws:" | "wss:" | "ftp:" => {
                format!("{}//{}", self.protocol, self.host())
            }
            _ => "null".to_string(),
        }
    }
}

/// 解析 URL（`base` 用于相对引用解析）。失败返回人类可读原因。
pub fn parse_url(input: &str, base: Option<&ParsedUrl>) -> Result<ParsedUrl, String> {
    let s = input.trim_matches(|c: char| c.is_whitespace() || (c as u32) < 0x20);
    // 拆 fragment
    let (s, hash) = match s.split_once('#') {
        Some((a, b)) => (a, format!("#{b}")),
        None => (s, String::new()),
    };
    // 拆 query
    let (s, search) = match s.split_once('?') {
        Some((a, b)) => (a, format!("?{b}")),
        None => (s, String::new()),
    };

    // 是否有 scheme
    if let Some((scheme, rest)) = split_scheme(s) {
        let mut u = ParsedUrl {
            protocol: format!("{}:", scheme.to_lowercase()),
            ..Default::default()
        };
        parse_authority_path(&mut u, rest)?;
        u.search = search;
        u.hash = hash;
        Ok(u)
    } else {
        // 相对引用：必须有 base
        let b = base.ok_or_else(|| format!("Invalid URL: {input}"))?;
        let mut u = b.clone();
        u.search = search;
        u.hash = hash;
        if let Some(after) = s.strip_prefix("//") {
            // 网络路径引用：继承 base 的 scheme
            u.username.clear();
            u.password.clear();
            u.hostname.clear();
            u.port.clear();
            parse_authority_path(&mut u, after)?;
        } else if s.starts_with('/') {
            u.pathname = normalize_path(s);
        } else if s.is_empty() {
            // 只有 query/hash 变化：pathname 保持
        } else {
            // 合并路径
            let merged = if u.pathname.contains('/') {
                let base_dir = u.pathname.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                format!("{base_dir}/{s}")
            } else {
                format!("/{s}")
            };
            u.pathname = normalize_path(&merged);
        }
        Ok(u)
    }
}

/// 切出 `scheme:` 前缀（`[a-zA-Z][a-zA-Z0-9+.-]*:`）。
fn split_scheme(s: &str) -> Option<(&str, &str)> {
    let mut it = s.char_indices();
    let (_, first) = it.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    for (i, c) in it {
        if c == ':' {
            return Some((&s[..i], &s[i + 1..]));
        }
        if !(c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
            return None;
        }
    }
    None
}

/// 解析 `//authority` + path（`rest` 为 scheme 之后的部分）。
fn parse_authority_path(u: &mut ParsedUrl, rest: &str) -> Result<(), String> {
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    // authority 到第一个 /, ?, # 为止（?, # 已在外层切掉，这里只防 /）
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // userinfo
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    if let Some(ui) = userinfo {
        match ui.split_once(':') {
            Some((name, pass)) => {
                u.username = name.to_string();
                u.password = pass.to_string();
            }
            None => u.username = ui.to_string(),
        }
    }
    // host[:port]（支持 [v6]）
    if let Some(hp) = hostport.strip_prefix('[') {
        let end = hp.find(']').ok_or("Invalid URL: bad IPv6 host")?;
        u.hostname = hp[..end].to_lowercase();
        let after = &hp[end + 1..];
        if let Some(p) = after.strip_prefix(':') {
            if !p.chars().all(|c| c.is_ascii_digit()) {
                return Err("Invalid URL: bad port".to_string());
            }
            u.port = p.to_string();
        } else if !after.is_empty() {
            return Err("Invalid URL: bad authority".to_string());
        }
    } else if let Some(i) = hostport.rfind(':') {
        let (h, p) = (&hostport[..i], &hostport[i + 1..]);
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return Err("Invalid URL: bad port".to_string());
        }
        u.hostname = h.to_lowercase();
        u.port = p.to_string();
    } else {
        u.hostname = hostport.to_lowercase();
    }
    if u.hostname.is_empty() {
        return Err("Invalid URL: empty host".to_string());
    }
    u.pathname = normalize_path(if path.is_empty() { "/" } else { path });
    Ok(())
}

/// 解析 `.` / `..` 路径段（保留百分号编码原样；保留尾部斜杠）。
fn normalize_path(path: &str) -> String {
    let trailing_slash = path.ends_with('/');
    let mut segs: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "." => {}
            "" => {
                // 只保留开头的空段（即前导 "/"）
                if segs.is_empty() {
                    segs.push("");
                }
            }
            ".." => {
                if segs.len() > 1 {
                    segs.pop();
                }
            }
            _ => segs.push(seg),
        }
    }
    let mut out = segs.join("/");
    if out.is_empty() {
        out.push('/');
    } else if trailing_slash && !out.ends_with('/') {
        out.push('/');
    }
    out
}

// ---------------------------------------------------------------------------
// 4. base64（atob / btoa 用，零依赖手写）
// ---------------------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准 base64 编码（带 `=` 填充）。
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let mut n: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            n |= (b as u32) << (16 - 8 * i);
        }
        let pad = 3 - chunk.len();
        for i in 0..4 - pad {
            out.push(B64_ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
        for _ in 0..pad {
            out.push('=');
        }
    }
    out
}

/// base64 解码（容忍空白；非法字符/填充错误 → Err）。
pub fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let clean: Vec<u8> = s
        .bytes()
        .filter(|b| !b" \t\n\r".contains(b))
        .collect();
    if clean.len() % 4 != 0 {
        return Err("InvalidCharacterError: bad base64 length".to_string());
    }
    let val = |b: u8| -> Result<u32, String> {
        match b {
            b'A'..=b'Z' => Ok((b - b'A') as u32),
            b'a'..=b'z' => Ok((b - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((b - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err("InvalidCharacterError: bad base64 character".to_string()),
        }
    };
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for chunk in clean.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 {
            return Err("InvalidCharacterError: bad base64 padding".to_string());
        }
        // `=` 之后不能再有数据字符
        if chunk[..4 - pad].contains(&b'=') {
            return Err("InvalidCharacterError: bad base64 padding".to_string());
        }
        let mut n: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            let v = if b == b'=' { 0 } else { val(b)? };
            n |= v << (18 - 6 * i);
        }
        for i in 0..3 - pad {
            out.push(((n >> (16 - 8 * i)) & 0xFF) as u8);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 5. 小工具
// ---------------------------------------------------------------------------

// (this_internal 已移除：统一用 check_tag)

/// 构造"标签对象"：`tag` + 原型。
fn tagged_object(proto: &ObjectRef, tag: &str) -> ObjectRef {
    let o = Rc::new(RefCell::new(JsObject::with_proto(Some(proto.clone()))));
    o.borrow_mut().tag = Some(tag.to_string());
    o
}

/// 新建 JS 数组值（`Native` 上下文用）。
fn js_array(ctx: &NativeCtx, elems: Vec<Value>) -> Value {
    let arr = Rc::new(RefCell::new(crate::value::JsArray::new(elems)));
    arr.borrow_mut().proto = Some(ctx.protos.array.clone());
    Value::Array(arr)
}

/// 从 `__p_pairs`（`[[k,v],…]` 数组）读出 owned 对。
pub(crate) fn usp_pairs_of(obj: &ObjectRef) -> Vec<(String, String)> {
    let pairs_val = obj.borrow().get("__p_pairs");
    let Some(Value::Array(arr)) = pairs_val else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for el in arr.borrow().elems.iter() {
        if let Value::Array(pair) = el {
            let e = pair.borrow();
            if e.elems.len() >= 2 {
                out.push((e.elems[0].to_js_string(), e.elems[1].to_js_string()));
            }
        }
    }
    out
}

/// 把 owned 对写回 `__p_pairs`。
fn usp_pairs_set(obj: &ObjectRef, protos: &BuiltinProtos, pairs: &[(String, String)]) {
    let mut elems = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        let pair = Rc::new(RefCell::new(crate::value::JsArray::new(vec![
            Value::String(k.clone()),
            Value::String(v.clone()),
        ])));
        pair.borrow_mut().proto = Some(protos.array.clone());
        elems.push(Value::Array(pair));
    }
    let arr = Rc::new(RefCell::new(crate::value::JsArray::new(elems)));
    arr.borrow_mut().proto = Some(protos.array.clone());
    obj.borrow_mut().set("__p_pairs", Value::Array(arr));
}

/// `application/x-www-form-urlencoded` 百分号编码（`toString` 用）。
fn form_urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// urlencoded 百分号解码（`+` → 空格；非法序列 → 原样保留）。
fn form_urldecode(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let mut it = s.as_bytes().iter().peekable();
    while let Some(&b) = it.next() {
        if b == b'+' {
            bytes.push(b' ');
        } else if b == b'%' {
            let h: Vec<u8> = it.by_ref().take(2).copied().collect();
            if h.len() == 2 {
                let v = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
                if let (Some(a), Some(b2)) = (v(h[0]), v(h[1])) {
                    bytes.push(a * 16 + b2);
                    continue;
                }
            }
            bytes.push(b'%');
            bytes.extend_from_slice(&h);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
// ---------------------------------------------------------------------------
// 6. 解释器侧：安装 / 构造器 / 方法分发 / 内部任务
// ---------------------------------------------------------------------------

/// 存活 WebSocket：JS 对象 + 宿主连接。
pub(crate) struct WsLive {
    pub obj: ObjectRef,
    pub conn: WsConnRef,
}

/// 存活 Worker：子解释器 + worker→父消息通道。
/// （JS 对象由 `workers` 映射的键隐含持有，不另存。）
pub(crate) struct WorkerEntry {
    pub interp: Box<Interpreter>,
    pub inbox: InboxRef,
    pub terminated: bool,
    /// 构造/运行 worker 脚本时的错误 → 下一次 drain 时以父 `error` 事件报告。
    pub error_pending: Option<String>,
}

impl Interpreter {
    // ---------------- 安装 ----------------

    /// 安装 Web API 全局：构造器 + `localStorage`/`sessionStorage` + `atob`/`btoa`。
    pub(crate) fn install_web(&mut self) {
        let mut protos: HashMap<String, ObjectRef> = HashMap::new();
        for name in [
            "WebSocket",
            "Worker",
            "URL",
            "USP",
            "Storage",
            "TextEncoder",
            "TextDecoder",
            "Blob",
            "FormData",
        ] {
            let p = Rc::new(RefCell::new(JsObject::with_proto(Some(
                self.protos.object.clone(),
            ))));
            protos.insert(name.to_string(), p);
        }
        let proto = |protos: &HashMap<String, ObjectRef>, n: &str| protos[n].clone();

        // 构造器（`eval_new` 按指针识别；实例原型链 → instanceof 可用）。
        for name in [
            "WebSocket",
            "Worker",
            "URL",
            "URLSearchParams",
            "TextEncoder",
            "TextDecoder",
            "Blob",
            "FormData",
        ] {
            let tag = match name {
                "URLSearchParams" => "USP",
                n => n,
            };
            let ctor = self.make_ctor(name, proto(&protos, tag));
            self.web_ctors.insert(name.to_string(), ctor.clone());
            self.define_global(name, Value::Object(ctor));
        }

        // URL 原型
        proto(&protos, "URL")
            .borrow_mut()
            .set("toString", Value::Native(native_url_to_string));
        proto(&protos, "URL")
            .borrow_mut()
            .set("valueOf", Value::Native(native_url_to_string));

        // URLSearchParams 原型（`forEach` 走解释器拦截）
        {
            let p = proto(&protos, "USP");
            let mut b = p.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("append", native_usp_append),
                ("delete", native_usp_delete),
                ("get", native_usp_get),
                ("getAll", native_usp_get_all),
                ("has", native_usp_has),
                ("set", native_usp_set),
                ("sort", native_usp_sort),
                ("toString", native_usp_to_string),
                ("keys", native_usp_keys),
                ("values", native_usp_values),
                ("entries", native_usp_entries),
            ];
            for (n, f) in methods {
                b.set(n, Value::Native(*f));
            }
        }

        // Storage 原型
        {
            let p = proto(&protos, "Storage");
            let mut b = p.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("getItem", native_storage_get_item),
                ("setItem", native_storage_set_item),
                ("removeItem", native_storage_remove_item),
                ("clear", native_storage_clear),
                ("key", native_storage_key),
            ];
            for (n, f) in methods {
                b.set(n, Value::Native(*f));
            }
        }

        // TextEncoder / TextDecoder 原型
        {
            let p = proto(&protos, "TextEncoder");
            let mut b = p.borrow_mut();
            b.set("encode", Value::Native(native_te_encode));
            b.set("encoding", Value::String("utf-8".to_string()));
        }
        proto(&protos, "TextDecoder")
            .borrow_mut()
            .set("decode", Value::Native(native_td_decode));

        // Blob 原型
        {
            let p = proto(&protos, "Blob");
            let mut b = p.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("slice", native_blob_slice),
                ("text", native_blob_text),
                ("arrayBuffer", native_blob_array_buffer),
            ];
            for (n, f) in methods {
                b.set(n, Value::Native(*f));
            }
        }

        // FormData 原型（`forEach` 走解释器拦截）
        {
            let p = proto(&protos, "FormData");
            let mut b = p.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("append", native_fd_append),
                ("delete", native_fd_delete),
                ("get", native_fd_get),
                ("getAll", native_fd_get_all),
                ("has", native_fd_has),
                ("set", native_fd_set),
            ];
            for (n, f) in methods {
                b.set(n, Value::Native(*f));
            }
        }

        // Storage 单例
        for name in ["localStorage", "sessionStorage"] {
            let obj = tagged_object(&proto(&protos, "Storage"), "Storage");
            let inner = Rc::new(RefCell::new(JsObject::with_proto(Some(
                self.protos.object.clone(),
            ))));
            obj.borrow_mut().set("__s_data", Value::Object(inner));
            self.define_global(name, Value::Object(obj));
        }

        // 全局函数
        self.define_global("atob", Value::Native(native_atob));
        self.define_global("btoa", Value::Native(native_btoa));
    }

    /// 按构造器名取其 `.prototype` 建标签实例。
    fn web_instance(&mut self, ctor_name: &str, tag: &str) -> Result<ObjectRef, FlowError> {
        let ctor = self
            .web_ctors
            .get(ctor_name)
            .cloned()
            .ok_or_else(|| type_err("internal error: web ctor missing"))?;
        let proto = match ctor.borrow().get("prototype") {
            Some(Value::Object(p)) => p,
            _ => self.protos.object.clone(),
        };
        Ok(tagged_object(&proto, tag))
    }

    // ---------------- 宿主绑定 ----------------

    /// 绑定 WebSocket 宿主（`engine/` 侧提供真实网络；不绑定则连接失败走
    /// `error` + `close` 事件）。
    pub fn bind_websocket_host(&mut self, host: WsHostRef) {
        self.ws_host = Some(host);
    }

    /// 给 `localStorage` 绑定持久化文件（加载已有内容；此后每次变更写回）。
    pub fn set_localstorage_path(&mut self, path: &str) {
        let obj = match self.global_lookup("localStorage") {
            Some(Value::Object(o)) => o,
            _ => return,
        };
        obj.borrow_mut()
            .set("__s_persist", Value::String(path.to_string()));
        let inner = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let Some((k, v)) = line.split_once('\t') else {
                    continue;
                };
                let (Ok(k), Ok(v)) = (pct_decode(k), pct_decode(v)) else {
                    continue;
                };
                inner.borrow_mut().set(&k, Value::String(v));
            }
        }
        obj.borrow_mut().set("__s_data", Value::Object(inner));
    }

    /// 宿主网络层有数据时调用：为所有存活连接各入队一个 `WsPoll` 任务。
    pub fn pump_websockets(&mut self) {
        let objs: Vec<ObjectRef> = self.ws_conns.values().map(|l| l.obj.clone()).collect();
        for obj in objs {
            self.enqueue_internal(TaskKind::WsPoll { obj });
        }
    }

    // ---------------- 内部任务分发 ----------------

    /// `drain_tasks` 的内部任务入口（`interpreter.rs` 的 `internal_task` 转调）。
    pub(crate) fn webapi_task(&mut self, kind: TaskKind) -> Result<(), FlowError> {
        match kind {
            TaskKind::WsOpened { obj } => self.ws_opened(&obj),
            TaskKind::WsOpenFail { obj, message } => self.ws_open_fail(&obj, &message),
            TaskKind::WsPoll { obj } => self.ws_poll(&obj),
            TaskKind::WsError { obj, message } => {
                self.ws_fire(&obj, "error", vec![("message", Value::String(message))]);
                Ok(())
            }
            TaskKind::WorkerDeliver { obj, data } => self.worker_deliver(&obj, data),
            TaskKind::WorkerInboxDrain { obj } => self.worker_drain(&obj),
            _ => Ok(()),
        }
    }

    /// 构造 `{type, target, ...extra}` 事件对象。
    fn make_event(
        &mut self,
        target: &ObjectRef,
        type_: &str,
        extra: Vec<(&str, Value)>,
    ) -> Value {
        let o = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        {
            let mut b = o.borrow_mut();
            b.set("type", Value::String(type_.to_string()));
            b.set("target", Value::Object(target.clone()));
            for (k, v) in extra {
                b.set(k, v);
            }
        }
        Value::Object(o)
    }

    // ---------------- WebSocket ----------------

    pub(crate) fn ws_construct(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let url_s = match args.first() {
            Some(v) => v.to_js_string(),
            None => {
                return Err(type_err(
                    "WebSocket constructor requires 1 argument",
                ))
            }
        };
        let u = parse_url(&url_s, None)
            .map_err(|e| type_err(format!("Invalid WebSocket URL: {e}")))?;
        if u.protocol != "ws:" && u.protocol != "wss:" {
            return Err(type_err(
                "WebSocket URL's scheme must be either 'ws' or 'wss'",
            ));
        }
        let protocols: Vec<String> = match args.get(1) {
            None | Some(Value::Undefined) | Some(Value::Null) => vec![],
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => {
                a.borrow().elems.iter().map(|v| v.to_js_string()).collect()
            }
            _ => vec![],
        };
        let obj = self.web_instance("WebSocket", "WebSocket")?;
        {
            let mut b = obj.borrow_mut();
            b.set("url", Value::String(u.href()));
            b.set("readyState", Value::Number(0.0));
            b.set("protocol", Value::String(String::new()));
            b.set("extensions", Value::String(String::new()));
            b.set("binaryType", Value::String("blob".to_string()));
            b.set("bufferedAmount", Value::Number(0.0));
            b.set("onopen", Value::Null);
            b.set("onmessage", Value::Null);
            b.set("onerror", Value::Null);
            b.set("onclose", Value::Null);
        }
        let id = Rc::as_ptr(&obj) as usize;
        match self.ws_host.clone() {
            Some(host) => match host.connect(&u.href(), &protocols) {
                Ok(conn) => {
                    self.ws_conns.insert(
                        id,
                        WsLive {
                            obj: obj.clone(),
                            conn,
                        },
                    );
                    self.enqueue_internal(TaskKind::WsOpened { obj: obj.clone() });
                }
                Err(e) => {
                    self.enqueue_internal(TaskKind::WsOpenFail {
                        obj: obj.clone(),
                        message: e,
                    });
                }
            },
            None => {
                self.enqueue_internal(TaskKind::WsOpenFail {
                    obj: obj.clone(),
                    message:
                        "WebSocket not implemented: no host bound (call bind_websocket_host)"
                            .to_string(),
                });
            }
        }
        Ok(Value::Object(obj))
    }

    /// `ws.send` / `ws.close`（需任务队列 → 走解释器拦截）。
    pub(crate) fn ws_method(
        &mut self,
        o: &ObjectRef,
        key: &str,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        match key {
            "send" => self.ws_send(o, args),
            "close" => self.ws_close(o),
            _ => Err(type_err(format!("ws.{key} is not a function"))),
        }
    }

    fn ws_ready_state(o: &ObjectRef) -> u8 {
        o.borrow()
            .get("readyState")
            .map(|v| v.to_number() as u8)
            .unwrap_or(3)
    }

    fn ws_send(&mut self, o: &ObjectRef, args: Vec<Value>) -> Result<Value, FlowError> {
        match Self::ws_ready_state(o) {
            0 => {
                return Err(type_err(
                    "InvalidStateError: WebSocket is still in CONNECTING state",
                ))
            }
            1 => {}
            // CLOSING / CLOSED：静默忽略（规范行为）。
            _ => return Ok(Value::Undefined),
        }
        let id = Rc::as_ptr(o) as usize;
        let conn = match self.ws_conns.get(&id) {
            Some(live) => live.conn.clone(),
            None => return Ok(Value::Undefined),
        };
        let data = args.first().cloned().unwrap_or(Value::Undefined);
        let res = match data {
            Value::String(s) => conn.send_text(&s),
            Value::ArrayBuffer(b) => conn.send_binary(&b.borrow().bytes.borrow()),
            Value::TypedArray(t) => {
                let r = t.borrow();
                let buf = r.buffer.borrow();
                let by = buf.bytes.borrow();
                let end = (r.byte_offset + r.len * r.kind.bytes()).min(by.len());
                conn.send_binary(&by[r.byte_offset..end])
            }
            Value::Object(bo) if bo.borrow().tag.as_deref() == Some("Blob") => {
                match bo.borrow().get("__b_data") {
                    Some(Value::ArrayBuffer(b)) => {
                        conn.send_binary(&b.borrow().bytes.borrow())
                    }
                    _ => conn.send_text(""),
                }
            }
            v => conn.send_text(&v.to_js_string()),
        };
        let obj = o.clone();
        match res {
            Ok(()) => {
                self.enqueue_internal(TaskKind::WsPoll { obj });
            }
            Err(e) => {
                self.enqueue_internal(TaskKind::WsError { obj, message: e });
            }
        }
        Ok(Value::Undefined)
    }

    fn ws_close(&mut self, o: &ObjectRef) -> Result<Value, FlowError> {
        let ready = Self::ws_ready_state(o);
        if ready == 0 || ready == 1 {
            let id = Rc::as_ptr(o) as usize;
            if let Some(live) = self.ws_conns.get(&id) {
                live.conn.close();
            }
            o.borrow_mut().set("readyState", Value::Number(2.0));
            self.enqueue_internal(TaskKind::WsPoll { obj: o.clone() });
        }
        Ok(Value::Undefined)
    }

    fn ws_opened(&mut self, obj: &ObjectRef) -> Result<(), FlowError> {
        let id = Rc::as_ptr(obj) as usize;
        if !self.ws_conns.contains_key(&id) {
            return Ok(());
        }
        obj.borrow_mut().set("readyState", Value::Number(1.0));
        self.ws_fire(obj, "open", vec![]);
        self.enqueue_internal(TaskKind::WsPoll { obj: obj.clone() });
        Ok(())
    }

    fn ws_open_fail(&mut self, obj: &ObjectRef, message: &str) -> Result<(), FlowError> {
        obj.borrow_mut().set("readyState", Value::Number(3.0));
        self.ws_fire(
            obj,
            "error",
            vec![("message", Value::String(message.to_string()))],
        );
        self.ws_fire(
            obj,
            "close",
            vec![
                ("wasClean", Value::Bool(false)),
                ("code", Value::Number(1006.0)),
                ("reason", Value::String(String::new())),
            ],
        );
        Ok(())
    }

    fn ws_poll(&mut self, obj: &ObjectRef) -> Result<(), FlowError> {
        let id = Rc::as_ptr(obj) as usize;
        let conn = match self.ws_conns.get(&id) {
            Some(live) => live.conn.clone(),
            None => return Ok(()),
        };
        for ev in conn.poll() {
            match ev {
                WsEvent::Message(WsMsg::Text(s)) => {
                    self.ws_fire(obj, "message", vec![("data", Value::String(s))]);
                }
                WsEvent::Message(WsMsg::Binary(b)) => {
                    let binary_type = obj
                        .borrow()
                        .get("binaryType")
                        .map(|v| v.to_js_string())
                        .unwrap_or_default();
                    let data = if binary_type == "arraybuffer" {
                        let buf = Rc::new(RefCell::new(JsArrayBuffer {
                            bytes: Rc::new(RefCell::new(b)),
                        }));
                        Value::ArrayBuffer(buf)
                    } else {
                        self.make_blob(b, "")
                    };
                    self.ws_fire(obj, "message", vec![("data", data)]);
                }
                WsEvent::Closed { code, reason } => {
                    obj.borrow_mut().set("readyState", Value::Number(3.0));
                    self.ws_conns.remove(&id);
                    self.ws_fire(
                        obj,
                        "close",
                        vec![
                            ("wasClean", Value::Bool(code == 1000)),
                            ("code", Value::Number(code as f64)),
                            ("reason", Value::String(reason)),
                        ],
                    );
                    return Ok(());
                }
                WsEvent::Error(msg) => {
                    self.ws_fire(obj, "error", vec![("message", Value::String(msg))]);
                }
            }
        }
        Ok(())
    }

    /// 分发 `onopen` / `onmessage` / `onerror` / `onclose`。
    fn ws_fire(&mut self, obj: &ObjectRef, type_: &str, extra: Vec<(&str, Value)>) {
        let prop = match type_ {
            "open" => "onopen",
            "message" => "onmessage",
            "error" => "onerror",
            "close" => "onclose",
            _ => return,
        };
        let handler = obj.borrow().get(prop).unwrap_or(Value::Null);
        if !handler.is_callable() {
            return;
        }
        let event = self.make_event(obj, type_, extra);
        if let Err(e) = self.call_value(handler, Value::Undefined, vec![event]) {
            self.console_push(format!(
                "uncaught error in WebSocket handler: {}",
                crate::interpreter::task_error_msg(&e)
            ));
        }
    }

    // ---------------- Worker ----------------

    pub(crate) fn worker_construct(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let src = match args.first() {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(type_err(
                    "Worker constructor requires a source string",
                ))
            }
        };
        let inbox: InboxRef = Rc::new(RefCell::new(VecDeque::new()));
        let mut wip = Box::new(Interpreter::new());
        wip.post_target = Some(inbox.clone());
        wip.define_global("postMessage", Value::Native(native_worker_post_message));
        let obj = self.web_instance("Worker", "Worker")?;
        {
            let mut b = obj.borrow_mut();
            b.set("onmessage", Value::Null);
            b.set("onerror", Value::Null);
        }
        let mut entry = WorkerEntry {
            interp: wip,
            inbox: inbox.clone(),
            terminated: false,
            error_pending: None,
        };
        // 同步执行 worker 脚本；顶层 `postMessage` 进 inbox，稍后异步投递。
        match crate::parser::parse_source(&src) {
            Ok(prog) => {
                if let Err(e) = entry.interp.run(&prog) {
                    entry.error_pending = Some(match e {
                        FlowError::Runtime(r) => r.to_string(),
                        FlowError::Thrown(v) => {
                            format!("uncaught: {}", v.to_js_string())
                        }
                    });
                }
            }
            Err(e) => {
                entry.error_pending = Some(format!("SyntaxError: {e}"));
            }
        }
        for line in entry.interp.take_console() {
            self.console_push(line);
        }
        let id = Rc::as_ptr(&obj) as usize;
        self.workers.insert(id, Box::new(entry));
        self.enqueue_internal(TaskKind::WorkerInboxDrain { obj: obj.clone() });
        Ok(Value::Object(obj))
    }

    /// `w.postMessage` / `w.terminate`（需任务队列 → 走解释器拦截）。
    pub(crate) fn worker_method(
        &mut self,
        o: &ObjectRef,
        key: &str,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        match key {
            "postMessage" => {
                let id = Rc::as_ptr(o) as usize;
                let terminated =
                    self.workers.get(&id).map(|e| e.terminated).unwrap_or(true);
                if terminated {
                    return Ok(Value::Undefined);
                }
                let data = args.first().cloned().unwrap_or(Value::Undefined);
                self.enqueue_internal(TaskKind::WorkerDeliver {
                    obj: o.clone(),
                    data,
                });
                Ok(Value::Undefined)
            }
            "terminate" => {
                let id = Rc::as_ptr(o) as usize;
                // 条目移除（已排队的内部任务遇到缺失条目会直接跳过）；
                // 附带打破子解释器的引用，防泄漏。
                self.workers.remove(&id);
                Ok(Value::Undefined)
            }
            _ => Err(type_err(format!("worker.{key} is not a function"))),
        }
    }

    /// 父 → Worker：进子解释器调 worker 的 `onmessage`。
    fn worker_deliver(&mut self, obj: &ObjectRef, data: Value) -> Result<(), FlowError> {
        let id = Rc::as_ptr(obj) as usize;
        let handler = match self.workers.get(&id) {
            Some(e) if !e.terminated => e.interp.global_lookup("onmessage"),
            _ => None,
        };
        let Some(handler) = handler else {
            return Ok(());
        };
        if !handler.is_callable() {
            return Ok(());
        }
        let event = self.make_event(obj, "message", vec![("data", data)]);
        let res = match self.workers.get_mut(&id) {
            Some(e) if !e.terminated => {
                e.interp.call_value(handler, Value::Undefined, vec![event])
            }
            _ => return Ok(()),
        };
        // 转发 worker console 到父 console
        let wconsole = match self.workers.get_mut(&id) {
            Some(e) => e.interp.take_console(),
            None => Vec::new(),
        };
        for line in wconsole {
            self.console_push(line);
        }
        if let Err(e) = res {
            self.console_push(format!(
                "uncaught error in Worker: {}",
                crate::interpreter::task_error_msg(&e)
            ));
        }
        // worker 可能在处理中 `postMessage` → 继续排空
        self.enqueue_internal(TaskKind::WorkerInboxDrain { obj: obj.clone() });
        Ok(())
    }

    /// Worker → 父：排空 inbox，分发父的 `onmessage` / `onerror`。
    fn worker_drain(&mut self, obj: &ObjectRef) -> Result<(), FlowError> {
        let id = Rc::as_ptr(obj) as usize;
        // 构造期错误 → 父 error 事件（一次性）
        let err = match self.workers.get_mut(&id) {
            Some(e) => e.error_pending.take(),
            None => return Ok(()),
        };
        if let Some(msg) = err {
            self.worker_fire(obj, "error", vec![("message", Value::String(msg))]);
        }
        loop {
            let next = match self.workers.get(&id) {
                Some(e) if !e.terminated => e.inbox.borrow_mut().pop_front(),
                _ => None,
            };
            let Some(data) = next else { break };
            self.worker_fire(obj, "message", vec![("data", data)]);
        }
        Ok(())
    }

    fn worker_fire(&mut self, obj: &ObjectRef, type_: &str, extra: Vec<(&str, Value)>) {
        let prop = if type_ == "message" {
            "onmessage"
        } else {
            "onerror"
        };
        let handler = obj.borrow().get(prop).unwrap_or(Value::Null);
        if !handler.is_callable() {
            return;
        }
        let event = self.make_event(obj, type_, extra);
        if let Err(e) = self.call_value(handler, Value::Undefined, vec![event]) {
            self.console_push(format!(
                "uncaught error in Worker handler: {}",
                crate::interpreter::task_error_msg(&e)
            ));
        }
    }

    // ---------------- Storage ----------------

    /// `bind` 给 `set_localstorage_path` 用的内部对象。
    fn storage_inner(&self, o: &ObjectRef) -> Option<ObjectRef> {
        match o.borrow().get("__s_data") {
            Some(Value::Object(inner)) => Some(inner),
            _ => None,
        }
    }

    /// `localStorage.length` / 方法 / 具名存储项。
    pub(crate) fn storage_get_prop(
        &mut self,
        o: &ObjectRef,
        key: &str,
    ) -> Result<Value, FlowError> {
        if key == "length" {
            let n = self
                .storage_inner(o)
                .map(|i| i.borrow().keys().len())
                .unwrap_or(0);
            return Ok(Value::Number(n as f64));
        }
        // 自有属性 → 原型链（方法在 Storage 原型上）→ 具名存储项。
        if let Some(v) = o.borrow().get_in_chain(key) {
            return Ok(v);
        }
        if let Some(inner) = self.storage_inner(o) {
            if let Some(v) = inner.borrow().get(key) {
                return Ok(v);
            }
        }
        Ok(Value::Null)
    }

    /// 具名写走 `setItem` 语义（值转字符串）；`length` 忽略；已有自有属性
    /// （方法/`__` 内部属性）走普通覆盖。
    pub(crate) fn storage_set_prop(
        &mut self,
        o: &ObjectRef,
        key: &str,
        val: Value,
    ) -> Result<(), FlowError> {
        if key == "length" {
            return Ok(());
        }
        // `__s_` 为内部命名空间：忽略写（防误伤 `__s_data` / `__s_persist`）。
        if key.starts_with("__s_") {
            return Ok(());
        }
        if o.borrow().has_own(key) {
            o.borrow_mut().set(key, val);
            return Ok(());
        }
        let s = val.to_js_string();
        if let Some(inner) = self.storage_inner(o) {
            inner.borrow_mut().set(key, Value::String(s));
            self.storage_persist(o);
        }
        Ok(())
    }

    /// 写回持久化文件（`__s_persist` 未设置则跳过；写失败静默忽略）。
    fn storage_persist(&self, o: &ObjectRef) {
        let path = match o.borrow().get("__s_persist") {
            Some(Value::String(p)) => p,
            _ => return,
        };
        let Some(inner) = self.storage_inner(o) else {
            return;
        };
        let b = inner.borrow();
        let mut out = String::new();
        for k in b.keys() {
            if let Some(Value::String(v)) = b.get(&k) {
                out.push_str(&pct_encode(&k));
                out.push('\t');
                out.push_str(&pct_encode(&v));
                out.push('\n');
            }
        }
        let _ = std::fs::write(&path, out);
    }

    // ---------------- URL ----------------

    pub(crate) fn url_construct(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let input = args.first().map(|v| v.to_js_string()).unwrap_or_default();
        let base: Option<ParsedUrl> = match args.get(1) {
            None | Some(Value::Undefined) | Some(Value::Null) => None,
            Some(Value::Object(o)) if o.borrow().tag.as_deref() == Some("URL") => {
                Some(self.url_to_parsed(o)?)
            }
            Some(v) => {
                let bs = v.to_js_string();
                Some(parse_url(&bs, None).map_err(type_err)?)
            }
        };
        let u = parse_url(&input, base.as_ref()).map_err(type_err)?;
        let obj = self.web_instance("URL", "URL")?;
        {
            let mut b = obj.borrow_mut();
            b.set("href", Value::String(u.href()));
            b.set("protocol", Value::String(u.protocol.clone()));
            b.set("username", Value::String(u.username.clone()));
            b.set("password", Value::String(u.password.clone()));
            b.set("host", Value::String(u.host()));
            b.set("hostname", Value::String(u.hostname.clone()));
            b.set("port", Value::String(u.port.clone()));
            b.set("pathname", Value::String(u.pathname.clone()));
            b.set("search", Value::String(u.search.clone()));
            b.set("hash", Value::String(u.hash.clone()));
            b.set("origin", Value::String(u.origin()));
        }
        let usp = self.usp_from_query(&u.search)?;
        obj.borrow_mut().set("searchParams", Value::Object(usp));
        Ok(Value::Object(obj))
    }

    /// 从 URL 对象的自有属性还原 `ParsedUrl`（`new URL(x, urlObj)` 用）。
    fn url_to_parsed(&self, o: &ObjectRef) -> Result<ParsedUrl, FlowError> {
        let b = o.borrow();
        let g = |k: &str| b.get(k).map(|v| v.to_js_string()).unwrap_or_default();
        Ok(ParsedUrl {
            protocol: g("protocol"),
            username: g("username"),
            password: g("password"),
            hostname: g("hostname"),
            port: g("port"),
            pathname: g("pathname"),
            search: g("search"),
            hash: g("hash"),
        })
    }

    /// 由 query 字符串建 URLSearchParams 对象。
    fn usp_from_query(&mut self, search: &str) -> Result<ObjectRef, FlowError> {
        let obj = self.web_instance("URLSearchParams", "USP")?;
        let q = search.strip_prefix('?').unwrap_or(search);
        let mut pairs = Vec::new();
        for part in q.split('&') {
            if part.is_empty() {
                continue;
            }
            match part.split_once('=') {
                Some((k, v)) => pairs.push((form_urldecode(k), form_urldecode(v))),
                None => pairs.push((form_urldecode(part), String::new())),
            }
        }
        usp_pairs_set(&obj, &self.protos, &pairs);
        Ok(obj)
    }

    pub(crate) fn usp_construct(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let obj = self.web_instance("URLSearchParams", "USP")?;
        let mut pairs: Vec<(String, String)> = Vec::new();
        match args.first() {
            None | Some(Value::Undefined) | Some(Value::Null) => {}
            Some(Value::Object(o)) if o.borrow().tag.as_deref() == Some("USP") => {
                pairs = usp_pairs_of(o);
            }
            Some(Value::String(s)) => {
                let q = s.strip_prefix('?').unwrap_or(s);
                for part in q.split('&') {
                    if part.is_empty() {
                        continue;
                    }
                    match part.split_once('=') {
                        Some((k, v)) => pairs.push((form_urldecode(k), form_urldecode(v))),
                        None => pairs.push((form_urldecode(part), String::new())),
                    }
                }
            }
            Some(Value::Array(arr)) => {
                for el in arr.borrow().elems.iter() {
                    if let Value::Array(p) = el {
                        let e = p.borrow();
                        if e.elems.len() >= 2 {
                            pairs.push((
                                e.elems[0].to_js_string(),
                                e.elems[1].to_js_string(),
                            ));
                        }
                    }
                }
            }
            Some(Value::Object(o)) => {
                for k in o.borrow().keys() {
                    let v = o.borrow().get(&k).unwrap_or(Value::Undefined);
                    pairs.push((k, v.to_js_string()));
                }
            }
            _ => {}
        }
        usp_pairs_set(&obj, &self.protos, &pairs);
        Ok(Value::Object(obj))
    }

    /// `usp.forEach(cb)`（回调需解释器 → 走拦截）。
    pub(crate) fn usp_for_each(
        &mut self,
        o: &ObjectRef,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let cb = args.first().cloned().unwrap_or(Value::Undefined);
        if !cb.is_callable() {
            return Err(type_err("forEach callback must be callable"));
        }
        for (k, v) in usp_pairs_of(o) {
            self.call_value(
                cb.clone(),
                Value::Undefined,
                vec![
                    Value::String(v),
                    Value::String(k),
                    Value::Object(o.clone()),
                ],
            )?;
        }
        Ok(Value::Undefined)
    }

    // ---------------- TextEncoder / TextDecoder ----------------

    pub(crate) fn te_construct(&mut self, _args: Vec<Value>) -> Result<Value, FlowError> {
        let obj = self.web_instance("TextEncoder", "TextEncoder")?;
        obj.borrow_mut()
            .set("encoding", Value::String("utf-8".to_string()));
        Ok(Value::Object(obj))
    }

    pub(crate) fn td_construct(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let label = args
            .first()
            .map(|v| v.to_js_string().to_lowercase())
            .unwrap_or_else(|| "utf-8".to_string());
        let label = label.replace('_', "-");
        if label != "utf-8" && label != "utf8" {
            return Err(crate::value::range_err(format!(
                "unsupported encoding: {label}"
            )));
        }
        let (fatal, ignore_bom) = match args.get(1) {
            Some(Value::Object(o)) => {
                let g = |k: &str| {
                    o.borrow()
                        .get(k)
                        .map(|v| v.to_boolean())
                        .unwrap_or(false)
                };
                (g("fatal"), g("ignoreBOM"))
            }
            _ => (false, false),
        };
        let obj = self.web_instance("TextDecoder", "TextDecoder")?;
        {
            let mut b = obj.borrow_mut();
            b.set("encoding", Value::String("utf-8".to_string()));
            b.set("fatal", Value::Bool(fatal));
            b.set("ignoreBOM", Value::Bool(ignore_bom));
        }
        Ok(Value::Object(obj))
    }

    // ---------------- Blob ----------------

    pub(crate) fn blob_construct(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let mut bytes: Vec<u8> = Vec::new();
        if let Some(parts) = args.first() {
            let elems: Vec<Value> = match parts {
                Value::Array(a) => a.borrow().elems.clone(),
                _ => return Err(type_err("Blob parts must be an array")),
            };
            for p in elems {
                match p {
                    Value::String(s) => bytes.extend_from_slice(s.as_bytes()),
                    Value::ArrayBuffer(b) => {
                        bytes.extend_from_slice(&b.borrow().bytes.borrow())
                    }
                    Value::TypedArray(t) => {
                        let r = t.borrow();
                        let buf = r.buffer.borrow();
                        let by = buf.bytes.borrow();
                        let end =
                            (r.byte_offset + r.len * r.kind.bytes()).min(by.len());
                        bytes.extend_from_slice(&by[r.byte_offset..end]);
                    }
                    Value::Object(o)
                        if o.borrow().tag.as_deref() == Some("Blob") =>
                    {
                        if let Some(Value::ArrayBuffer(b)) = o.borrow().get("__b_data") {
                            bytes.extend_from_slice(&b.borrow().bytes.borrow());
                        }
                    }
                    v => bytes.extend_from_slice(v.to_js_string().as_bytes()),
                }
            }
        }
        let mime = match args.get(1) {
            Some(Value::Object(o)) => o
                .borrow()
                .get("type")
                .map(|v| v.to_js_string().to_lowercase())
                .unwrap_or_default(),
            _ => String::new(),
        };
        Ok(self.make_blob(bytes, &mime))
    }

    /// 由字节建 Blob 对象（`slice` / WebSocket 二进制消息共用）。
    fn make_blob(&mut self, bytes: Vec<u8>, mime: &str) -> Value {
        let buf = Rc::new(RefCell::new(JsArrayBuffer {
            bytes: Rc::new(RefCell::new(bytes)),
        }));
        let size = buf.borrow().bytes.borrow().len();
        let obj = self
            .web_instance("Blob", "Blob")
            .unwrap_or_else(|_| Rc::new(RefCell::new(JsObject::new())));
        {
            let mut b = obj.borrow_mut();
            b.set("__b_data", Value::ArrayBuffer(buf));
            b.set("__b_type", Value::String(mime.to_string()));
            b.set("size", Value::Number(size as f64));
            b.set("type", Value::String(mime.to_string()));
        }
        Value::Object(obj)
    }

    // ---------------- FormData ----------------

    pub(crate) fn fd_construct(&mut self, _args: Vec<Value>) -> Result<Value, FlowError> {
        let obj = self.web_instance("FormData", "FormData")?;
        let arr = Rc::new(RefCell::new(crate::value::JsArray::new(vec![])));
        arr.borrow_mut().proto = Some(self.protos.array.clone());
        obj.borrow_mut().set("__f_entries", Value::Array(arr));
        Ok(Value::Object(obj))
    }

    /// `fd.forEach(cb)`（回调需解释器 → 走拦截）。
    pub(crate) fn formdata_for_each(
        &mut self,
        o: &ObjectRef,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let cb = args.first().cloned().unwrap_or(Value::Undefined);
        if !cb.is_callable() {
            return Err(type_err("forEach callback must be callable"));
        }
        for (k, v) in fd_entries_of(o) {
            self.call_value(
                cb.clone(),
                Value::Undefined,
                vec![v, Value::String(k), Value::Object(o.clone())],
            )?;
        }
        Ok(Value::Undefined)
    }
}

// ---------------------------------------------------------------------------
// 7. Native 函数（纯方法，经 `ctx.this` 读标签对象的 `__` 状态）
// ---------------------------------------------------------------------------

/// `ctx.this` 必须是对应标签的对象，否则 TypeError。
fn check_tag(ctx: &NativeCtx, tag: &str) -> Result<ObjectRef, FlowError> {
    let Value::Object(o) = &ctx.this else {
        return Err(type_err("called on incompatible receiver"));
    };
    if o.borrow().tag.as_deref() != Some(tag) {
        return Err(type_err("called on incompatible receiver"));
    }
    Ok(o.clone())
}

fn native_atob(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let _ = ctx;
    let s = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let bytes = base64_decode(&s).map_err(type_err)?;
    Ok(Value::String(bytes.iter().map(|&b| b as char).collect()))
}

fn native_btoa(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let _ = ctx;
    let s = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    if s.chars().any(|c| (c as u32) > 0xFF) {
        return Err(type_err(
            "InvalidCharacterError: btoa input must be Latin-1",
        ));
    }
    let bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
    Ok(Value::String(base64_encode(&bytes)))
}

// ---- Storage ----

fn storage_inner_of(ctx: &NativeCtx) -> Result<ObjectRef, FlowError> {
    let o = check_tag(ctx, "Storage")?;
    match o.borrow().get("__s_data") {
        Some(Value::Object(inner)) => Ok(inner),
        _ => Err(type_err("called on incompatible receiver")),
    }
}

fn storage_persist_of(ctx: &NativeCtx) {
    let Ok(o) = check_tag(ctx, "Storage") else {
        return;
    };
    let path = match o.borrow().get("__s_persist") {
        Some(Value::String(p)) => p,
        _ => return,
    };
    let Ok(inner) = storage_inner_of(ctx) else {
        return;
    };
    let b = inner.borrow();
    let mut out = String::new();
    for k in b.keys() {
        if let Some(Value::String(v)) = b.get(&k) {
            out.push_str(&pct_encode(&k));
            out.push('\t');
            out.push_str(&pct_encode(&v));
            out.push('\n');
        }
    }
    let _ = std::fs::write(&path, out);
}

fn native_storage_get_item(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let key = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let inner = storage_inner_of(ctx)?;
    Ok(inner.borrow().get(&key).unwrap_or(Value::Null))
}

fn native_storage_set_item(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let key = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let val = args.get(1).map(|v| v.to_js_string()).unwrap_or_default();
    let inner = storage_inner_of(ctx)?;
    inner.borrow_mut().set(&key, Value::String(val));
    storage_persist_of(ctx);
    Ok(Value::Undefined)
}

fn native_storage_remove_item(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let key = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let inner = storage_inner_of(ctx)?;
    inner.borrow_mut().delete(&key);
    storage_persist_of(ctx);
    Ok(Value::Undefined)
}

fn native_storage_clear(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let inner = storage_inner_of(ctx)?;
    let keys: Vec<String> = inner.borrow().keys();
    {
        let mut b = inner.borrow_mut();
        for k in keys {
            b.delete(&k);
        }
    }
    storage_persist_of(ctx);
    Ok(Value::Undefined)
}

fn native_storage_key(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let i = args.first().map(|v| v.to_number()).unwrap_or(-1.0);
    let inner = storage_inner_of(ctx)?;
    let keys = inner.borrow().keys();
    if i < 0.0 || i >= keys.len() as f64 {
        return Ok(Value::Null);
    }
    Ok(Value::String(keys[i as usize].clone()))
}

// ---- URL ----

fn native_url_to_string(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let o = check_tag(ctx, "URL")?;
    Ok(o
        .borrow()
        .get("href")
        .unwrap_or(Value::String(String::new())))
}

// ---- URLSearchParams ----

fn usp_this(ctx: &NativeCtx) -> Result<ObjectRef, FlowError> {
    check_tag(ctx, "USP")
}

fn native_usp_append(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let val = args.get(1).map(|v| v.to_js_string()).unwrap_or_default();
    let mut pairs = usp_pairs_of(&o);
    pairs.push((name, val));
    usp_pairs_set(&o, &ctx.protos, &pairs);
    Ok(Value::Undefined)
}

fn native_usp_delete(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let pairs: Vec<(String, String)> =
        usp_pairs_of(&o).into_iter().filter(|(k, _)| k != &name).collect();
    usp_pairs_set(&o, &ctx.protos, &pairs);
    Ok(Value::Undefined)
}

fn native_usp_get(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    Ok(usp_pairs_of(&o)
        .into_iter()
        .find(|(k, _)| k == &name)
        .map(|(_, v)| Value::String(v))
        .unwrap_or(Value::Null))
}

fn native_usp_get_all(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let vals: Vec<Value> = usp_pairs_of(&o)
        .into_iter()
        .filter(|(k, _)| k == &name)
        .map(|(_, v)| Value::String(v))
        .collect();
    Ok(js_array(ctx, vals))
}

fn native_usp_has(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    Ok(Value::Bool(
        usp_pairs_of(&o).iter().any(|(k, _)| k == &name),
    ))
}

fn native_usp_set(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let val = args.get(1).map(|v| v.to_js_string()).unwrap_or_default();
    let mut pairs = usp_pairs_of(&o);
    let mut found = false;
    pairs.retain(|(k, _)| {
        if k == &name {
            if found {
                return false;
            }
            found = true;
        }
        true
    });
    if found {
        for (k, v) in pairs.iter_mut() {
            if k == &name {
                *v = val.clone();
                break;
            }
        }
    } else {
        pairs.push((name, val));
    }
    usp_pairs_set(&o, &ctx.protos, &pairs);
    Ok(Value::Undefined)
}

fn native_usp_sort(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    let mut pairs = usp_pairs_of(&o);
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    usp_pairs_set(&o, &ctx.protos, &pairs);
    Ok(Value::Undefined)
}

fn native_usp_to_string(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    let s = usp_pairs_of(&o)
        .iter()
        .map(|(k, v)| format!("{}={}", form_urlencode(k), form_urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(Value::String(s))
}

fn native_usp_keys(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    Ok(js_array(
        ctx,
        usp_pairs_of(&o)
            .into_iter()
            .map(|(k, _)| Value::String(k))
            .collect(),
    ))
}

fn native_usp_values(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    Ok(js_array(
        ctx,
        usp_pairs_of(&o)
            .into_iter()
            .map(|(_, v)| Value::String(v))
            .collect(),
    ))
}

fn native_usp_entries(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let o = usp_this(ctx)?;
    let mut elems = Vec::new();
    for (k, v) in usp_pairs_of(&o) {
        let pair = Rc::new(RefCell::new(crate::value::JsArray::new(vec![
            Value::String(k),
            Value::String(v),
        ])));
        pair.borrow_mut().proto = Some(ctx.protos.array.clone());
        elems.push(Value::Array(pair));
    }
    Ok(js_array(ctx, elems))
}

// ---- TextEncoder / TextDecoder ----

fn native_te_encode(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    check_tag(ctx, "TextEncoder")?;
    let s = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let bytes = s.as_bytes();
    let ta = ta_new(TypedKind::Uint8, bytes.len());
    ta.borrow().buffer.borrow().bytes.borrow_mut().copy_from_slice(bytes);
    Ok(Value::TypedArray(ta))
}

fn native_td_decode(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = check_tag(ctx, "TextDecoder")?;
    let (fatal, ignore_bom) = {
        let b = o.borrow();
        let g = |k: &str| b.get(k).map(|v| v.to_boolean()).unwrap_or(false);
        (g("fatal"), g("ignoreBOM"))
    };
    let bytes: Vec<u8> = match args.first() {
        None | Some(Value::Undefined) => Vec::new(),
        Some(Value::TypedArray(t)) => {
            let r = t.borrow();
            let buf = r.buffer.borrow();
            let by = buf.bytes.borrow();
            let end = (r.byte_offset + r.len * r.kind.bytes()).min(by.len());
            by[r.byte_offset..end].to_vec()
        }
        Some(Value::ArrayBuffer(b)) => b.borrow().bytes.borrow().clone(),
        _ => {
            return Err(type_err(
                "TextDecoder.decode input must be a BufferSource",
            ))
        }
    };
    let s = if fatal {
        String::from_utf8(bytes.clone()).map_err(|_| {
            type_err("TextDecoder: invalid UTF-8 sequence (fatal)")
        })?
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let s = if ignore_bom {
        s
    } else {
        s.strip_prefix('\u{FEFF}').unwrap_or(&s).to_string()
    };
    Ok(Value::String(s))
}

// ---- Blob ----

fn ctx_blob_bytes(ctx: &NativeCtx) -> Result<Vec<u8>, FlowError> {
    let o = check_tag(ctx, "Blob")?;
    match o.borrow().get("__b_data") {
        Some(Value::ArrayBuffer(b)) => Ok(b.borrow().bytes.borrow().clone()),
        _ => Err(type_err("called on incompatible receiver")),
    }
}

fn native_blob_slice(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = check_tag(ctx, "Blob")?;
    let bytes = ctx_blob_bytes(ctx)?;
    let len = bytes.len() as f64;
    let clamp = |v: f64| -> usize {
        let n = if v < 0.0 {
            (len + v).max(0.0)
        } else {
            v.min(len)
        };
        n as usize
    };
    let start = args.first().map(|v| v.to_number()).map(clamp).unwrap_or(0);
    let end = args
        .get(1)
        .map(|v| v.to_number())
        .map(clamp)
        .unwrap_or(len as usize);
    let (start, end) = (start.min(end), start.max(end));
    let mime = args
        .get(2)
        .map(|v| v.to_js_string().to_lowercase())
        .unwrap_or_default();
    let sliced = bytes[start..end].to_vec();
    let buf = Rc::new(RefCell::new(JsArrayBuffer {
        bytes: Rc::new(RefCell::new(sliced)),
    }));
    let proto = o.borrow().proto.clone().unwrap_or(ctx.protos.object.clone());
    let nb = tagged_object(&proto, "Blob");
    {
        let mut b = nb.borrow_mut();
        b.set("__b_data", Value::ArrayBuffer(buf.clone()));
        b.set("__b_type", Value::String(mime.clone()));
        b.set(
            "size",
            Value::Number(buf.borrow().bytes.borrow().len() as f64),
        );
        b.set("type", Value::String(mime));
    }
    Ok(Value::Object(nb))
}

fn native_blob_text(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let bytes = ctx_blob_bytes(ctx)?;
    let p = JsPromise::pending();
    let s = String::from_utf8_lossy(&bytes).into_owned();
    let _ = settle_promise(&p, false, Value::String(s), &mut ctx.microtasks);
    Ok(Value::Promise(p))
}

fn native_blob_array_buffer(
    ctx: &mut NativeCtx,
    _args: Vec<Value>,
) -> Result<Value, FlowError> {
    let o = check_tag(ctx, "Blob")?;
    let buf = match o.borrow().get("__b_data") {
        Some(Value::ArrayBuffer(b)) => b,
        _ => return Err(type_err("called on incompatible receiver")),
    };
    let p = JsPromise::pending();
    let _ = settle_promise(&p, false, Value::ArrayBuffer(buf), &mut ctx.microtasks);
    Ok(Value::Promise(p))
}

// ---- FormData ----

fn fd_entries_of(obj: &ObjectRef) -> Vec<(String, Value)> {
    let entries_val = obj.borrow().get("__f_entries");
    let Some(Value::Array(arr)) = entries_val else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for el in arr.borrow().elems.iter() {
        if let Value::Array(pair) = el {
            let e = pair.borrow();
            if e.elems.len() >= 2 {
                out.push((e.elems[0].to_js_string(), e.elems[1].clone()));
            }
        }
    }
    out
}

fn fd_entries_set(
    obj: &ObjectRef,
    protos: &BuiltinProtos,
    entries: &[(String, Value)],
) {
    let mut elems = Vec::with_capacity(entries.len());
    for (k, v) in entries {
        let pair = Rc::new(RefCell::new(crate::value::JsArray::new(vec![
            Value::String(k.clone()),
            v.clone(),
        ])));
        pair.borrow_mut().proto = Some(protos.array.clone());
        elems.push(Value::Array(pair));
    }
    let arr = Rc::new(RefCell::new(crate::value::JsArray::new(elems)));
    arr.borrow_mut().proto = Some(protos.array.clone());
    obj.borrow_mut().set("__f_entries", Value::Array(arr));
}

fn fd_this(ctx: &NativeCtx) -> Result<ObjectRef, FlowError> {
    check_tag(ctx, "FormData")
}

fn fd_value_of(v: &Value) -> Value {
    match v {
        // Blob / 文件值原样保留；其余转字符串。
        Value::Object(o) if o.borrow().tag.as_deref() == Some("Blob") => v.clone(),
        _ => Value::String(v.to_js_string()),
    }
}

fn native_fd_append(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = fd_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let val = fd_value_of(&args.get(1).cloned().unwrap_or(Value::Undefined));
    let mut entries = fd_entries_of(&o);
    entries.push((name, val));
    fd_entries_set(&o, &ctx.protos, &entries);
    Ok(Value::Undefined)
}

fn native_fd_delete(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = fd_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let entries: Vec<(String, Value)> = fd_entries_of(&o)
        .into_iter()
        .filter(|(k, _)| k != &name)
        .collect();
    fd_entries_set(&o, &ctx.protos, &entries);
    Ok(Value::Undefined)
}

fn native_fd_get(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = fd_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    Ok(fd_entries_of(&o)
        .into_iter()
        .find(|(k, _)| k == &name)
        .map(|(_, v)| v)
        .unwrap_or(Value::Null))
}

fn native_fd_get_all(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = fd_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    Ok(js_array(
        ctx,
        fd_entries_of(&o)
            .into_iter()
            .filter(|(k, _)| k == &name)
            .map(|(_, v)| v)
            .collect(),
    ))
}

fn native_fd_has(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = fd_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    Ok(Value::Bool(
        fd_entries_of(&o).iter().any(|(k, _)| k == &name),
    ))
}

fn native_fd_set(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let o = fd_this(ctx)?;
    let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let val = fd_value_of(&args.get(1).cloned().unwrap_or(Value::Undefined));
    let mut entries = fd_entries_of(&o);
    let mut found = false;
    entries.retain(|(k, _)| {
        if k == &name {
            if found {
                return false;
            }
            found = true;
        }
        true
    });
    if found {
        for (k, v) in entries.iter_mut() {
            if k == &name {
                *v = val.clone();
                break;
            }
        }
    } else {
        entries.push((name, val));
    }
    fd_entries_set(&o, &ctx.protos, &entries);
    Ok(Value::Undefined)
}

// ---- Worker 子解释器的全局 `postMessage` ----

fn native_worker_post_message(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let msg = args.first().cloned().unwrap_or(Value::Undefined);
    if let Some(inbox) = &ctx.post_target {
        inbox.borrow_mut().push_back(msg);
    }
    Ok(Value::Undefined)
}

// ---------------------------------------------------------------------------
// 8. 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::MockDom;
    use crate::parser::parse_source;

    /// 建解释器（可选 ws host / dom），跑源码，返回 console。
    fn run_ip(
        src: &str,
        ws_host: Option<WsHostRef>,
        with_dom: bool,
    ) -> (Option<String>, Vec<String>) {
        let prog = parse_source(src).expect("parse test source");
        let mut ip = Interpreter::new();
        if let Some(h) = ws_host {
            ip.bind_websocket_host(h);
        }
        if with_dom {
            ip.bind_dom(Rc::new(MockDom::new()));
        }
        let r = match ip.run(&prog) {
            Ok(v) => Some(v.to_js_string()),
            Err(e) => Some(format!("ERROR: {e:?}")),
        };
        (r, ip.take_console())
    }

    /// 树模式 + VM 模式双跑，console 必须一致（纯 API 用）。
    fn evc(src: &str) -> Vec<String> {
        let (_, c1) = run_ip(src, None, false);
        let (_, c2) = crate::vm::eval_with_console_vm(src)
            .unwrap_or_else(|e| panic!("vm failed for {src:?}: {e:?}"));
        assert_eq!(c1, c2, "tree/vm console mismatch for {src:?}");
        c1
    }

    // ---- 回环 WebSocket 宿主（测试用） ----

    #[derive(Default)]
    struct LoopbackConn {
        inbox: RefCell<Vec<WsEvent>>,
        open: std::cell::Cell<bool>,
    }

    impl WsConn for LoopbackConn {
        fn send_text(&self, data: &str) -> Result<(), String> {
            if !self.open.get() {
                return Err("connection is not open".to_string());
            }
            self.inbox
                .borrow_mut()
                .push(WsEvent::Message(WsMsg::Text(format!("echo:{data}"))));
            Ok(())
        }
        fn send_binary(&self, data: &[u8]) -> Result<(), String> {
            if !self.open.get() {
                return Err("connection is not open".to_string());
            }
            self.inbox
                .borrow_mut()
                .push(WsEvent::Message(WsMsg::Binary(data.to_vec())));
            Ok(())
        }
        fn poll(&self) -> Vec<WsEvent> {
            std::mem::take(&mut *self.inbox.borrow_mut())
        }
        fn close(&self) {
            self.open.set(false);
            self.inbox.borrow_mut().push(WsEvent::Closed {
                code: 1000,
                reason: "bye".to_string(),
            });
        }
        fn is_open(&self) -> bool {
            self.open.get()
        }
    }

    struct LoopbackWsHost;
    impl WsHost for LoopbackWsHost {
        fn connect(&self, url: &str, _protocols: &[String]) -> Result<WsConnRef, String> {
            assert!(
                url.starts_with("ws://") || url.starts_with("wss://"),
                "bad url {url}"
            );
            let c = LoopbackConn::default();
            c.open.set(true);
            Ok(Rc::new(c))
        }
    }

    fn loopback() -> WsHostRef {
        Rc::new(LoopbackWsHost)
    }

    // ---- 可手动投递的宿主（pump_websockets 测试用） ----

    #[derive(Default)]
    struct ManualConn {
        inbox: RefCell<Vec<WsEvent>>,
    }
    impl WsConn for ManualConn {
        fn send_text(&self, _data: &str) -> Result<(), String> {
            Ok(())
        }
        fn send_binary(&self, _data: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn poll(&self) -> Vec<WsEvent> {
            std::mem::take(&mut *self.inbox.borrow_mut())
        }
        fn close(&self) {}
        fn is_open(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct ManualHost {
        conns: RefCell<Vec<Rc<ManualConn>>>,
    }
    impl ManualHost {
        fn push_text(&self, s: &str) {
            for c in self.conns.borrow().iter() {
                c.inbox
                    .borrow_mut()
                    .push(WsEvent::Message(WsMsg::Text(s.to_string())));
            }
        }
    }
    impl WsHost for ManualHost {
        fn connect(&self, _url: &str, _protocols: &[String]) -> Result<WsConnRef, String> {
            let c = Rc::new(ManualConn::default());
            self.conns.borrow_mut().push(c.clone());
            Ok(c)
        }
    }

    // ================= atob / btoa =================

    #[test]
    fn p13_atob_btoa() {
        let c = evc(
            r#"
            console.log(btoa("hello"));
            console.log(atob("aGVsbG8="));
            console.log(atob(btoa("ÿ")));
            "#,
        );
        assert_eq!(c, vec!["aGVsbG8=", "hello", "ÿ"]);
        // 非 Latin-1 / 非法 base64 → 运行期错误（与 JSON.parse 等 builtin
        // 一致：fail-fast，不可被 try/catch 捕获）。
        let (r, _) = run_ip(r#"btoa("中文");"#, None, false);
        assert!(r.unwrap().contains("InvalidCharacterError"), "btoa");
        let (r, _) = run_ip(r#"atob("!!!");"#, None, false);
        assert!(r.unwrap().contains("InvalidCharacterError"), "atob");
    }

    // ================= URL =================

    #[test]
    fn p13_url_parse() {
        let c = evc(
            r#"
            const u = new URL("https://user:pass@example.com:8080/p/a/t?q=1&r=2#frag");
            console.log(u.protocol);
            console.log(u.username);
            console.log(u.password);
            console.log(u.host);
            console.log(u.hostname);
            console.log(u.port);
            console.log(u.pathname);
            console.log(u.search);
            console.log(u.hash);
            console.log(u.origin);
            console.log(u.href);
            console.log(u.toString());
            console.log(u.searchParams.get("q"));
            console.log(u instanceof URL);
            "#,
        );
        assert_eq!(
            c,
            vec![
                "https:",
                "user",
                "pass",
                "example.com:8080",
                "example.com",
                "8080",
                "/p/a/t",
                "?q=1&r=2",
                "#frag",
                "https://example.com:8080",
                "https://user:pass@example.com:8080/p/a/t?q=1&r=2#frag",
                "https://user:pass@example.com:8080/p/a/t?q=1&r=2#frag",
                "1",
                "true",
            ]
        );
    }

    #[test]
    fn p13_url_tostring_note() {
        // 说明：`String(urlObj)` 走引擎既有简化（普通对象 → "[object Object]"），
        // 与 Intl formatter 等一致；取 href 请用 `url.href` / `url.toString()`。
        let c = evc(r#"console.log(String(new URL("https://a.com/")));"#);
        assert_eq!(c, vec!["[object Object]"]);
    }

    #[test]
    fn p13_url_relative() {
        let c = evc(
            r#"
            console.log(new URL("/x?y=1", "https://a.com/base/").href);
            console.log(new URL("../up", "https://a.com/a/b/c").href);
            console.log(new URL("https://b.com/z", "https://a.com/").href);
            console.log(new URL("page", "https://a.com/dir/").href);
            "#,
        );
        assert_eq!(
            c,
            vec![
                "https://a.com/x?y=1",
                "https://a.com/a/up",
                "https://b.com/z",
                "https://a.com/dir/page",
            ]
        );
    }

    #[test]
    fn p13_url_invalid() {
        // 非法 URL → 运行期错误（fail-fast，与其他构造器一致）
        for src in [
            r#"new URL(":::");"#,
            r#"new URL("/rel");"#,
            r#"new URL();"#,
        ] {
            let (r, _) = run_ip(src, None, false);
            assert!(r.unwrap().starts_with("ERROR"), "{src}");
        }
    }

    // ================= URLSearchParams =================

    #[test]
    fn p13_usp() {
        let c = evc(
            r#"
            const p = new URLSearchParams("a=1&b=2&a=3");
            console.log(p.get("a"));
            console.log(p.getAll("a").join(","));
            console.log(p.has("b"));
            console.log(p.has("z"));
            p.append("c", "4");
            p.set("b", "20");
            p.delete("a");
            console.log(p.toString());
            console.log(p.size);
            const q = new URLSearchParams({x: "1", y: "2"});
            console.log(q.get("x") + q.get("y"));
            console.log(p.get("missing") === null);
            "#,
        );
        assert_eq!(
            c,
            vec!["1", "1,3", "true", "false", "b=20&c=4", "2", "12", "true"]
        );
    }

    #[test]
    fn p13_usp_for_each() {
        let c = evc(
            r#"
            const p = new URLSearchParams("a=1&b=2");
            const out = [];
            p.forEach((v, k) => out.push(k + "=" + v));
            console.log(out.join(","));
            "#,
        );
        assert_eq!(c, vec!["a=1,b=2"]);
    }

    // ================= TextEncoder / TextDecoder =================

    #[test]
    fn p13_text_encoder_decoder() {
        let c = evc(
            r#"
            const enc = new TextEncoder();
            const bytes = enc.encode("hi中文");
            console.log(bytes.length);
            console.log(bytes[0] + "," + bytes[1]);
            const dec = new TextDecoder();
            console.log(dec.decode(bytes));
            "#,
        );
        assert_eq!(c, vec!["8", "104,105", "hi中文"]);
        // 未知编码 / fatal 下的非法序列 → 运行期错误（fail-fast）
        let (r, _) = run_ip(r#"new TextDecoder("gbk");"#, None, false);
        assert!(r.unwrap().contains("unsupported encoding"));
        let (r, _) = run_ip(
            r#"new TextDecoder("utf-8", {fatal: true}).decode(new Uint8Array([0xff]));"#,
            None,
            false,
        );
        assert!(r.unwrap().contains("fatal"));
    }

    // ================= Blob =================

    #[test]
    fn p13_blob() {
        let c = evc(
            r#"
            const b = new Blob(["a", "bc"], {type: "text/plain"});
            console.log(b.size);
            console.log(b.type);
            console.log(b instanceof Blob);
            const s = b.slice(1, 3);
            console.log(s.size);
            b.text().then(t => console.log("text:" + t));
            b.arrayBuffer().then(ab => console.log("ab:" + ab.byteLength));
            "#,
        );
        assert_eq!(
            c,
            vec!["3", "text/plain", "true", "2", "text:abc", "ab:3"]
        );
    }

    // ================= FormData =================

    #[test]
    fn p13_formdata() {
        let c = evc(
            r#"
            const fd = new FormData();
            fd.append("a", "1"); fd.append("a", "2"); fd.append("b", "3");
            console.log(fd.get("a"));
            console.log(fd.getAll("a").join(","));
            console.log(fd.has("b"));
            console.log(fd.has("z"));
            fd.set("a", "9");
            console.log(fd.getAll("a").join(","));
            fd.delete("b");
            console.log(fd.has("b"));
            const out = [];
            fd.forEach((v, k) => out.push(k + "=" + v));
            console.log(out.join(","));
            console.log(fd instanceof FormData);
            "#,
        );
        assert_eq!(
            c,
            vec!["1", "1,2", "true", "false", "9", "false", "a=9", "true"]
        );
    }

    // ================= Storage =================

    #[test]
    fn p13_storage_basic() {
        let c = evc(
            r#"
            localStorage.setItem("k", "v");
            console.log(localStorage.getItem("k"));
            console.log(localStorage.getItem("missing") === null);
            console.log(localStorage.length);
            localStorage.setItem("a", "1");
            console.log(localStorage.key(0) + "=" + localStorage.key(1));
            console.log(localStorage.key(9) === null);
            localStorage.removeItem("k");
            console.log(localStorage.length);
            console.log(sessionStorage.length);
            sessionStorage.setItem("s", "x");
            console.log(localStorage.getItem("s") === null);
            console.log(sessionStorage.getItem("s"));
            localStorage.clear();
            console.log(localStorage.length);
            "#,
        );
        assert_eq!(
            c,
            vec!["v", "true", "1", "k=a", "true", "1", "0", "true", "x", "0"]
        );
    }

    #[test]
    fn p13_storage_named_props() {
        let c = evc(
            r#"
            localStorage.setItem("a", "1");
            localStorage.b = "2";
            console.log(localStorage.b);
            console.log(localStorage.getItem("b"));
            console.log(localStorage.length);
            localStorage.length = 99;
            console.log(localStorage.length);
            "#,
        );
        assert_eq!(c, vec!["2", "2", "2", "2"]);
    }

    #[test]
    fn p13_storage_persist() {
        let path = std::env::temp_dir().join("p13_storage_test.txt");
        let _ = std::fs::remove_file(&path);
        let ps = path.to_str().unwrap().to_string();
        // 写
        {
            let prog = parse_source(r#"localStorage.setItem("pk", "pv");"#).unwrap();
            let mut ip = Interpreter::new();
            ip.set_localstorage_path(&ps);
            ip.run(&prog).expect("run1");
        }
        // 新解释器读回
        {
            let prog =
                parse_source(r#"console.log("loaded:" + localStorage.getItem("pk"));"#)
                    .unwrap();
            let mut ip = Interpreter::new();
            ip.set_localstorage_path(&ps);
            ip.run(&prog).expect("run2");
            assert_eq!(ip.take_console(), vec!["loaded:pv"]);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn p13_instanceof_typeof() {
        let c = evc(
            r#"
            console.log(typeof new URL("https://a.com/"));
            console.log(new URL("https://a.com/") instanceof URL);
            console.log(new Blob([]) instanceof Blob);
            console.log(new FormData() instanceof FormData);
            console.log(new TextEncoder() instanceof TextEncoder);
            console.log(typeof localStorage);
            "#,
        );
        assert_eq!(c, vec!["object", "true", "true", "true", "true", "object"]);
    }

    // ================= setInterval =================

    #[test]
    fn p13_setinterval() {
        let (_, c) = run_ip(
            r#"
            let n = 0;
            const id = window.setInterval(() => {
                n++;
                console.log("tick" + n);
                if (n >= 3) window.clearInterval(id);
            }, 10);
            console.log("idnum:" + (typeof id));
            "#,
            None,
            true,
        );
        assert_eq!(c, vec!["idnum:number", "tick1", "tick2", "tick3"]);
    }

    // ================= WebSocket =================

    #[test]
    fn p13_websocket_loopback() {
        let (_, c) = run_ip(
            r#"
            const ws = new WebSocket("ws://example.com/chat");
            ws.onopen = () => {
                console.log("open:" + ws.readyState);
                ws.send("hi");
            };
            ws.onmessage = e => {
                console.log("msg:" + e.data);
                ws.close();
            };
            ws.onclose = e => console.log("close:" + e.code + ":" + ws.readyState);
            "#,
            Some(loopback()),
            false,
        );
        assert_eq!(c, vec!["open:1", "msg:echo:hi", "close:1000:3"]);
    }

    #[test]
    fn p13_websocket_binary() {
        let (_, c) = run_ip(
            r#"
            const ws = new WebSocket("ws://example.com/b");
            ws.binaryType = "arraybuffer";
            ws.onopen = () => ws.send(new Uint8Array([1, 2, 3]));
            ws.onmessage = e => {
                console.log("isbuf:" + (e.data instanceof ArrayBuffer));
                console.log("len:" + e.data.byteLength);
                ws.close();
            };
            "#,
            Some(loopback()),
            false,
        );
        assert_eq!(c, vec!["isbuf:true", "len:3"]);
    }

    #[test]
    fn p13_websocket_no_host() {
        // 未绑定宿主：清晰的 error 事件，不 panic
        let (_, c) = run_ip(
            r#"
            const ws = new WebSocket("ws://example.com/x");
            console.log("rs0:" + ws.readyState);
            ws.onerror = e => console.log("err:" + e.message);
            ws.onclose = () => console.log("closed:" + ws.readyState);
            "#,
            None,
            false,
        );
        assert_eq!(
            c,
            vec![
                "rs0:0",
                "err:WebSocket not implemented: no host bound (call bind_websocket_host)",
                "closed:3",
            ]
        );
    }

    #[test]
    fn p13_websocket_bad_scheme() {
        // 非 ws/wss scheme → 构造期运行错误（fail-fast）
        let (r, _) = run_ip(
            r#"new WebSocket("http://example.com/");"#,
            Some(loopback()),
            false,
        );
        assert!(
            r.unwrap()
                .contains("scheme must be either 'ws' or 'wss'"),
            "bad scheme"
        );
    }

    #[test]
    fn p13_websocket_send_early_throws() {
        // CONNECTING 时 send → 运行期错误（fail-fast）
        let (r, c) = run_ip(
            r#"
            const ws = new WebSocket("ws://example.com/x");
            ws.send("early");
            "#,
            Some(loopback()),
            false,
        );
        assert!(r.unwrap().contains("InvalidStateError"), "early send");
        assert!(c.is_empty());
    }

    #[test]
    fn p13_pump_websockets() {
        // 宿主事后才有数据：显式 pump_websockets() 驱动
        let host = Rc::new(ManualHost::default());
        let prog = parse_source(
            r#"
            const ws = new WebSocket("ws://x");
            ws.onmessage = e => console.log("m:" + e.data);
            "#,
        )
        .unwrap();
        let mut ip = Interpreter::new();
        ip.bind_websocket_host(host.clone());
        ip.run(&prog).expect("run");
        host.push_text("late");
        ip.pump_websockets();
        // 再跑一段空程序以排空任务队列
        ip.run(&parse_source("0;").unwrap()).expect("drain");
        assert_eq!(ip.take_console(), vec!["m:late"]);
    }

    // ================= Worker =================

    #[test]
    fn p13_worker_echo() {
        let (_, c) = run_ip(
            r#"
            const w = new Worker("onmessage = e => { postMessage('reply:' + e.data); };");
            w.onmessage = e => console.log("got:" + e.data);
            w.postMessage("ping");
            "#,
            None,
            false,
        );
        assert_eq!(c, vec!["got:reply:ping"]);
    }

    #[test]
    fn p13_worker_construction_post() {
        let (_, c) = run_ip(
            r#"
            const w = new Worker("postMessage('w1'); postMessage('w2');");
            w.onmessage = e => console.log("c:" + e.data);
            "#,
            None,
            false,
        );
        assert_eq!(c, vec!["c:w1", "c:w2"]);
    }

    #[test]
    fn p13_worker_terminate() {
        let (_, c) = run_ip(
            r#"
            const w = new Worker("postMessage('x');");
            w.terminate();
            w.onmessage = e => console.log("nope:" + e.data);
            console.log("terminated");
            "#,
            None,
            false,
        );
        assert_eq!(c, vec!["terminated"]);
    }

    #[test]
    fn p13_worker_console_forward() {
        let (_, c) = run_ip(
            r#"
            const w = new Worker("console.log('worker-hi');");
            w.onmessage = () => {};
            console.log("parent-hi");
            "#,
            None,
            false,
        );
        assert_eq!(c, vec!["worker-hi", "parent-hi"]);
    }

    #[test]
    fn p13_worker_script_error() {
        let (_, c) = run_ip(
            r#"
            const w = new Worker("throw new Error('boom');");
            w.onerror = e => console.log("werr:" + e.message);
            "#,
            None,
            false,
        );
        assert_eq!(c.len(), 1);
        assert!(c[0].contains("boom"), "unexpected: {:?}", c);
    }

    // ================= URL 解析单元测试 =================

    #[test]
    fn p13_parse_url_unit() {
        let u = parse_url("https://example.com:8080/a?x=1#f", None).unwrap();
        assert_eq!(u.protocol, "https:");
        assert_eq!(u.hostname, "example.com");
        assert_eq!(u.port, "8080");
        assert_eq!(u.pathname, "/a");
        assert_eq!(u.search, "?x=1");
        assert_eq!(u.hash, "#f");
        assert_eq!(u.href(), "https://example.com:8080/a?x=1#f");
        assert!(parse_url("http://x", None).is_err() == false);
        assert!(parse_url("nota url", None).is_err());
    }

    #[test]
    fn p13_base64_unit() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap_err(), "InvalidCharacterError: bad base64 length".to_string());
        assert!(base64_decode("!!!!").is_err());
    }
}
