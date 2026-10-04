//! test262 合规测试 harness（Phase 14）。
//!
//! 用法：
//!   cargo build --example test262
//!   ./target/debug/examples/test262 --t262 /tmp/test262 --dirs language --threads 8
//!   ./target/debug/examples/test262 --t262 /tmp/test262 --dirs language,built-ins --threads 8 --out /tmp/t262.jsonl
//!
//! 只用引擎公开 API（`eval_with_console`）。每个测试拼装：
//!   "use strict";(可选) + includes + $DONE shim + try/catch 包装 + 结果串
//! 最终表达式返回 "T262RESULT|syncok|errname|donecalled|doneerr"。

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use yousj_js::interpreter::eval_with_console;
use yousj_js::value::Value;

// ---- 支持的 harness includes（其余的测试直接 skip） ----
const SUPPORTED_INCLUDES: &[&str] = &[
    "sta.js",
    "assert.js",
    "compareArray.js",
    "nans.js",
    "deepEqual.js",
    "propertyHelper.js",
    "promiseHelper.js",
    "testTypedArray.js",
    "byteConversionValues.js",
    "dateConstants.js",
    "decimalToHexString.js",
    "isConstructor.js",
    "testIterator.js",
    "compareIterator.js",
    "regExpUtils.js",
    "typeCoercion.js",
    "wellKnownIntrinsicObjects.js",
    "nativeErrors.js",
    "fnGlobalObject.js",
    "asyncHelpers.js",
    "tcoHelper.js",
    "iteratorZipUtils.js",
    "proxyTrapsHelper.js",
    "assertRelativeDateMs.js",
    "nativeFunctionMatcher.js",
];

/// 明确不支持的 test262 特性（跳过；其中 tail-call-optimization 会
/// 100000 层递归直接 abort 进程，必须在运行前过滤）。
const UNSUPPORTED_FEATURES: &[&str] = &[
    "tail-call-optimization",
    "BigInt",
];

// ---- frontmatter ----

#[derive(Debug, Default)]
struct FrontMatter {
    flags: Vec<String>,
    includes: Vec<String>,
    features: Vec<String>,
    negative_phase: Option<String>,
    negative_type: Option<String>,
}

fn parse_frontmatter(src: &str) -> (FrontMatter, String) {
    let mut fm = FrontMatter::default();
    let start = match src.find("/*---") {
        Some(i) => i,
        None => return (fm, src.to_string()),
    };
    let end = match src[start..].find("---*/") {
        Some(i) => start + i + "---*/".len(),
        None => return (fm, src.to_string()),
    };
    let yaml = &src[start + "/*---".len()..end - "---*/".len()];
    let body = src[end..].to_string();

    let mut cur_key = String::new();
    let mut in_negative = false;
    for line in yaml.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        // negative 子块
        if in_negative && (line.starts_with("  ") || line.starts_with('\t')) {
            if let Some(v) = t.strip_prefix("phase:") {
                fm.negative_phase = Some(v.trim().to_string());
                continue;
            }
            if let Some(v) = t.strip_prefix("type:") {
                fm.negative_type = Some(v.trim().to_string());
                continue;
            }
        }
        in_negative = false;
        if t.starts_with("- ") && !cur_key.is_empty() {
            let item = t[2..].trim().to_string();
            push_item(&mut fm, &cur_key, &item);
            continue;
        }
        if let Some(idx) = t.find(':') {
            let key = t[..idx].trim().to_string();
            let val = t[idx + 1..].trim();
            if key == "negative" {
                in_negative = true;
                cur_key.clear();
                continue;
            }
            if val.starts_with('[') && val.ends_with(']') {
                for item in val[1..val.len() - 1].split(',') {
                    let item = item.trim().trim_matches('"').trim();
                    if !item.is_empty() {
                        push_item(&mut fm, &key, item);
                    }
                }
                cur_key.clear();
            } else if val.is_empty() {
                cur_key = key; // 块列表随后
            } else {
                cur_key.clear(); // description 等单行值，忽略
            }
        }
    }
    (fm, body)
}

fn push_item(fm: &mut FrontMatter, key: &str, item: &str) {
    match key {
        "flags" => fm.flags.push(item.to_string()),
        "includes" => fm.includes.push(item.to_string()),
        "features" => fm.features.push(item.to_string()),
        _ => {}
    }
}

// ---- 测试任务 ----

struct Task {
    file: PathBuf,
    strict: bool,
}

#[derive(Debug)]
struct Verdict {
    file: String,
    variant: String,
    status: String, // pass / fail / skip
    reason: String,
}

const SHIM: &str = r#"
var $T262_SYNC_DONE = false;
var $T262_ERROR_NAME = "";
var $T262_DONE_CALLED = false;
var $T262_DONE_ERROR = "";
function $DONE(err) {
  $T262_DONE_CALLED = true;
  if (err) {
    try { $T262_DONE_ERROR = String((err && err.message) || err); }
    catch (e) { $T262_DONE_ERROR = "error"; }
  }
}
function $T262_ERRNAME(e) {
  try {
    if (e && e.constructor && e.constructor.name) return e.constructor.name;
  } catch (x) {}
  return "unknown";
}
"#;

fn build_source(
    harness_dir: &Path,
    fm: &FrontMatter,
    body: &str,
    strict: bool,
    include_src: &HashMap<String, String>,
) -> String {
    let mut s = String::new();
    if strict {
        s.push_str("\"use strict\";\n");
    }
    // INTERPRETING.md：除 raw 外，assert.js + sta.js 必须先于测试求值。
    let is_raw = fm.flags.iter().any(|f| f == "raw");
    if !is_raw {
        for pre in ["assert.js", "sta.js"] {
            if let Some(c) = include_src.get(pre) {
                s.push_str(c);
                s.push('\n');
            }
        }
    }
    for inc in &fm.includes {
        if *inc == "assert.js" || *inc == "sta.js" {
            continue; // 已预加载，避免重复
        }
        if let Some(c) = include_src.get(inc) {
            s.push_str(c);
            s.push('\n');
        }
    }
    s.push_str(SHIM);
    s.push_str("try {\n");
    s.push_str(body);
    s.push_str("\n$T262_SYNC_DONE = true;\n} catch ($t262e) {\n  $T262_ERROR_NAME = $T262_ERRNAME($t262e);\n}\n");
    s.push_str(
        "\"T262RESULT|\" + ($T262_SYNC_DONE ? \"1\" : \"0\") + \"|\" + $T262_ERROR_NAME \
         + \"|\" + ($T262_DONE_CALLED ? \"1\" : \"0\") + \"|\" + $T262_DONE_ERROR",
    );
    let _ = harness_dir;
    s
}

fn error_type_from_msg(msg: &str) -> String {
    // JsError::Runtime 的 message 形如 "TypeError: xxx"
    let first = msg.split(':').next().unwrap_or("").trim();
    for t in [
        "SyntaxError",
        "TypeError",
        "ReferenceError",
        "RangeError",
        "Test262Error",
        "Error",
    ] {
        if first == t || msg.contains(&format!("{t}:")) {
            return t.to_string();
        }
    }
    "unknown".to_string()
}

fn run_one(
    harness: &HashMap<String, String>,
    task: &Task,
    only_filter: &Option<String>,
) -> Verdict {
    let rel = task
        .file
        .strip_prefix("/tmp/test262/test")
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| task.file.to_string_lossy().to_string());
    let variant = if task.strict { "strict" } else { "sloppy" }.to_string();
    let mk = |status: &str, reason: String| Verdict {
        file: rel.clone(),
        variant: variant.clone(),
        status: status.to_string(),
        reason,
    };
    if let Some(f) = only_filter {
        if !rel.contains(f) {
            return mk("skip", "filtered".to_string());
        }
    }
    let src = match fs::read_to_string(&task.file) {
        Ok(s) => s,
        Err(e) => return mk("skip", format!("read error: {e}")),
    };
    let (fm, body) = parse_frontmatter(&src);

    if fm.flags.iter().any(|f| f == "module") {
        return mk("skip", "module test".to_string());
    }
    if fm.flags.iter().any(|f| f == "CanBlockIsFalse") {
        return mk("skip", "CanBlockIsFalse".to_string());
    }
    for inc in &fm.includes {
        if !SUPPORTED_INCLUDES.contains(&inc.as_str()) {
            return mk("skip", format!("unsupported include: {inc}"));
        }
    }
    for feat in &fm.features {
        if UNSUPPORTED_FEATURES.contains(&feat.as_str()) {
            return mk("skip", format!("unsupported feature: {feat}"));
        }
    }

    let is_async = fm.flags.iter().any(|f| f == "async");
    let neg_phase = fm.negative_phase.clone().unwrap_or_else(|| "runtime".to_string());
    let neg_type = fm.negative_type.clone();

    let full = build_source(Path::new(""), &fm, &body, task.strict, harness);
    let (val, console) = match eval_with_console(&full) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("{e:?}");
            if msg.contains("Parse") || msg.contains("parse") {
                let pass = neg_type.is_some()
                    && (neg_phase == "parse" || neg_phase == "early");
                return mk(
                    if pass { "pass" } else { "fail" },
                    format!("parse error: {}", trunc(&msg, 120)),
                );
            }
            // 运行时错误逃逸出包装（drain 阶段等）
            let etype = error_type_from_msg(&msg);
            if let Some(nt) = &neg_type {
                if neg_phase == "runtime" && &etype == nt {
                    return mk("pass", "negative(runtime, escaped)".to_string());
                }
            }
            return mk("fail", format!("escaped error: {}", trunc(&msg, 150)));
        }
    };

    let s = match &val {
        Value::String(st) => st.clone(),
        v => return mk("fail", format!("no T262RESULT, got {v:?}")),
    };
    let parts: Vec<&str> = s.strip_prefix("T262RESULT|").unwrap_or("").split('|').collect();
    if parts.len() != 4 {
        return mk("fail", format!("bad T262RESULT: {}", trunc(&s, 100)));
    }
    let sync_done = parts[0] == "1";
    let err_name = parts[1];
    let done_called = parts[2] == "1";
    let done_error = parts[3];

    let uncaught: Vec<&String> = console
        .iter()
        .filter(|l| l.contains("uncaught exception") || l.contains("UnhandledPromiseRejection"))
        .collect();

    if is_async {
        if let Some(nt) = &neg_type {
            if neg_phase == "runtime" {
                // 期望异步抛错：$DONE 不应被调用，且 console 有对应类型
                let hit = uncaught.iter().any(|l| l.contains(nt));
                if !done_called && hit {
                    return mk("pass", "negative(async)".to_string());
                }
                return mk(
                    "fail",
                    format!("neg async: done={done_called} uncaught={uncaught:?}"),
                );
            }
            return mk("fail", format!("neg async phase={neg_phase} unsupported"));
        }
        if !done_called {
            return mk("fail", "$DONE never called".to_string());
        }
        if !done_error.is_empty() {
            return mk("fail", format!("$DONE error: {}", trunc(done_error, 120)));
        }
        if !uncaught.is_empty() {
            return mk("fail", format!("uncaught: {}", trunc(uncaught[0], 120)));
        }
        if !sync_done {
            return mk("fail", format!("sync throw in async test: {err_name}"));
        }
        mk("pass", String::new())
    } else {
        if !uncaught.is_empty() {
            return mk("fail", format!("uncaught: {}", trunc(uncaught[0], 120)));
        }
        if let Some(nt) = &neg_type {
            if neg_phase == "runtime" {
                if !sync_done && err_name == nt {
                    return mk("pass", "negative(runtime)".to_string());
                }
                return mk(
                    "fail",
                    format!("neg: want {nt}, sync_done={sync_done} got {err_name}"),
                );
            }
            return mk("fail", format!("neg phase={neg_phase} not matched at runtime"));
        }
        if sync_done {
            mk("pass", String::new())
        } else {
            mk("fail", format!("threw {err_name}"))
        }
    }
}

fn trunc(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out.replace('\n', " ")
}

fn collect_tests(test_root: &Path, dirs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let dir = test_root.join(d);
        collect_dir(&dir, &mut out);
    }
    out.sort();
    out
}

fn collect_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_dir(&p, out);
        } else if p.extension().map(|x| x == "js").unwrap_or(false) {
            // 跳过 _FIXTURE 等辅助文件
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("_FIXTURE") {
                    continue;
                }
                out.push(p);
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut t262 = String::from("/tmp/test262");
    let mut dirs = vec!["language".to_string()];
    let mut threads = 8usize;
    let mut out_path: Option<String> = None;
    let mut only: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--t262" => {
                t262 = args[i + 1].clone();
                i += 2;
            }
            "--dirs" => {
                dirs = args[i + 1].split(',').map(|s| s.to_string()).collect();
                i += 2;
            }
            "--threads" => {
                threads = args[i + 1].parse().unwrap_or(8);
                i += 2;
            }
            "--out" => {
                out_path = Some(args[i + 1].clone());
                i += 2;
            }
            "--only" => {
                only = Some(args[i + 1].clone());
                i += 2;
            }
            "--limit" => {
                limit = Some(args[i + 1].parse().unwrap_or(0));
                i += 2;
            }
            _ => i += 1,
        }
    }

    let harness_dir = Path::new(&t262).join("harness");
    let mut harness = HashMap::new();
    for inc in SUPPORTED_INCLUDES {
        let p = harness_dir.join(inc);
        if let Ok(c) = fs::read_to_string(&p) {
            // 去掉 harness 文件自己的 frontmatter（避免干扰）
            let (_, body) = parse_frontmatter(&c);
            harness.insert(inc.to_string(), body);
        }
    }

    let test_root = Path::new(&t262).join("test");
    let files = collect_tests(&test_root, &dirs);
    println!("collected {} test files", files.len());

    // 展开 strict/sloppy 变体
    let mut tasks: Vec<Task> = Vec::new();
    for f in &files {
        // 先读 frontmatter 决定变体（只读一次）
        let src = fs::read_to_string(f).unwrap_or_default();
        let (fm, _) = parse_frontmatter(&src);
        let only_strict = fm.flags.iter().any(|x| x == "onlyStrict");
        let no_strict = fm.flags.iter().any(|x| x == "noStrict");
        if !only_strict {
            tasks.push(Task { file: f.clone(), strict: false });
        }
        if !no_strict {
            tasks.push(Task { file: f.clone(), strict: true });
        }
        if let Some(lim) = limit {
            if tasks.len() >= lim {
                break;
            }
        }
    }
    println!("expanded to {} tasks", tasks.len());

    let queue = Arc::new(Mutex::new(VecDeque::from(tasks)));
    let (tx, rx) = mpsc::channel();
    let harness = Arc::new(harness);
    let only = Arc::new(only);

    for _ in 0..threads {
        let queue = queue.clone();
        let tx = tx.clone();
        let harness = harness.clone();
        let only = only.clone();
        thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || loop {
                let task = { queue.lock().unwrap().pop_front() };
                match task {
                    Some(t) => {
                        let v = run_one(&harness, &t, &only);
                        let _ = tx.send(v);
                    }
                    None => break,
                }
            })
            .unwrap();
    }
    drop(tx);

    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut skip = 0usize;
    let mut out_lines: Vec<String> = Vec::new();
    let mut done = 0usize;
    for v in rx {
        done += 1;
        match v.status.as_str() {
            "pass" => pass += 1,
            "fail" => fail += 1,
            _ => skip += 1,
        }
        if v.status == "fail" || v.status == "skip" {
            out_lines.push(format!(
                "{{\"file\":{:?},\"variant\":{:?},\"status\":{:?},\"reason\":{:?}}}",
                v.file, v.variant, v.status, v.reason
            ));
        }
        if done % 2000 == 0 {
            println!("... {done} done (pass={pass} fail={fail} skip={skip})");
        }
    }
    println!("TOTAL: pass={pass} fail={fail} skip={skip}");
    if let Some(p) = out_path {
        fs::write(&p, out_lines.join("\n")).unwrap();
        println!("wrote {}", p);
    }
}
