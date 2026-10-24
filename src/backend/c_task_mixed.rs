//! The synchronous service setup may carry an async handler to registration,
//! but it must never execute the signature placeholder as an ordinary function.
use super::{expr_children, unsupported, walk_inst_exprs, walk_terminator_exprs};
use crate::{
    error::KuResult,
    ir::{
        task::TaskProgram, IrCallKind, IrExpr, IrExprKind, IrInst, IrLValue, IrProgram,
        IrTerminator,
    },
};
use std::collections::{HashMap, HashSet, VecDeque};

fn local(name: &str) -> String {
    format!("l:{name}")
}
fn temporary(id: crate::ir::TempId) -> String {
    format!("t:{}", id.0)
}
fn sources(expression: &IrExpr, async_ids: &HashSet<crate::ir::FunctionId>) -> (Vec<String>, bool) {
    let mut names = Vec::new();
    let mut seed = false;
    let mut work = vec![expression];
    while let Some(value) = work.pop() {
        match &value.kind {
            IrExprKind::Local(name)
            | IrExprKind::BorrowedParam(name)
            | IrExprKind::CapturedCell(name) => names.push(local(name)),
            IrExprKind::Temp(id) | IrExprKind::BorrowedTemp(id) => names.push(temporary(*id)),
            IrExprKind::MakeClosure { function_id, .. } => seed |= async_ids.contains(function_id),
            _ => work.extend(expr_children(value)),
        }
    }
    (names, seed)
}
fn tainted(
    expression: &IrExpr,
    async_ids: &HashSet<crate::ir::FunctionId>,
    aliases: &HashSet<String>,
) -> bool {
    let (names, seed) = sources(expression, async_ids);
    seed || names.iter().any(|name| aliases.contains(name))
}
pub(super) fn validate(program: &IrProgram, tasks: &TaskProgram) -> KuResult<()> {
    let names: HashSet<_> = tasks
        .functions
        .iter()
        .map(|function| function.name.as_str())
        .collect();
    let async_ids: HashSet<_> = program
        .functions
        .iter()
        .filter(|function| names.contains(function.name.as_str()))
        .map(|function| function.id)
        .collect();
    for function in &program.functions {
        let mut edges: HashMap<String, Vec<String>> = HashMap::new();
        let mut aliases = HashSet::new();
        let mut pending = VecDeque::new();
        for block in &function.blocks {
            for instruction in &block.instructions {
                let binding = match instruction {
                    IrInst::Temp { id, value, .. } => Some((temporary(*id), value)),
                    IrInst::Let { name, value, .. }
                    | IrInst::CellNew {
                        name, init: value, ..
                    } => Some((local(name), value)),
                    IrInst::Store {
                        target: IrLValue::Local(name),
                        value,
                    } => Some((local(name), value)),
                    IrInst::CellStore { cell, value } => {
                        let (destinations, _) = sources(cell, &async_ids);
                        let (from, seed) = sources(value, &async_ids);
                        for destination in destinations {
                            for source in &from {
                                edges
                                    .entry(source.clone())
                                    .or_default()
                                    .push(destination.clone());
                            }
                            if seed && aliases.insert(destination.clone()) {
                                pending.push_back(destination);
                            }
                        }
                        None
                    }
                    _ => None,
                };
                if let Some((destination, value)) = binding {
                    let (from, seed) = sources(value, &async_ids);
                    for source in from {
                        edges.entry(source).or_default().push(destination.clone());
                    }
                    if seed && aliases.insert(destination.clone()) {
                        pending.push_back(destination);
                    }
                }
                if let IrInst::DefineClosure {
                    name, function_id, ..
                } = instruction
                {
                    if async_ids.contains(function_id) && aliases.insert(local(name)) {
                        pending.push_back(local(name));
                    }
                }
            }
        }
        while let Some(source) = pending.pop_front() {
            if let Some(destinations) = edges.get(&source) {
                for destination in destinations {
                    if aliases.insert(destination.clone()) {
                        pending.push_back(destination.clone());
                    }
                }
            }
        }
        let mut failure = false;
        let mut check = |expression: &IrExpr| {
            let mut work = vec![expression];
            while let Some(value) = work.pop() {
                match &value.kind {
                    IrExprKind::Call { callee, args, kind } => {
                        let route = matches!(kind,IrCallKind::Intrinsic(name) if name.starts_with("__ku_http_route:"));
                        let clone =
                            matches!(kind,IrCallKind::Intrinsic(name) if name=="__ku_clone");
                        if matches!(kind,IrCallKind::Direct(id) if async_ids.contains(id))
                            || matches!(kind, IrCallKind::Indirect)
                                && tainted(callee, &async_ids, &aliases)
                            || !route
                                && !clone
                                && args
                                    .iter()
                                    .any(|argument| tainted(argument, &async_ids, &aliases))
                        {
                            failure = true;
                        }
                    }
                    IrExprKind::MakeClosure {
                        function_id,
                        captures,
                    } if !async_ids.contains(function_id) => {
                        if captures
                            .iter()
                            .any(|(name, _, _)| aliases.contains(&local(name)))
                        {
                            failure = true;
                        }
                    }
                    _ => {}
                }
                work.extend(expr_children(value));
            }
        };
        for block in &function.blocks {
            for instruction in &block.instructions {
                walk_inst_exprs(instruction, &mut check);
            }
            walk_terminator_exprs(&block.terminator, &mut check);
        }
        if failure
            || function.blocks.iter().any(|block| {
                matches!(&block.terminator,
            IrTerminator::Return(Some(value)) if tainted(value,&async_ids,&aliases))
            })
        {
            return Err(unsupported("native synchronous setup may register an async HTTP handler, but cannot call, capture or pass that handler to a synchronous function; use async helpers inside an async handler"));
        }
    }
    Ok(())
}
