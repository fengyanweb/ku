//! Conservative, private proofs for short synchronous HTTP work and request
//! field materialization in synchronous handlers and native Task factories.
//! Unknown work keeps the existing worker path. This is a size/work bound,
//! never a promise about OS scheduling or allocator latency.
use std::collections::{HashMap, HashSet};

use super::{
    closure_invoke_symbol, closure_thunk_targets, expr_children, walk_inst_exprs,
    walk_terminator_exprs, COutput,
};
use crate::ir::{
    BlockId, FunctionId, IrCallKind, IrExpr, IrExprKind, IrFunction, IrInst, IrLValue, IrProgram,
    IrTerminator, IrType, TempId,
};

const MAX_NODES: usize = 192;
const MAX_STRING: usize = 256;
const MAX_BYTE_WORK: usize = 4096;
const MAX_CALLS: usize = 8;

// The JSON proof has its own certificate. Runtime counters are:
// 1, body bytes, target bytes, body key bytes, body container slots,
// body lexical nodes, body fields, query key bytes, query slots, query fields,
// extra encoded bytes from legacy short \b/\f escapes (four per occurrence).
// Byte work counts bounded scans/copies/comparisons. Visits count fixed-size
// node/bucket operations (the runtime also checks the ABI element-size bound).
// A JSON call requires all32 untouched credits and consumes that complete turn.
// Bounds cover normal execution and returning errors. Existing fatal allocation
// failure still reports to stderr and terminates the process, as on the worker
// and on the original owner's string allocation paths; it is not a resumable
// Result or a promise of continued scheduling after process termination begins.
const JSON_INPUT_BYTES: usize = 2048;
const JSON_MAX_BYTE_WORK: usize = 32 * 4096;
const JSON_MAX_VISITS: usize = 32 * 192;
const JSON_MAX_VALUE_BYTES: usize = 16384;
const JSON_MAX_VALUE_NODES: usize = 256;
const JSON_MAX_DEPTH: usize = 24;
const JSON_METRICS: usize = 11;

#[derive(Clone, Default)]
struct JsonCost([usize; JSON_METRICS]);
impl JsonCost {
    fn constant(value: usize) -> Self {
        let mut out = Self::default();
        out.0[0] = value;
        out
    }
    fn metric(index: usize) -> Self {
        let mut out = Self::default();
        out.0[index] = 1;
        out
    }
    fn add(&self, other: &Self) -> Option<Self> {
        let mut out = Self::default();
        for (index, value) in out.0.iter_mut().enumerate() {
            *value = self.0[index].checked_add(other.0[index])?;
            // Avoid enormous generated certificates even when runtime input is0.
            if *value > 1 << 20 {
                return None;
            }
        }
        Some(out)
    }
    fn times(&self, factor: usize) -> Option<Self> {
        let mut out = Self::default();
        for (index, value) in out.0.iter_mut().enumerate() {
            *value = self.0[index].checked_mul(factor)?;
            if *value > 1 << 20 {
                return None;
            }
        }
        Some(out)
    }
    fn cover(&mut self, other: &Self) {
        for (value, next) in self.0.iter_mut().zip(&other.0) {
            *value = (*value).max(*next);
        }
    }
    fn c(&self) -> String {
        self.0
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}
#[derive(Clone)]
struct JsonBound {
    ty: IrType,
    bytes: JsonCost,
    encoded: JsonCost,
    keys: JsonCost,
    slots: JsonCost,
    nodes: JsonCost,
    fields: JsonCost,
    depth: usize,
    body_text: bool,
}
impl JsonBound {
    fn scalar(ty: &IrType, bytes: usize) -> Self {
        Self {
            ty: ty.clone(),
            bytes: JsonCost::constant(bytes),
            encoded: JsonCost::constant(bytes),
            keys: JsonCost::default(),
            slots: JsonCost::default(),
            nodes: JsonCost::constant(1),
            fields: JsonCost::default(),
            depth: 0,
            body_text: false,
        }
    }
    fn string(ty: &IrType, bytes: JsonCost) -> Option<Self> {
        let mut out = Self::scalar(ty, 0);
        out.encoded = bytes.times(6)?.add(&JsonCost::constant(2))?;
        out.bytes = bytes;
        Some(out)
    }
    fn retag(mut self, ty: &IrType) -> Option<Self> {
        match ty {
            IrType::Int | IrType::Bool | IrType::Null | IrType::Void => {
                return Some(Self::scalar(ty, if *ty == IrType::Int { 20 } else { 5 }))
            }
            IrType::Float
            | IrType::Array(_)
            | IrType::Unknown
            | IrType::Function
            | IrType::Closure { .. }
            | IrType::Cell(_) => return None,
            _ => {}
        }
        self.ty = ty.clone();
        self.checked()
    }
    fn checked(self) -> Option<Self> {
        let supported = match &self.ty {
            IrType::Int | IrType::Bool | IrType::Null | IrType::Str | IrType::Void => true,
            IrType::Named(name) => matches!(
                name.as_str(),
                "__ku_value" | "__ku_object" | "__ku_error_type" | "__ku_http_response"
            ),
            IrType::Result(inner) => {
                matches!(
                    inner.as_ref(),
                    IrType::Int | IrType::Bool | IrType::Null | IrType::Str
                ) || matches!(inner.as_ref(),IrType::Named(name) if matches!(name.as_str(),"__ku_value"|"__ku_object"|"__ku_error_type"|"__ku_http_response"))
            }
            _ => false,
        };
        (supported && self.depth <= JSON_MAX_DEPTH).then_some(self)
    }
    fn merge(mut self, other: &Self) -> Option<Self> {
        if self.ty != other.ty {
            return None;
        }
        self.bytes.cover(&other.bytes);
        self.encoded.cover(&other.encoded);
        self.keys.cover(&other.keys);
        self.slots.cover(&other.slots);
        self.nodes.cover(&other.nodes);
        self.fields.cover(&other.fields);
        self.depth = self.depth.max(other.depth);
        self.body_text &= other.body_text;
        self.checked()
    }
    fn combine(mut self, other: &Self) -> Option<Self> {
        self.bytes = self.bytes.add(&other.bytes)?;
        self.keys = self.keys.add(&other.keys)?;
        self.slots = self.slots.add(&other.slots)?;
        self.nodes = self.nodes.add(&other.nodes)?;
        self.fields = self.fields.add(&other.fields)?;
        self.depth = self.depth.max(other.depth);
        self.body_text = false;
        Some(self)
    }
}
#[derive(Default)]
struct JsonProof {
    instructions: usize,
    byte_work: JsonCost,
    visits: JsonCost,
    max_bytes: JsonCost,
    max_nodes: JsonCost,
    json: bool,
}
fn json_forward_cfg(function: &IrFunction) -> bool {
    function.blocks.iter().enumerate().all(|(position, block)| {
        let edges = match &block.terminator {
            IrTerminator::Next => function
                .blocks
                .get(position + 1)
                .map(|block| vec![block.id])
                .unwrap_or_default(),
            IrTerminator::Jump(target) => vec![*target],
            IrTerminator::Branch {
                then_block,
                else_block,
                ..
            } => vec![*then_block, *else_block],
            IrTerminator::ResultBranch {
                ok_block,
                err_block,
                ..
            } => vec![*ok_block, *err_block],
            IrTerminator::JumpErr { target, .. } => vec![*target],
            IrTerminator::Safepoint {
                continue_block,
                timeout_block,
            } => vec![*continue_block, *timeout_block],
            IrTerminator::SyncGuard {
                continue_block,
                cleanup_block,
            } => vec![*continue_block, *cleanup_block],
            IrTerminator::Return(_) | IrTerminator::PropagateErr(_) | IrTerminator::Unreachable => {
                vec![]
            }
            _ => return false,
        };
        edges.iter().all(|target| {
            function
                .blocks
                .iter()
                .position(|block| block.id == *target)
                .is_some_and(|next| next > position)
        })
    })
}
impl JsonProof {
    fn work(&mut self, bytes: JsonCost, visits: JsonCost) -> Option<()> {
        self.byte_work = self.byte_work.add(&bytes)?;
        self.visits = self.visits.add(&visits)?;
        Some(())
    }
    fn observe(&mut self, value: &JsonBound) -> Option<()> {
        self.max_bytes.cover(&value.bytes);
        self.max_bytes.cover(&value.encoded);
        self.max_nodes.cover(&value.nodes);
        Some(())
    }
    fn retire(&mut self, value: &JsonBound) -> Option<()> {
        // One bucket scan plus recursive leaves. Also charge the inactive legacy
        // Result error alternative, so all OOM/error-prefix cleanup is covered.
        let error = usize::from(matches!(value.ty, IrType::Result(_)));
        self.work(
            JsonCost::constant(error * 256),
            value
                .slots
                .add(&value.nodes.times(2)?)?
                .add(&JsonCost::constant(1 + error * 6))?,
        )
    }
    fn clone_tree(&mut self, value: &JsonBound) -> Option<()> {
        // <=16 probes for each inserted key, one hash and full leaf copies.
        // Three slot passes cover initialization, input traversal and a failed
        // clone's partial cleanup. No generic clone/drop implementation changes.
        self.work(
            value.bytes.add(&value.keys.times(17)?)?,
            value
                .slots
                .times(3)?
                .add(&value.fields.times(32)?)?
                .add(&value.nodes.times(4)?)?,
        )
    }
    fn expression(
        &mut self,
        expr: &IrExpr,
        parameter: &str,
        locals: &HashMap<String, JsonBound>,
        temps: &HashMap<TempId, JsonBound>,
    ) -> Option<JsonBound> {
        self.instructions = self.instructions.checked_add(1)?;
        if self.instructions > 512 {
            return None;
        }
        self.work(JsonCost::default(), JsonCost::constant(1))?;
        let value = match &expr.kind {
            IrExprKind::Literal(value) => {
                if expr.ty == IrType::Str {
                    let length = crate::backend::llvm::decode_string_literal(value)
                        .ok()?
                        .len();
                    if length > 256 {
                        return None;
                    }
                    JsonBound::string(&expr.ty, JsonCost::constant(length))?
                } else {
                    JsonBound::scalar(&expr.ty, if expr.ty == IrType::Int { 20 } else { 5 })
                }
            }
            IrExprKind::Local(name) | IrExprKind::BorrowedParam(name) => locals.get(name)?.clone(),
            IrExprKind::Temp(id) | IrExprKind::BorrowedTemp(id) => temps.get(id)?.clone(),
            IrExprKind::Borrow(value) => self.expression(value, parameter, locals, temps)?,
            IrExprKind::Field { target, name } if matches!(&target.kind,IrExprKind::Local(local)|IrExprKind::BorrowedParam(local) if local==parameter) => {
                match name.as_str() {
                    "body" => {
                        let mut value = JsonBound::string(&expr.ty, JsonCost::metric(1))?;
                        value.body_text = true;
                        value
                    }
                    "query" => {
                        let mut value = JsonBound::scalar(&expr.ty, 0);
                        value.bytes = JsonCost::metric(2);
                        value.encoded = value.bytes.times(6)?.add(&JsonCost::constant(50))?;
                        value.keys = JsonCost::metric(7);
                        value.slots = JsonCost::metric(8);
                        value.nodes = JsonCost::metric(9).add(&JsonCost::constant(1))?;
                        value.fields = JsonCost::metric(9);
                        value.depth = 1;
                        value
                    }
                    _ => return None,
                }
            }
            IrExprKind::Unary { expr: value, .. } => {
                let value = self.expression(value, parameter, locals, temps)?;
                if !matches!(value.ty, IrType::Int | IrType::Bool) {
                    return None;
                }
                JsonBound::scalar(&expr.ty, 20)
            }
            IrExprKind::Binary { left, op, right } => {
                let left = self.expression(left, parameter, locals, temps)?;
                let right = self.expression(right, parameter, locals, temps)?;
                if left.ty == IrType::Str && right.ty == IrType::Str {
                    let bytes = left.bytes.add(&right.bytes)?;
                    self.work(bytes.clone(), JsonCost::constant(1))?;
                    if *op == crate::ast::BinaryOp::Add {
                        JsonBound::string(&expr.ty, bytes)?
                    } else {
                        JsonBound::scalar(&expr.ty, 0)
                    }
                } else if matches!(left.ty, IrType::Int | IrType::Bool | IrType::Null)
                    && matches!(right.ty, IrType::Int | IrType::Bool | IrType::Null)
                {
                    JsonBound::scalar(&expr.ty, 20)
                } else {
                    return None;
                }
            }
            IrExprKind::Call {
                args,
                kind: IrCallKind::Intrinsic(name),
                ..
            } => {
                let values = args
                    .iter()
                    .map(|arg| self.expression(arg, parameter, locals, temps))
                    .collect::<Option<Vec<_>>>()?;
                match (name.as_str(), values.as_slice()) {
                    ("json.decode" | "json.parse" | "json.try_parse", [text])
                        if text.ty == IrType::Str && text.body_text =>
                    {
                        self.json = true;
                        // <=18 total hashes/comparison byte-passes over raw key
                        // bytes. <=46 field probe/rehash visits, four capacity/
                        // node passes include every partial-error cleanup.
                        self.work(
                            JsonCost::metric(1)
                                .times(8)?
                                .add(&JsonCost::metric(3).times(18)?)?,
                            JsonCost::metric(4)
                                .times(4)?
                                .add(&JsonCost::metric(5).times(4)?)?
                                .add(&JsonCost::metric(6).times(46)?)?,
                        )?;
                        let mut value = JsonBound::scalar(&expr.ty, 0);
                        value.bytes = JsonCost::metric(1);
                        // Legacy writer expands \b/\f to six-byte Unicode escapes.
                        // All other integer-only decoded token encodings fit
                        // their source span; E includes keys and values.
                        value.encoded = value.bytes.add(&JsonCost::metric(10))?;
                        value.keys = JsonCost::metric(3);
                        value.slots = JsonCost::metric(4);
                        value.nodes = JsonCost::metric(5);
                        value.fields = JsonCost::metric(6);
                        value.depth = 16;
                        value
                    }
                    ("json.encode" | "json.stringify", [value]) => {
                        self.json = true;
                        // <=8 fields: insertion sort only. Each key appears in
                        // <=14 comparator operands; no qsort or strtod here.
                        self.work(
                            value
                                .encoded
                                .times(8)?
                                .add(&value.keys.times(14)?)?
                                .add(&value.nodes.times(20)?)?,
                            value
                                .slots
                                .add(&value.fields.times(7)?)?
                                .add(&value.nodes.times(2)?)?,
                        )?;
                        JsonBound::string(&expr.ty, value.encoded.clone())?
                    }
                    ("__ku_object", _) if values.len() % 2 == 0 && values.len() <= 16 => {
                        let mut object = JsonBound::scalar(&expr.ty, 0);
                        object.slots = JsonCost::constant(if values.len() > 10 { 16 } else { 8 });
                        object.encoded = JsonCost::constant(2);
                        object.depth = 1;
                        for (args, values) in args.chunks_exact(2).zip(values.chunks_exact(2)) {
                            if !matches!(args[0].kind, IrExprKind::Literal(_))
                                || values[0].ty != IrType::Str
                                || values[0].bytes.0[0] > 64
                                || values[0].bytes.0[1..].iter().any(|n| *n != 0)
                            {
                                return None;
                            }
                            if !matches!(
                                &values[1].ty,
                                IrType::Int | IrType::Bool | IrType::Null | IrType::Str
                            ) && !matches!(&values[1].ty,IrType::Named(name) if name=="__ku_object"||name=="__ku_value")
                            {
                                return None;
                            }
                            object.encoded = object
                                .encoded
                                .add(&values[0].encoded)?
                                .add(&values[1].encoded)?
                                .add(&JsonCost::constant(2))?;
                            object.keys = object.keys.add(&values[0].bytes)?;
                            object = object.combine(&values[0])?.combine(&values[1])?;
                        }
                        object.depth = object.depth.checked_add(1)?;
                        object.fields = object.fields.add(&JsonCost::constant(values.len() / 2))?;
                        self.work(
                            JsonCost::constant(
                                values
                                    .iter()
                                    .step_by(2)
                                    .map(|key| key.bytes.0[0] * 18)
                                    .sum(),
                            ),
                            // Even an empty object initializes its eight buckets.
                            // The per-field allowance covers this for nonempty
                            // objects, but must not erase the empty-table cost.
                            JsonCost::constant((values.len() * 32).max(8)),
                        )?;
                        object
                    }
                    ("__ku_http_map_get", [map, key])
                        if key.ty == IrType::Str
                            && key.bytes.0[0] <= 64
                            && key.bytes.0[1..].iter().all(|n| *n == 0) =>
                    {
                        self.work(
                            map.bytes.add(&key.bytes.times(16)?)?,
                            JsonCost::constant(16),
                        )?;
                        JsonBound::string(&expr.ty, map.bytes.clone())?
                    }
                    ("__ku_value_get" | "kuvalue.take", [value, key])
                        if key.ty == IrType::Str
                            && key.bytes.0[0] <= 64
                            && key.bytes.0[1..].iter().all(|n| *n == 0) =>
                    {
                        if name == "kuvalue.take" {
                            self.retire(value)?;
                        } else {
                            self.clone_tree(value)?;
                        }
                        self.work(
                            key.bytes.times(16)?.add(&JsonCost::constant(256))?,
                            JsonCost::constant(16),
                        )?;
                        value.clone().retag(&expr.ty)?
                    }
                    ("kuvalue.as_int", [value]) => {
                        self.retire(value)?;
                        JsonBound::scalar(&IrType::Int, 20).retag(&expr.ty)?
                    }
                    ("kuvalue.as_str", [value]) => {
                        self.retire(value)?;
                        let mut out = JsonBound::string(&expr.ty, value.bytes.clone())?;
                        out.encoded = value.encoded.clone();
                        out
                    }
                    ("__ku_clone", [value]) => {
                        self.clone_tree(value)?;
                        value.clone()
                    }
                    ("__ku_drop_borrow_temp", [value]) => {
                        self.retire(value)?;
                        JsonBound::scalar(&expr.ty, 0)
                    }
                    ("ok" | "err" | "fail", [value]) => value.clone().retag(&expr.ty)?,
                    ("http.text" | "http.html" | "http.redirect", [body])
                        if body.ty == IrType::Str =>
                    {
                        body.clone().retag(&expr.ty)?
                    }
                    ("http.text" | "http.html" | "http.redirect", [status, body])
                        if status.ty == IrType::Int && body.ty == IrType::Str =>
                    {
                        body.clone().retag(&expr.ty)?
                    }
                    ("http.empty", []) => JsonBound::scalar(&expr.ty, 0),
                    ("http.empty", [status]) if status.ty == IrType::Int => {
                        JsonBound::scalar(&expr.ty, 0)
                    }
                    ("str", [value])
                        if matches!(value.ty, IrType::Int | IrType::Bool | IrType::Null) =>
                    {
                        JsonBound::string(&expr.ty, JsonCost::constant(32))?
                    }
                    ("str", [value]) if value.ty == IrType::Str => {
                        self.work(value.bytes.clone(), JsonCost::constant(1))?;
                        value.clone()
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };
        if value.ty != expr.ty {
            return None;
        }
        let value = value.checked()?;
        self.observe(&value)?;
        Some(value)
    }
    fn function(&mut self, function: &IrFunction) -> Option<()> {
        let [parameter] = function.params.as_slice() else {
            return None;
        };
        if parameter.ty != IrType::Named("__ku_http_request".into())
            || !function.captures.is_empty()
            || !json_forward_cfg(function)
            || function.blocks.len() > 128
        {
            return None;
        }
        let fields = request_fields(function)?;
        if fields & !36u32 != 0 {
            return None;
        }
        // Read-only gate, route-name projection (<=8192 bytes), request ABI and
        // all query construction/drop work are outside the user IR but included.
        self.work(
            JsonCost::constant(8192)
                .add(&JsonCost::metric(1).times(2)?)?
                .add(&JsonCost::metric(2).times(3)?)?,
            JsonCost::constant(192).add(&JsonCost::metric(5).times(4)?)?,
        )?;
        if fields & 4 != 0 {
            self.work(
                JsonCost::metric(2)
                    .times(8)?
                    .add(&JsonCost::metric(7).times(18)?)?,
                JsonCost::metric(8)
                    .times(3)?
                    .add(&JsonCost::metric(9).times(50)?)?,
            )?;
        }
        let mut locals = HashMap::new();
        let mut temps = HashMap::new();
        for block in &function.blocks {
            for instruction in &block.instructions {
                match instruction {
                    IrInst::Temp { id, ty, value } => {
                        let value = self.expression(value, &parameter.name, &locals, &temps)?;
                        if value.ty != *ty {
                            return None;
                        }
                        self.retire(&value)?;
                        let value = if let Some(old) = temps.get(id) {
                            value.merge(old)?
                        } else {
                            value
                        };
                        temps.insert(*id, value);
                    }
                    IrInst::Let { name, ty, value } => {
                        let value = self.expression(value, &parameter.name, &locals, &temps)?;
                        if value.ty != *ty {
                            return None;
                        }
                        self.retire(&value)?;
                        let value = if let Some(old) = locals.get(name) {
                            value.merge(old)?
                        } else {
                            value
                        };
                        locals.insert(name.clone(), value);
                    }
                    IrInst::Store {
                        target: IrLValue::Local(name),
                        value,
                    } => {
                        let value = self
                            .expression(value, &parameter.name, &locals, &temps)?
                            .merge(locals.get(name)?)?;
                        self.retire(&value)?;
                        locals.insert(name.clone(), value);
                    }
                    IrInst::BindOk { id, ty, result } => {
                        // Result_take only transfers and clears an addressable
                        // source. Its producer already paid for payload/error
                        // retirement, including any later overwrite or unwind.
                        // Keep the fixed take/zero/old-destination operations;
                        // charging the whole tree again would count one owned
                        // resource once per move rather than once per lifetime.
                        if !matches!(result.kind, IrExprKind::Local(_) | IrExprKind::Temp(_)) {
                            return None;
                        }
                        let value = self
                            .expression(result, &parameter.name, &locals, &temps)?
                            .retag(ty)?;
                        self.work(JsonCost::default(), JsonCost::constant(8))?;
                        let value = if let Some(old) = temps.get(id) {
                            value.merge(old)?
                        } else {
                            value
                        };
                        temps.insert(*id, value);
                    }
                    IrInst::Expr(value) => {
                        let value = self.expression(value, &parameter.name, &locals, &temps)?;
                        self.retire(&value)?;
                    }
                    _ => return None,
                }
            }
            let value = match &block.terminator {
                IrTerminator::Branch { condition, .. } => Some(condition),
                IrTerminator::ResultBranch { result, .. }
                | IrTerminator::JumpErr { result, .. }
                | IrTerminator::PropagateErr(result) => Some(result),
                IrTerminator::Return(value) => value.as_ref(),
                IrTerminator::Next
                | IrTerminator::Jump(_)
                | IrTerminator::Safepoint { .. }
                | IrTerminator::SyncGuard { .. }
                | IrTerminator::Unreachable => None,
                _ => return None,
            };
            if let Some(value) = value {
                let value = self.expression(value, &parameter.name, &locals, &temps)?;
                self.retire(&value)?;
            }
        }
        self.json.then_some(())
    }
}
fn bounded_json(function: &IrFunction) -> Option<JsonProof> {
    let response = matches!(&function.return_type,IrType::Named(name) if name=="__ku_http_response")
        || matches!(&function.return_type,IrType::Result(inner) if matches!(inner.as_ref(),IrType::Named(name) if name=="__ku_http_response"));
    if !response {
        return None;
    }
    let mut proof = JsonProof::default();
    proof.function(function)?;
    Some(proof)
}

#[derive(Clone)]
struct Bound {
    ty: IrType,
    bytes: usize,
}
impl Bound {
    fn new(ty: &IrType, bytes: usize) -> Option<Self> {
        fn admitted(ty: &IrType) -> bool {
            match ty {
                IrType::Int
                | IrType::Float
                | IrType::Bool
                | IrType::Null
                | IrType::Str
                | IrType::Void => true,
                IrType::Named(name) => name == "__ku_http_response" || name == "__ku_error_type",
                IrType::Result(inner) => admitted(inner),
                _ => false,
            }
        }
        (admitted(ty) && bytes <= MAX_STRING).then(|| Self {
            ty: ty.clone(),
            bytes,
        })
    }
}

#[derive(Default)]
struct Proof {
    nodes: usize,
    byte_work: usize,
    stack: HashSet<FunctionId>,
}
impl Proof {
    fn tick(&mut self, bytes: usize) -> Option<()> {
        self.nodes = self.nodes.checked_add(1)?;
        self.byte_work = self.byte_work.checked_add(bytes)?;
        (self.nodes <= MAX_NODES && self.byte_work <= MAX_BYTE_WORK).then_some(())
    }
    fn expression(
        &mut self,
        program: &IrProgram,
        expr: &IrExpr,
        locals: &HashMap<String, Bound>,
        temps: &HashMap<TempId, Bound>,
    ) -> Option<Bound> {
        self.tick(0)?;
        let bytes = match &expr.kind {
            IrExprKind::Literal(value) => {
                if expr.ty == IrType::Str {
                    crate::backend::llvm::decode_string_literal(value)
                        .ok()?
                        .len()
                } else {
                    0
                }
            }
            IrExprKind::Local(name) | IrExprKind::BorrowedParam(name) => {
                let bound = locals.get(name)?;
                if bound.ty != expr.ty {
                    return None;
                }
                bound.bytes
            }
            IrExprKind::Temp(id) | IrExprKind::BorrowedTemp(id) => {
                let bound = temps.get(id)?;
                if bound.ty != expr.ty {
                    return None;
                }
                bound.bytes
            }
            IrExprKind::Borrow(value) => self.expression(program, value, locals, temps)?.bytes,
            IrExprKind::Unary { expr: value, .. } => {
                if !matches!(value.ty, IrType::Int | IrType::Float | IrType::Bool) {
                    return None;
                }
                self.expression(program, value, locals, temps)?;
                0
            }
            IrExprKind::Binary { left, op, right } => {
                let a = self.expression(program, left, locals, temps)?;
                let b = self.expression(program, right, locals, temps)?;
                if a.ty == IrType::Str && b.ty == IrType::Str {
                    if *op == crate::ast::BinaryOp::Add {
                        let bytes = a.bytes.checked_add(b.bytes)?;
                        self.tick(bytes)?;
                        bytes
                    } else {
                        self.tick(a.bytes.max(b.bytes))?;
                        0
                    }
                } else if matches!(
                    a.ty,
                    IrType::Int | IrType::Float | IrType::Bool | IrType::Null
                ) && matches!(
                    b.ty,
                    IrType::Int | IrType::Float | IrType::Bool | IrType::Null
                ) {
                    0
                } else {
                    return None;
                }
            }
            IrExprKind::Call { args, kind, .. } => {
                let args = args
                    .iter()
                    .map(|arg| self.expression(program, arg, locals, temps))
                    .collect::<Option<Vec<_>>>()?;
                match kind {
                    IrCallKind::Direct(id) => {
                        let callee = program
                            .functions
                            .iter()
                            .find(|function| function.id == *id)?;
                        self.function(program, callee, &args)?.bytes
                    }
                    IrCallKind::Intrinsic(name) => match (name.as_str(), args.as_slice()) {
                        ("http.text" | "http.html" | "http.redirect", [body])
                            if body.ty == IrType::Str =>
                        {
                            body.bytes
                        }
                        ("http.text" | "http.html" | "http.redirect", [status, body])
                            if status.ty == IrType::Int && body.ty == IrType::Str =>
                        {
                            body.bytes
                        }
                        ("http.empty", []) => 0,
                        ("http.empty", [status]) if status.ty == IrType::Int => 0,
                        ("ok" | "err" | "fail", [value]) => value.bytes,
                        ("__ku_drop_borrow_temp", [value]) => {
                            self.tick(value.bytes)?;
                            0
                        }
                        ("__ku_clone", [value]) => {
                            self.tick(value.bytes)?;
                            value.bytes
                        }
                        ("str", [value]) => match value.ty {
                            IrType::Int => {
                                self.tick(32)?;
                                32
                            }
                            IrType::Bool | IrType::Null => 5,
                            IrType::Str => {
                                self.tick(value.bytes)?;
                                value.bytes
                            }
                            _ => return None,
                        },
                        _ => return None,
                    },
                    IrCallKind::Indirect => return None,
                }
            }
            // No arrays, arbitrary aggregate destructors, captured cells,
            // callbacks, indexing, JSON, I/O or unknown foreign calls.
            _ => return None,
        };
        Bound::new(&expr.ty, bytes)
    }
    fn function(
        &mut self,
        program: &IrProgram,
        function: &IrFunction,
        args: &[Bound],
    ) -> Option<Bound> {
        if function.params.len() != args.len()
            || !function.captures.is_empty()
            || self.stack.len() >= MAX_CALLS
            || !self.stack.insert(function.id)
        {
            return None;
        }
        if function.blocks.len() > MAX_NODES || !acyclic(function) {
            return None;
        }
        let mut locals = HashMap::new();
        let mut temps = HashMap::new();
        for (param, arg) in function.params.iter().zip(args) {
            if param.ty != arg.ty {
                return None;
            }
            locals.insert(param.name.clone(), arg.clone());
        }
        let mut returned = Bound::new(&function.return_type, 0)?;
        for block in &function.blocks {
            self.tick(0)?;
            for instruction in &block.instructions {
                self.tick(0)?;
                match instruction {
                    IrInst::Temp { id, ty, value } => {
                        let bound = self.expression(program, value, &locals, &temps)?;
                        if bound.ty != *ty {
                            return None;
                        }
                        let bytes = temps
                            .get(id)
                            .map_or(bound.bytes, |old: &Bound| old.bytes.max(bound.bytes));
                        temps.insert(*id, Bound::new(ty, bytes)?);
                    }
                    IrInst::Let { name, ty, value } => {
                        let bound = self.expression(program, value, &locals, &temps)?;
                        if bound.ty != *ty {
                            return None;
                        }
                        let bytes = locals
                            .get(name)
                            .map_or(bound.bytes, |old: &Bound| old.bytes.max(bound.bytes));
                        locals.insert(name.clone(), Bound::new(ty, bytes)?);
                    }
                    IrInst::Store {
                        target: IrLValue::Local(name),
                        value,
                    } => {
                        let bound = self.expression(program, value, &locals, &temps)?;
                        let previous = locals.get(name)?;
                        if bound.ty != previous.ty {
                            return None;
                        }
                        locals.insert(
                            name.clone(),
                            Bound::new(&bound.ty, bound.bytes.max(previous.bytes))?,
                        );
                    }
                    IrInst::Expr(value) => {
                        self.expression(program, value, &locals, &temps)?;
                    }
                    _ => return None,
                }
            }
            let value = match &block.terminator {
                IrTerminator::Branch { condition, .. } => Some(condition),
                IrTerminator::Return(Some(value)) => {
                    let bound = self.expression(program, value, &locals, &temps)?;
                    if bound.ty != returned.ty {
                        return None;
                    }
                    returned.bytes = returned.bytes.max(bound.bytes);
                    None
                }
                IrTerminator::Next
                | IrTerminator::Jump(_)
                | IrTerminator::Safepoint { .. }
                | IrTerminator::SyncGuard { .. }
                | IrTerminator::Return(None)
                | IrTerminator::Unreachable => None,
                _ => return None,
            };
            if let Some(value) = value {
                self.expression(program, value, &locals, &temps)?;
            }
        }
        self.stack.remove(&function.id);
        Some(returned)
    }
}

fn acyclic(function: &IrFunction) -> bool {
    fn visit(
        function: &IrFunction,
        id: BlockId,
        active: &mut HashSet<BlockId>,
        done: &mut HashSet<BlockId>,
    ) -> bool {
        if done.contains(&id) {
            return true;
        }
        if !active.insert(id) {
            return false;
        }
        let Some(position) = function.blocks.iter().position(|block| block.id == id) else {
            return false;
        };
        let edges = match &function.blocks[position].terminator {
            IrTerminator::Next => function
                .blocks
                .get(position + 1)
                .map(|block| vec![block.id])
                .unwrap_or_default(),
            IrTerminator::Jump(target) => vec![*target],
            IrTerminator::Branch {
                then_block,
                else_block,
                ..
            } => vec![*then_block, *else_block],
            IrTerminator::Safepoint {
                continue_block,
                timeout_block,
            } => vec![*continue_block, *timeout_block],
            IrTerminator::SyncGuard {
                continue_block,
                cleanup_block,
            } => vec![*continue_block, *cleanup_block],
            IrTerminator::Return(_) | IrTerminator::Unreachable => vec![],
            _ => return false,
        };
        // Bounds below are propagated once in block storage order, not by a
        // general CFG data-flow solver. Even an acyclic backward edge can make
        // a later store execute before an already-proven earlier expression.
        // Keep such layouts on workers rather than underestimating their work.
        if !edges.iter().all(|next| {
            function
                .blocks
                .iter()
                .position(|block| block.id == *next)
                .is_some_and(|target| target > position)
        }) || !edges
            .into_iter()
            .all(|next| visit(function, next, active, done))
        {
            return false;
        }
        active.remove(&id);
        done.insert(id);
        true
    }
    let mut active = HashSet::new();
    let mut done = HashSet::new();
    function
        .blocks
        .iter()
        .all(|block| visit(function, block.id, &mut active, &mut done))
}

fn proven(program: &IrProgram, function: &IrFunction) -> bool {
    // An unused request projects no maps or strings. Its zero-valued aggregate
    // adds only fixed construction/drop work to the existing owner path. Keep
    // every observed or escaped request on workers until field sizes are proven.
    let args = match function.params.as_slice() {
        [] => Vec::new(),
        [parameter] if request_fields(function) == Some(0) => vec![Bound {
            ty: parameter.ty.clone(),
            bytes: 0,
        }],
        _ => return false,
    };
    let response = matches!(&function.return_type,
        IrType::Named(name) if name=="__ku_http_response")
        || matches!(&function.return_type,
            IrType::Result(inner) if matches!(inner.as_ref(),IrType::Named(name) if name=="__ku_http_response"));
    response
        && Proof::default()
            .function(program, function, &args)
            .is_some()
}

fn request_fields(function: &IrFunction) -> Option<u32> {
    let [parameter] = function.params.as_slice() else {
        return None;
    };
    if parameter.ty != IrType::Named("__ku_http_request".into()) {
        return None;
    }
    let name = &parameter.name;
    let is_request = |expr: &IrExpr| {
        matches!(&expr.kind,
        IrExprKind::Local(local) | IrExprKind::BorrowedParam(local) if local == name)
    };
    let mut fields = 0u32;
    let mut nodes = 0usize;
    let mut inspect = |root: &IrExpr| -> Option<()> {
        let mut pending = vec![root];
        while let Some(expr) = pending.pop() {
            nodes += 1;
            if nodes > 8192 {
                return None;
            }
            if let IrExprKind::Field { target, name } = &expr.kind {
                if is_request(target) {
                    fields |= match name.as_str() {
                        "headers" => 1,
                        "params" => 2,
                        "query" => 4,
                        "method" => 8,
                        "path" => 16,
                        "body" => 32,
                        _ => return None,
                    };
                    continue;
                }
            }
            if is_request(expr) {
                return None;
            }
            pending.extend(expr_children(expr));
        }
        Some(())
    };
    for block in &function.blocks {
        for instruction in &block.instructions {
            if let IrInst::DefineClosure { captures, .. } = instruction {
                if captures.iter().any(|capture| capture == name) {
                    return None;
                }
            }
            if let IrInst::Store { target, .. } = instruction {
                match target {
                    IrLValue::Local(local) if local == name => return None,
                    IrLValue::Field { target, .. } => inspect(target)?,
                    IrLValue::Index { target, index } => {
                        inspect(target)?;
                        inspect(index)?;
                    }
                    _ => {}
                }
            }
            let mut valid = true;
            walk_inst_exprs(instruction, &mut |expr| {
                if inspect(expr).is_none() {
                    valid = false;
                }
            });
            if !valid {
                return None;
            }
        }
        let mut valid = true;
        walk_terminator_exprs(&block.terminator, &mut |expr| {
            if inspect(expr).is_none() {
                valid = false;
            }
        });
        if !valid {
            return None;
        }
    }
    Some(fields)
}

fn task_request_fields(function: &crate::ir::task::TaskFunction) -> Option<u32> {
    use crate::ir::task::{TaskOp, TaskSlotType, TaskTerminator};
    let request = |slot: &crate::ir::task::SlotId| {
        matches!(&function.slots[slot.0].ty, TaskSlotType::Value { ty, .. }
            if crate::ir::task::http_request_type(ty))
    };
    let mut fields = 0;
    // This consumes verified, typed Task IR, not the signature-only async
    // placeholder in synchronous IR. All whole-request transfers fall back;
    // there is no interprocedural or alias assumption. The exhaustive matches
    // force new Task capabilities to be reviewed before projection admits them.
    for state in &function.states {
        for operation in &state.operations {
            match operation {
                TaskOp::HttpMapGet { map, .. } => {
                    fields |= match map.as_str() {
                        "headers" => 1,
                        "params" => 2,
                        "query" => 4,
                        _ => return None,
                    };
                }
                TaskOp::ErrorField { src, field, .. } => {
                    if request(src) {
                        fields |= match field.as_str() {
                            "method" => 8,
                            "path" => 16,
                            "body" => 32,
                            _ => return None,
                        };
                    }
                }
                TaskOp::Move { src, dst } | TaskOp::Copy { src, dst } => {
                    if request(src) || request(dst) {
                        return None;
                    }
                }
                TaskOp::Start { arguments, .. } | TaskOp::StartIndirect { arguments, .. } => {
                    if arguments.iter().any(request) {
                        return None;
                    }
                }
                TaskOp::Read { slot } => {
                    if request(slot) {
                        return None;
                    }
                }
                // The typed verifier restricts all remaining operands to
                // primitives, errors, responses, flat arrays, PG/net handles,
                // or Task/callable identities; they cannot observe a request.
                // Drop accepts the zero owners of omitted fields.
                TaskOp::Drop { .. }
                | TaskOp::DropIfInit { .. }
                | TaskOp::FinallyEnter { .. }
                | TaskOp::FinallyLeave { .. }
                | TaskOp::FinallyUnwindBegin
                | TaskOp::FinallyUnwindEnd
                | TaskOp::ScopeExitBegin { .. }
                | TaskOp::ScopeExitEnd { .. }
                | TaskOp::ScopeEnter { .. }
                | TaskOp::Init { .. }
                | TaskOp::InitCallable { .. }
                | TaskOp::CloneCallable { .. }
                | TaskOp::ArrayMake { .. }
                | TaskOp::ArrayClone { .. }
                | TaskOp::ArrayLen { .. }
                | TaskOp::StringLen { .. }
                | TaskOp::StringClone { .. }
                | TaskOp::PrimitiveString { .. }
                | TaskOp::StringConcat { .. }
                | TaskOp::ArrayTryGet { .. }
                | TaskOp::ArrayTake { .. }
                | TaskOp::Bytes { .. }
                | TaskOp::Unary { .. }
                | TaskOp::Binary { .. }
                | TaskOp::WrapOk { .. }
                | TaskOp::MakeError { .. }
                | TaskOp::WrapError { .. }
                | TaskOp::PgResult { .. }
                | TaskOp::JsonEncodeObject { .. }
                | TaskOp::JsonDecode { .. }
                | TaskOp::JsonValue { .. }
                | TaskOp::JsonObject { .. }
                | TaskOp::HttpResponse { .. }
                | TaskOp::CheckForLimit { .. }
                | TaskOp::Print { .. } => {}
            }
        }
        match &state.terminator {
            TaskTerminator::Complete { value } | TaskTerminator::Exit { value } => {
                if request(value) {
                    return None;
                }
            }
            TaskTerminator::ScopeDrain { .. }
            | TaskTerminator::Jump { .. }
            | TaskTerminator::Branch { .. }
            | TaskTerminator::Suspend { .. }
            | TaskTerminator::Await { .. }
            | TaskTerminator::Timer { .. }
            | TaskTerminator::BlockingWrite { .. }
            | TaskTerminator::BlockingRead { .. }
            | TaskTerminator::BlockingReadDir { .. }
            | TaskTerminator::BlockingExists { .. }
            | TaskTerminator::BlockingNet { .. }
            | TaskTerminator::PgQuery { .. }
            | TaskTerminator::TryResult { .. }
            | TaskTerminator::Terminate => {}
        }
    }
    Some(fields)
}

pub(super) fn emit(
    out: &mut COutput,
    program: &IrProgram,
    tasks: Option<&crate::ir::task::TaskProgram>,
    task_factories: bool,
) {
    let async_names: HashSet<_> = tasks
        .into_iter()
        .flat_map(|tasks| &tasks.functions)
        .map(|function| function.name.as_str())
        .collect();
    let targets = closure_thunk_targets(program);
    out.push_str("static int ku_http_handler_is_short(void* invoke) {\n");
    for function in &program.functions {
        if (!function.is_closure_body && !targets.iter().any(|target| target.id == function.id))
            || !proven(program, function)
        {
            continue;
        }
        if let Some(symbol) = closure_invoke_symbol(function.id) {
            out.push_str(&format!(
                "  if(invoke==(void*){symbol}) return 1; /* bounded owner handler {} */\n",
                function.id.0
            ));
        }
    }
    out.push_str("  return 0;\n}\n");
    // Descriptor lookup has a fixed comparison bound as well. Larger programs
    // retain their complete worker capability.
    let json_candidates = program
        .functions
        .iter()
        .filter(|function| {
            (function.is_closure_body || targets.iter().any(|target| target.id == function.id))
                && bounded_json(function).is_some()
        })
        .count();
    out.push_str(&format!("/* JSON certificate caps: input {JSON_INPUT_BYTES}, bytes {JSON_MAX_BYTE_WORK}, visits {JSON_MAX_VISITS}, output {JSON_MAX_VALUE_BYTES}, nodes {JSON_MAX_VALUE_NODES}. */\n"));
    out.push_str(
        "typedef struct { size_t bytes[11],visits[11],output[11],nodes[11]; } KuHttpJsonWork;\n",
    );
    out.push_str("static const KuHttpJsonWork* ku_http_handler_json_work(void* invoke) {\n");
    for function in &program.functions {
        if !function.is_closure_body && !targets.iter().any(|target| target.id == function.id) {
            continue;
        }
        if json_candidates > 64 {
            continue;
        }
        if let (Some(proof), Some(symbol)) =
            (bounded_json(function), closure_invoke_symbol(function.id))
        {
            out.push_str(&format!("  if(invoke==(void*){symbol}) {{ static const KuHttpJsonWork work={{{{{}}},{{{}}},{{{}}},{{{}}}}}; return &work; }} /* bounded JSON owner handler {} */\n",proof.byte_work.c(),proof.visits.c(),proof.max_bytes.c(),proof.max_nodes.c(),function.id.0));
        }
    }
    out.push_str("  return NULL;\n}\n");
    out.push_str("static unsigned int ku_http_handler_request_fields(void* invoke) {\n");
    for function in &program.functions {
        if async_names.contains(function.name.as_str())
            || (!function.is_closure_body && !targets.iter().any(|target| target.id == function.id))
        {
            continue;
        }
        if let (Some(mask), Some(symbol)) =
            (request_fields(function), closure_invoke_symbol(function.id))
        {
            out.push_str(&format!("  if(invoke==(void*){symbol}) return {mask}u;\n"));
        }
    }
    if let Some(tasks) = tasks.filter(|_| task_factories) {
        for function in &tasks.functions {
            if super::task_http::is_handler(function) {
                if let Some(mask) = task_request_fields(function) {
                    out.push_str(&format!(
                        "  if(invoke==(void*)ku_http_task_start_{}) return {mask}u;\n",
                        function.id.0
                    ));
                }
            }
        }
    }
    out.push_str("  return 63u; /* unknown use: retain complete request */\n}\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    fn accepts(source: &str, name: &str) -> bool {
        let ast = crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
            .parse_program()
            .unwrap();
        let program = crate::ir::lower_program(&ast).unwrap();
        proven(
            &program,
            program.functions.iter().find(|f| f.name == name).unwrap(),
        )
    }
    #[test]
    fn owner_proof_bounds_transitive_work_and_rejects_unknown_paths() {
        assert!(accepts("import \"std.http\" fn Ready(): HttpResponse { return http.text(\"ok\") } fn main() {}","Ready"));
        assert!(accepts("import \"std.http\" fn Text(): str { return \"ok\".clone() } fn Ready(): HttpResponse { return http.text(Text()) } fn main() {}","Ready"));
        assert!(accepts(
            "import \"std.http\" fn Ready(): HttpResponse! { return err(\"no\") } fn main() {}",
            "Ready"
        ));
        assert!(!accepts("import \"std.http\" fn Loop(): int { while (true) {} return 1 } fn Ready(): HttpResponse { n=Loop() return http.empty(n) } fn main() {}","Ready"));
        assert!(!accepts("import \"std.http\" fn Rec(): int { return Rec() } fn Ready(): HttpResponse { return http.empty(Rec()) } fn main() {}","Ready"));
        assert!(!accepts("import \"std.http\" fn Ready(req: HttpRequest): HttpResponse { return http.text(req.body) } fn main() {}","Ready"));
        for (returns, body) in [
            ("HttpResponse", "return http.text(\"ok\")"),
            ("HttpResponse", "return http.text(str(40 + 2))"),
            ("HttpResponse!", "return err(\"no\")"),
            ("HttpResponse!", "return ok(http.text(\"ok\"))"),
        ] {
            assert!(accepts(&format!("import \"std.http\" fn Ready(_req: HttpRequest): {returns} {{ {body} }} fn main() {{}}"),"Ready"));
        }
        for body in [
            "copy=req return ok(http.text(\"ok\"))",
            "return Read(req)",
            "while (true) {} return ok(http.text(\"ok\"))",
            "return ok(http.text(json.encode({a: 1})?))",
        ] {
            assert!(!accepts(&format!("import {{ http, json }} from \"std\" fn Read(req: HttpRequest): HttpResponse! {{ return ok(http.text(req.body)) }} fn Ready(req: HttpRequest): HttpResponse! {{ {body} }} fn main() {{}}"),"Ready"));
        }
        let long = "a".repeat(MAX_STRING + 1);
        assert!(!accepts(&format!("import \"std.http\" fn Ready(): HttpResponse {{ return http.text(\"{long}\") }} fn main() {{}}"),"Ready"));
    }
    #[test]
    fn owner_proof_rejects_acyclic_backward_layout_before_reusing_bounds() {
        let ast=crate::parser::Parser::new(crate::lexer::Lexer::new(
            r#"import "std.http" fn Ready(): HttpResponse { return http.text("ok") } fn main() {}"#
        ).tokenize().unwrap()).parse_program().unwrap();
        let mut program = crate::ir::lower_program(&ast).unwrap();
        let index = program
            .functions
            .iter()
            .position(|function| function.name == "Ready")
            .unwrap();
        let literal = |text: &str| IrExpr {
            ty: IrType::Str,
            kind: IrExprKind::Literal(format!("\"{text}\"")),
        };
        let local = || IrExpr {
            ty: IrType::Str,
            kind: IrExprKind::Local("text".into()),
        };
        let response = IrExpr {
            ty: IrType::Named("__ku_http_response".into()),
            kind: IrExprKind::Call {
                callee: Box::new(IrExpr {
                    ty: IrType::Function,
                    kind: IrExprKind::Local("http.text".into()),
                }),
                args: vec![IrExpr {
                    ty: IrType::Str,
                    kind: IrExprKind::Binary {
                        left: Box::new(local()),
                        op: crate::ast::BinaryOp::Add,
                        right: Box::new(local()),
                    },
                }],
                kind: IrCallKind::Intrinsic("http.text".into()),
            },
        };
        // Execution is 0 -> 2 -> 1, with no cycle. A storage-order-only scan
        // previously bounded the return as two bytes, before seeing the store
        // that really makes the concatenation 2*MAX_STRING bytes.
        program.functions[index].blocks = vec![
            crate::ir::IrBlock {
                id: BlockId(0),
                name: "entry".into(),
                instructions: vec![IrInst::Let {
                    name: "text".into(),
                    ty: IrType::Str,
                    value: literal("a"),
                }],
                terminator: IrTerminator::Jump(BlockId(2)),
            },
            crate::ir::IrBlock {
                id: BlockId(1),
                name: "return".into(),
                instructions: vec![],
                terminator: IrTerminator::Return(Some(response)),
            },
            crate::ir::IrBlock {
                id: BlockId(2),
                name: "grow".into(),
                instructions: vec![IrInst::Store {
                    target: IrLValue::Local("text".into()),
                    value: literal(&"x".repeat(MAX_STRING)),
                }],
                terminator: IrTerminator::Jump(BlockId(1)),
            },
        ];
        assert!(!proven(&program, &program.functions[index]));
    }

    #[test]
    fn json_owner_empty_object_work_cannot_hide_below_the_turn_limit() {
        let source = r#"import {http,json} from "std"
            fn Reply(_req:HttpRequest):HttpResponse! {
                return ok(http.text(json.encode(null)?))
            } fn main() {}"#;
        let ast = crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
            .parse_program()
            .unwrap();
        let program = crate::ir::optimize_program(&crate::ir::lower_program(&ast).unwrap());
        let mut function = program
            .functions
            .iter()
            .find(|function| function.name == "Reply")
            .unwrap()
            .clone();
        // Typed IR counterexample: 400 zero-field object temporaries already
        // require 6,400 bucket initializations/retirement visits. The original
        // certificate forgot initialization and incorrectly fitted this turn.
        // Build IR after optimization so unrelated DCE cannot erase the witness.
        let objects = (0..400).map(|index| IrInst::Temp {
            id: TempId(100_000 + index),
            ty: IrType::Named("__ku_object".into()),
            value: IrExpr {
                ty: IrType::Named("__ku_object".into()),
                kind: IrExprKind::Call {
                    callee: Box::new(IrExpr {
                        ty: IrType::Function,
                        kind: IrExprKind::Local("__ku_object".into()),
                    }),
                    args: vec![],
                    kind: IrCallKind::Intrinsic("__ku_object".into()),
                },
            },
        });
        function.blocks[0].instructions.splice(0..0, objects);
        let proof = bounded_json(&function).expect("finite typed JSON IR");
        assert!(proof.instructions <= 512);
        assert!(
            proof.visits.0[0] > JSON_MAX_VISITS,
            "the runtime must keep this over-budget handler on its worker"
        );
    }
    #[test]
    fn json_owner_certificate_keeps_pure_value_flow_and_rejects_escape() {
        fn plan(body: &str) -> Option<JsonProof> {
            let source = format!(
                r#"import {{http,json}} from "std"
                fn Reply(req:HttpRequest):HttpResponse! {{ {body} }} fn main() {{}}"#
            );
            let ast =
                crate::parser::Parser::new(crate::lexer::Lexer::new(&source).tokenize().unwrap())
                    .parse_program()
                    .unwrap();
            let program = crate::ir::optimize_program(&crate::ir::lower_program(&ast).unwrap());
            bounded_json(
                program
                    .functions
                    .iter()
                    .find(|function| function.name == "Reply")
                    .unwrap(),
            )
        }
        let proof = plan(
            r#"decoded=json.decode(req.body)?
            count=decoded["count"]?.as_int()?
            payload=decoded.take("payload")?
            encoded=json.encode({trace:req.query.trace,total:count+1,payload:payload})?
            return ok(http.text(encoded))"#,
        )
        .expect("bounded JSON value flow has an input certificate");
        // A concrete small tree fits; wider trees may conservatively retain
        // workers without changing any of their public JSON capabilities.
        for metrics in [
            [1, 120, 24, 24, 24, 16, 4, 5, 8, 1, 0],
            [1, 1048, 80, 64, 72, 56, 10, 5, 8, 1, 0],
        ] {
            let evaluate = |cost: &JsonCost| {
                cost.0
                    .iter()
                    .zip(metrics)
                    .map(|(a, b)| a * b)
                    .sum::<usize>()
            };
            assert!(evaluate(&proof.byte_work) <= JSON_MAX_BYTE_WORK);
            assert!(evaluate(&proof.visits) <= JSON_MAX_VISITS);
            assert!(evaluate(&proof.max_bytes) <= JSON_MAX_VALUE_BYTES);
            assert!(evaluate(&proof.max_nodes) <= JSON_MAX_VALUE_NODES);
        }
        assert!(
            proof.max_bytes.0[10] > 0,
            "decoded escapes remain in the output certificate"
        );
        let escaped = plan(
            r#"value=json.decode(req.body)?
            text=value.as_str()?
            first=json.encode(text)?
            return ok(http.text(json.encode(first)?))"#,
        )
        .expect("bounded escape composition");
        // body "\\b" has4 source bytes and E=4: first encoded string has8
        // bytes, re-encoding that returned string must have its own escaping.
        let metrics = [1, 4, 5, 0, 0, 1, 0, 0, 8, 0, 4];
        let max_encoded = escaped
            .max_bytes
            .0
            .iter()
            .zip(metrics)
            .map(|(a, b)| a * b)
            .sum::<usize>();
        assert!(
            max_encoded >= 14,
            "as_str + encode + encode cannot discard escape provenance"
        );
        assert!(escaped.max_bytes.0[10] >= 6);

        for body in [
            r#"x=req return ok(http.text(json.encode(x.body)?))"#,
            r#"while(true) {} return ok(http.text(json.encode(req.body)?))"#,
            r#"x=json.decode(req.query.value)? return ok(http.text(json.encode(x)?))"#,
            r#"x=json.decode(req.body)? x["cycle"]=x return ok(http.text(json.encode(x)?))"#,
            r#"return ok(http.text(json.encode({number:1.5})?))"#,
            r#"x=json.decode(req.body)? println(x) return ok(http.text(json.encode(x)?))"#,
            r#"return ok(http.text(json.encode({a:1,b:2,c:3,d:4,e:5,f:6,g:7,h:8,i:9})?))"#,
        ] {
            assert!(plan(body).is_none(), "worker fallback: {body}");
        }
    }
    #[test]
    fn request_field_proof_rejects_whole_value_escape_and_keeps_direct_fields() {
        fn mask(source: &str) -> Option<u32> {
            let ast =
                crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                    .parse_program()
                    .unwrap();
            let program = crate::ir::lower_program(&ast).unwrap();
            request_fields(
                program
                    .functions
                    .iter()
                    .find(|function| function.name == "Ready")
                    .unwrap(),
            )
        }
        assert_eq!(mask("import \"std.http\" fn Ready(req: HttpRequest): HttpResponse { return http.text(req.body + req.query.id) } fn main() {}"),Some(36));
        assert_eq!(mask("import \"std.http\" fn Read(req: HttpRequest): HttpResponse { return http.text(req.body) } fn Ready(req: HttpRequest): HttpResponse { return Read(req) } fn main() {}"),None);
        assert_eq!(mask("import \"std.http\" fn Ready(req: HttpRequest): HttpResponse { copy=req.clone() return http.text(copy.body) } fn main() {}"),None);
        assert_eq!(mask("import \"std.http\" fn Ready(req: HttpRequest): HttpResponse { return http.text(req.headers.host + req.params.id + req.path + req.method) } fn main() {}"),Some(27));
    }

    #[test]
    fn task_request_projection_uses_real_body_and_rejects_whole_request_forwarding() {
        fn mask(body: &str) -> Option<u32> {
            let ast =
                crate::parser::Parser::new(crate::lexer::Lexer::new(body).tokenize().unwrap())
                    .parse_program()
                    .unwrap();
            let tasks = crate::ir::task_lower::lower_service_handlers(&ast).unwrap();
            task_request_fields(tasks.functions.iter().find(|f| f.name == "Ready").unwrap())
        }
        for (expression, expected) in [
            ("req.headers.host", 1),
            ("req.params.id", 2),
            ("req.query.rid", 4),
            ("req.method", 8),
            ("req.path", 16),
            ("req.body", 32),
        ] {
            let source = format!("import \"std.http\" async fn Ready(req: HttpRequest): HttpResponse! {{ return ok(http.text({expression})) }} fn main() {{}}");
            assert_eq!(mask(&source), Some(expected), "field {expression}");
        }
        assert_eq!(mask("import \"std.http\" async fn Ready(req: HttpRequest): HttpResponse! { return ok(http.text(req.query.rid)) } fn main() {}"),Some(4));
        assert_eq!(mask("import \"std.http\" async fn Ready(req: HttpRequest): HttpResponse! { return ok(http.text(req.headers.host + req.params.id + req.method + req.path + req.body)) } fn main() {}"),Some(59));
        assert_eq!(mask("import \"std.http\" async fn Ready(): HttpResponse! { return ok(http.text(\"ok\")) } fn main() {}"),Some(0));
        assert_eq!(mask("import \"std.http\" async fn Child(req: HttpRequest): HttpResponse! { return ok(http.text(req.body)) } async fn Ready(req: HttpRequest): HttpResponse! { return await Child(req) } fn main() {}"),None);
        assert_eq!(mask("import \"std.http\" async fn Ready(req: HttpRequest): HttpResponse! { alias=req return ok(http.text(alias.body)) } fn main() {}"),None);
    }
}
