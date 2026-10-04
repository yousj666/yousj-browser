fn main() {
    for src in ["var x = new Array(4294967295); x.length", "var x = new Array(5); x.length", "var a=[1,2,3]; a.push(99); a.length"] {
        match yousj_js::interpreter::eval_with_console(src) {
            Ok((v, _)) => println!("{src} => {v:?}"),
            Err(e) => println!("{src} => ERR {:?}", format!("{e:?}").chars().take(60).collect::<String>()),
        }
    }
}
