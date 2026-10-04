//! yousj-js Phase 12：GC/内存优化基准。
//!
//! 带计数全局分配器的 bench 二进制：跑一组 JS 负载，报告耗时与
//! 分配器层面的峰值内存（`System` 包装计数，含 size-class 开销，
//! 仅用于优化前后的相对对比）。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct CountingAlloc;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let cur = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            let mut peak = PEAK.load(Ordering::Relaxed);
            while peak < cur {
                match PEAK.compare_exchange_weak(
                    peak,
                    cur,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(x) => peak = x,
                }
            }
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static A: CountingAlloc = CountingAlloc;

fn reset_peak() {
    PEAK.store(CURRENT.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn peak_kb() -> usize {
    PEAK.load(Ordering::Relaxed) / 1024
}

// ---- 负载 ----

/// 对象属性 churn：大量小对象 + 重复字符串键读写。
const BENCH_OBJECT: &str = r#"
let total = 0;
for (let i = 0; i < 3000; i++) {
    let o = { name: "obj" + i, x: i, y: i * 2, flag: true, tag: "t" };
    o.extra = i % 7;
    total += o.x + o.y + o.extra + o.name.length;
}
console.log(total);
"#;

/// 函数调用 / 环境链 churn：闭包 + 递归。
const BENCH_CALL: &str = r#"
function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
function adder(a) { return function (b) { return function (c) { return a + b + c; }; }; }
let s = 0;
for (let i = 0; i < 200; i++) { s += fib(15); }
for (let i = 0; i < 20000; i++) { s += adder(i)(i + 1)(i + 2); }
console.log(s);
"#;

/// 字符串拼接 churn。
const BENCH_STRING: &str = r#"
let s = "";
for (let i = 0; i < 20000; i++) { s += "k" + (i % 100) + ","; }
console.log(s.length);
"#;

/// 类定义 churn：每个类产生 prototype/constructor 引用环（优化前泄漏）。
const BENCH_CLASS: &str = r#"
let n = 0;
for (let i = 0; i < 3000; i++) {
    class P { constructor(x) { this.x = x; } get() { return this.x; } }
    let p = new P(i);
    n += p.get();
}
console.log(n);
"#;

/// 数组 + 高阶函数。
const BENCH_ARRAY: &str = r#"
let a = [];
for (let i = 0; i < 20000; i++) { a.push(i); }
let s = a.map(x => x * 2).filter(x => x % 3 === 0).reduce((p, c) => p + c, 0);
console.log(s);
"#;

fn run_bench(name: &str, src: &str, vm: bool) {
    // 预热一次（排除冷启动噪声），再正式计时。
    if vm {
        yousj_js::vm::eval_source_vm(src).unwrap();
    } else {
        yousj_js::interpreter::eval_source(src).unwrap();
    }
    reset_peak();
    let t = Instant::now();
    if vm {
        yousj_js::vm::eval_source_vm(src).unwrap();
    } else {
        yousj_js::interpreter::eval_source(src).unwrap();
    }
    let dt = t.elapsed();
    println!(
        "{:<28} {:>10.1} ms   peak {:>8} KB",
        name,
        dt.as_secs_f64() * 1000.0,
        peak_kb()
    );
}

fn main() {
    let vm = std::env::args().any(|a| a == "--vm");
    println!(
        "mode: {}",
        if vm { "VM(字节码)" } else { "tree(树遍历)" }
    );
    run_bench("object-churn", BENCH_OBJECT, vm);
    run_bench("call/env-churn", BENCH_CALL, vm);
    run_bench("string-churn", BENCH_STRING, vm);
    run_bench("class-churn", BENCH_CLASS, vm);
    run_bench("array-higher-order", BENCH_ARRAY, vm);
}
