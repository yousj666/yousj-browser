//! yousj-js · Phase 4：内置原型对象与原型链。
//!
//! - `BuiltinProtos`：`Object.prototype` / `Array.prototype` /
//!   `String.prototype` / `Number.prototype` / `Function.prototype`，
//!   每个解释器实例一份（经 `Rc` 共享）。
//! - 原型链关系：Array/String/Number/Function 的 prototype
//!   → Object.prototype → null。
//! - `proto_head_of`：取某个值的原型链头（`instanceof` 判定用）。

use std::cell::RefCell;
use std::rc::Rc;

use crate::value::{JsObject, ObjectRef, Value};

#[derive(Debug, Clone)]
pub struct BuiltinProtos {
    pub object: ObjectRef,
    pub array: ObjectRef,
    pub string: ObjectRef,
    pub number: ObjectRef,
    pub function: ObjectRef,
    /// `Promise.prototype`（phase 7）。
    pub promise: ObjectRef,
    /// `RegExp.prototype`（phase 7）。
    pub regexp: ObjectRef,
    /// `Generator.prototype`（phase 9）：生成器对象的原型链头。
    pub generator: ObjectRef,
    /// `AsyncGenerator.prototype`（phase 15）。
    pub async_generator: ObjectRef,
    /// phase 10：集合与二进制视图的原型。
    pub map: ObjectRef,
    pub set: ObjectRef,
    pub weakmap: ObjectRef,
    pub weakset: ObjectRef,
    pub arraybuffer: ObjectRef,
    pub typedarray: ObjectRef,
    pub dataview: ObjectRef,
    pub date: ObjectRef,
}

impl BuiltinProtos {
    pub fn new() -> Self {
        let object = Rc::new(RefCell::new(JsObject::new()));
        // object.proto = None（链顶）

        let mut array = JsObject::new();
        array.proto = Some(object.clone());
        let array = Rc::new(RefCell::new(array));

        let mut string = JsObject::new();
        string.proto = Some(object.clone());
        let string = Rc::new(RefCell::new(string));

        let mut number = JsObject::new();
        number.proto = Some(object.clone());
        let number = Rc::new(RefCell::new(number));

        let mut function = JsObject::new();
        function.proto = Some(object.clone());
        let function = Rc::new(RefCell::new(function));

        let mut promise = JsObject::new();
        promise.proto = Some(object.clone());
        let promise = Rc::new(RefCell::new(promise));

        let mut regexp = JsObject::new();
        regexp.proto = Some(object.clone());
        let regexp = Rc::new(RefCell::new(regexp));

        let mut generator = JsObject::new();
        generator.proto = Some(object.clone());
        let generator = Rc::new(RefCell::new(generator));

        let mut async_generator = JsObject::new();
        async_generator.proto = Some(object.clone());
        let async_generator = Rc::new(RefCell::new(async_generator));

        // phase 10：集合与二进制视图原型（皆以 Object.prototype 为父）。
        let mut protos10 = Vec::new();
        for _ in 0..8 {
            let mut p = JsObject::new();
            p.proto = Some(object.clone());
            protos10.push(Rc::new(RefCell::new(p)));
        }

        BuiltinProtos {
            object,
            array,
            string,
            number,
            function,
            promise,
            regexp,
            generator,
            async_generator,
            map: protos10[0].clone(),
            set: protos10[1].clone(),
            weakmap: protos10[2].clone(),
            weakset: protos10[3].clone(),
            arraybuffer: protos10[4].clone(),
            typedarray: protos10[5].clone(),
            dataview: protos10[6].clone(),
            date: protos10[7].clone(),
        }
    }
}

impl Default for BuiltinProtos {
    fn default() -> Self {
        Self::new()
    }
}

/// 取值的原型链头（`instanceof` 从此处开始沿链查找）。
pub fn proto_head_of(v: &Value, p: &BuiltinProtos) -> Option<ObjectRef> {
    match v {
        Value::Object(o) => o.borrow().proto.clone(),
        Value::Array(a) => a.borrow().proto.clone(),
        Value::String(_) => Some(p.string.clone()),
        Value::Number(_) => Some(p.number.clone()),
        Value::Function(_) | Value::Native(_) | Value::PromiseSettler(_) => {
            Some(p.function.clone())
        }
        Value::Promise(_) => Some(p.promise.clone()),
        Value::RegExp(_) => Some(p.regexp.clone()),
        // 生成器 → Generator.prototype；Proxy 透传 target 的原型链头。
        Value::Generator(_) => Some(p.generator.clone()),
        Value::Proxy(px) => proto_head_of(&px.borrow().target, p),
        // phase 10：集合与二进制视图 → 各自的 prototype。
        Value::Map(_) => Some(p.map.clone()),
        Value::Set(_) => Some(p.set.clone()),
        Value::WeakMap(_) => Some(p.weakmap.clone()),
        Value::WeakSet(_) => Some(p.weakset.clone()),
        Value::ArrayBuffer(_) => Some(p.arraybuffer.clone()),
        Value::TypedArray(_) => Some(p.typedarray.clone()),
        Value::DataView(_) => Some(p.dataview.clone()),
        Value::Date(_) => Some(p.date.clone()),
        // DOM 节点视为普通对象。
        Value::DomNode(_) => Some(p.object.clone()),
        _ => None,
    }
}

/// 判定 `lhs instanceof rhs`：沿 lhs 原型链找 rhs 的 `.prototype`。
pub fn instance_of(lhs: &Value, rhs: &Value, p: &BuiltinProtos) -> bool {
    let target: Option<ObjectRef> = match rhs {
        Value::Function(f) => f.prototype.clone(),
        Value::Object(o) => o
            .borrow()
            .get("prototype")
            .and_then(|v| match v {
                Value::Object(o) => Some(o),
                _ => None,
            }),
        _ => None,
    };
    let target = match target {
        Some(t) => t,
        None => return false,
    };
    let mut link = proto_head_of(lhs, p);
    while let Some(pr) = link {
        if Rc::ptr_eq(&pr, &target) {
            return true;
        }
        link = pr.borrow().proto.clone();
    }
    false
}
