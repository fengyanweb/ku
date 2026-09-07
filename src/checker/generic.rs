//! Bounded, concrete ownership checking for the existing generic-function syntax.
//! Runtime values remain unchanged; this is compiler-only specialization state.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};

const MAX_INSTANCES: usize = 256;
const MAX_INSTANCE_DEPTH: usize = 32;
const MAX_TYPE_DEPTH: usize = 32;
const MAX_KEY_BYTES: usize = 4096;
const MAX_INSTANCE_BODY_NODES: usize = 262_144;
const MAX_INSTANCE_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_CALL_SITES: usize = 65_536;
const MAX_CALL_SITE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GenericCallSite {
    pub context: String,
    pub start: usize,
    pub end: usize,
    pub callee: String,
}

impl GenericCallSite {
    pub(crate) fn new(context: &str, callee: &str, span: Span) -> Self {
        Self {
            context: context.to_string(),
            start: span.start.offset,
            end: span.end.offset,
            callee: callee.to_string(),
        }
    }
}

#[derive(Clone)]
struct Request {
    source: String,
    symbol: String,
    bindings: HashMap<String, Type>,
    arguments: Vec<Type>,
    depth: usize,
    span: Span,
}

#[derive(Default)]
pub(super) struct GenericState {
    pub context: String,
    pub bindings: HashMap<String, Type>,
    depth: usize,
    pending: VecDeque<Request>,
    instances: BTreeMap<String, Request>,
    calls: BTreeMap<GenericCallSite, String>,
    unresolved_calls: BTreeMap<GenericCallSite, Span>,
    call_bytes: usize,
    ambiguous_calls: bool,
    nodes: usize,
    bytes: usize,
}

pub(crate) struct NativeGenericInstance {
    pub source: String,
    pub symbol: String,
    pub bindings: BTreeMap<String, TypeName>,
    pub parameters: Vec<TypeName>,
    pub returns: TypeName,
}

pub(crate) struct NativeSpecializationPlan {
    pub instances: Vec<NativeGenericInstance>,
    pub calls: BTreeMap<GenericCallSite, String>,
}

fn limit(span: Span, detail: &str) -> KuError {
    KuError::runtime(
        format!("generic specialization limit exceeded: {detail}"),
        span,
    )
}

fn reserve_call_site(count: usize, used: usize, bytes: usize, span: Span) -> KuResult<usize> {
    if count >= MAX_CALL_SITES || used > MAX_CALL_SITE_BYTES || bytes > MAX_CALL_SITE_BYTES - used {
        return Err(limit(span, "call site count or bytes"));
    }
    Ok(used + bytes)
}

fn key_name(name: &str, output: &mut String, span: Span) -> KuResult<()> {
    // Reserve the length prefix and separators before appending user text.
    if name.len() > MAX_KEY_BYTES
        || output.len() > MAX_KEY_BYTES - name.len()
        || output.len() + name.len() > MAX_KEY_BYTES - 32
    {
        return Err(limit(span, "type key bytes"));
    }
    output.push_str(&format!("{}:{name}", name.len()));
    Ok(())
}

fn concrete_key(ty: &Type, output: &mut String, depth: usize, span: Span) -> KuResult<bool> {
    if depth > MAX_TYPE_DEPTH {
        return Err(limit(span, "type depth"));
    }
    match ty {
        Type::Generic(_) | Type::Unknown => return Ok(false),
        Type::Int => output.push('i'),
        Type::Float => output.push('f'),
        Type::Bool => output.push('b'),
        Type::String => output.push('s'),
        Type::Null => output.push('n'),
        Type::Void => output.push('v'),
        Type::Array(inner) | Type::Result(inner) | Type::Task(inner) => {
            output.push(match ty {
                Type::Array(_) => 'a',
                Type::Result(_) => 'r',
                _ => 't',
            });
            if !concrete_key(inner, output, depth + 1, span)? {
                return Ok(false);
            }
            output.push(';');
        }
        Type::Struct(name) | Type::Enum(name) | Type::Native(name) => {
            output.push(match ty {
                Type::Struct(_) => 'S',
                Type::Enum(_) => 'E',
                _ => 'N',
            });
            key_name(name, output, span)?;
            output.push(';');
        }
        Type::StringMap => output.push('m'),
        Type::DynamicObject => output.push('d'),
        Type::KuValue => output.push('k'),
        Type::Object(fields) => {
            output.push('o');
            if fields.len() > MAX_KEY_BYTES {
                return Err(limit(span, "type key field count"));
            }
            let sorted = fields.iter().collect::<BTreeMap<_, _>>();
            for (name, ty) in sorted {
                key_name(name, output, span)?;
                output.push('=');
                if !concrete_key(ty, output, depth + 1, span)? {
                    return Ok(false);
                }
                if output.len() > MAX_KEY_BYTES {
                    return Err(limit(span, "type key bytes"));
                }
            }
            output.push(';');
        }
        Type::Union(types) => {
            output.push('u');
            for ty in types {
                if !concrete_key(ty, output, depth + 1, span)? {
                    return Ok(false);
                }
            }
            output.push(';');
        }
        Type::FunctionValue {
            params,
            return_type,
            is_async,
            ..
        } => {
            output.push(if *is_async { 'F' } else { 'c' });
            for param in params {
                output.push(if param.mode == ParamMode::View {
                    '&'
                } else {
                    '='
                });
                let Some(ty) = &param.ty else {
                    return Ok(false);
                };
                if !concrete_key(ty, output, depth + 1, span)? {
                    return Ok(false);
                }
            }
            output.push(':');
            let Some(ty) = return_type else {
                return Ok(false);
            };
            if !concrete_key(ty, output, depth + 1, span)? {
                return Ok(false);
            }
            output.push(';');
        }
    }
    if output.len() > MAX_KEY_BYTES {
        return Err(limit(span, "type key bytes"));
    }
    Ok(true)
}

impl Checker {
    fn record_generic_site(
        &mut self,
        name: &str,
        span: Span,
        symbol: Option<&str>,
    ) -> KuResult<()> {
        // The temporary lookup key is bounded before allocating either string.
        if name.len() > MAX_KEY_BYTES || self.generic_state.context.len() > MAX_KEY_BYTES + 128 {
            return Err(limit(span, "call site name bytes"));
        }
        let site = GenericCallSite::new(&self.generic_state.context, name, span);
        let known = self.generic_state.calls.contains_key(&site)
            || self.generic_state.unresolved_calls.contains_key(&site);
        if !known {
            let bytes = std::mem::size_of::<GenericCallSite>()
                + std::mem::size_of::<String>()
                + std::mem::size_of::<Span>()
                + site.context.len()
                + 2 * site.callee.len()
                + 128;
            self.generic_state.call_bytes = reserve_call_site(
                self.generic_state.calls.len() + self.generic_state.unresolved_calls.len(),
                self.generic_state.call_bytes,
                bytes,
                span,
            )?;
        }
        match symbol {
            Some(symbol) => {
                // Rechecking an untyped callback may encounter different actual
                // types at the same source site. Never silently choose one.
                if self.generic_state.unresolved_calls.contains_key(&site) {
                    self.generic_state.ambiguous_calls = true;
                }
                if let Some(previous) = self.generic_state.calls.get(&site) {
                    self.generic_state.ambiguous_calls |= previous != symbol;
                } else if !self.generic_state.unresolved_calls.contains_key(&site) {
                    self.generic_state.calls.insert(site, symbol.to_owned());
                }
            }
            None => {
                if self.generic_state.calls.contains_key(&site) {
                    self.generic_state.ambiguous_calls = true;
                } else {
                    self.generic_state.unresolved_calls.insert(site, span);
                }
            }
        }
        Ok(())
    }

    pub(super) fn record_generic_call(
        &mut self,
        name: &str,
        function: &FunctionType,
        bindings: &HashMap<String, Type>,
        arguments: &[Type],
        span: Span,
    ) -> KuResult<()> {
        if name.len() > MAX_KEY_BYTES {
            return Err(limit(span, "declaration name bytes"));
        }
        let mut key = format!("{}:{name}:", name.len());
        for parameter in &function.type_params {
            let Some(ty) = bindings.get(parameter) else {
                return self.record_generic_site(name, span, None);
            };
            if !concrete_key(ty, &mut key, 0, span)? {
                return self.record_generic_site(name, span, None);
            }
        }
        // Parameter modes are fixed by this lexical declaration. Function-valued
        // type arguments additionally encode every nested callback slot mode.
        let symbol = format!(
            "__ku_ns_generic_{name}_{:x}",
            Sha256::digest(key.as_bytes())
        );
        self.record_generic_site(name, span, Some(&symbol))?;
        if self.generic_state.instances.contains_key(&key) {
            return Ok(());
        }
        if self.generic_state.instances.len() >= MAX_INSTANCES {
            return Err(limit(span, "instance count"));
        }
        let depth = self.generic_state.depth + 1;
        if depth > MAX_INSTANCE_DEPTH {
            return Err(limit(span, "instance depth"));
        }
        let (nodes, bytes) = budget::instance_cost(function, bindings, arguments, span)?;
        if nodes > MAX_INSTANCE_BODY_NODES - self.generic_state.nodes
            || bytes > MAX_INSTANCE_BODY_BYTES - self.generic_state.bytes
        {
            return Err(limit(span, "expanded function body size"));
        }
        self.generic_state.nodes += nodes;
        self.generic_state.bytes += bytes;
        let request = Request {
            source: name.to_string(),
            symbol,
            bindings: bindings.clone(),
            arguments: arguments.to_vec(),
            depth,
            span,
        };
        self.generic_state.instances.insert(key, request.clone());
        self.generic_state.pending.push_back(request);
        Ok(())
    }

    pub(super) fn check_generic_instances(&mut self) -> KuResult<()> {
        while let Some(request) = self.generic_state.pending.pop_front() {
            let function = self
                .functions
                .get(&request.source)
                .cloned()
                .ok_or_else(|| {
                    KuError::runtime(
                        "generic declaration disappeared during specialization",
                        request.span,
                    )
                })?;
            let params = function
                .value_params
                .iter()
                .map(|param| FunctionValueParam {
                    name: param.name.clone(),
                    mode: param.mode,
                    ty: param
                        .ty
                        .as_ref()
                        .map(|ty| substitute_generics(ty, &request.bindings)),
                })
                .collect::<Vec<_>>();
            let returns = function
                .return_type
                .as_ref()
                .map(|ty| substitute_generics(ty, &request.bindings));
            let old_bindings =
                std::mem::replace(&mut self.generic_state.bindings, request.bindings.clone());
            let old_context = std::mem::replace(&mut self.generic_state.context, request.symbol);
            let old_depth = std::mem::replace(&mut self.generic_state.depth, request.depth);
            let saved_async = self.async_depth;
            self.async_depth = usize::from(function.is_async);
            let result = self.check_function_value_body(
                &params,
                returns.as_ref(),
                &function.body,
                Some(function.body_id),
                &request.arguments,
                request.span,
            );
            self.async_depth = saved_async;
            self.generic_state.bindings = old_bindings;
            self.generic_state.context = old_context;
            self.generic_state.depth = old_depth;
            result?;
        }
        Ok(())
    }
}

fn native_type(ty: &Type, span: Span, depth: usize) -> KuResult<TypeName> {
    if depth > MAX_TYPE_DEPTH {
        return Err(limit(span, "native type depth"));
    }
    Ok(match ty {
        Type::Int => TypeName::Int,
        Type::Float => TypeName::Float,
        Type::Bool => TypeName::Bool,
        Type::String => TypeName::String,
        Type::Null | Type::Void => TypeName::Null,
        Type::Array(inner) => TypeName::Array(Box::new(native_type(inner, span, depth + 1)?)),
        Type::Result(inner) => TypeName::Result(Box::new(native_type(inner, span, depth + 1)?)),
        Type::Struct(name) | Type::Enum(name) => TypeName::Custom(name.clone()),
        Type::FunctionValue {
            params,
            return_type,
            is_async: false,
            ..
        } => TypeName::Function {
            params: params
                .iter()
                .map(|param| {
                    native_type(
                        param.ty.as_ref().ok_or_else(|| {
                            KuError::runtime(
                                "native generic callback requires concrete parameter types",
                                span,
                            )
                        })?,
                        span,
                        depth + 1,
                    )
                })
                .collect::<KuResult<_>>()?,
            param_modes: params.iter().map(|param| param.mode).collect(),
            return_type: Box::new(native_type(
                return_type.as_deref().ok_or_else(|| {
                    KuError::runtime(
                        "native generic callback requires a concrete return type",
                        span,
                    )
                })?,
                span,
                depth + 1,
            )?),
            is_async: false,
        },
        _ => {
            return Err(KuError::runtime(
                format!(
                    "native generic specialization does not support type '{}' yet",
                    type_name(ty)
                ),
                span,
            ))
        }
    })
}

pub(crate) fn native_specialization_plan(program: &Program) -> KuResult<NativeSpecializationPlan> {
    let mut checker = Checker::new();
    checker.check_program(program)?;
    checker.check_generic_instances()?;
    if checker.generic_state.ambiguous_calls {
        return Err(KuError::message("native generic specialization cannot resolve a polymorphic or ambiguous expression call site"));
    }
    for (site, span) in &checker.generic_state.unresolved_calls {
        let is_template_context = checker
            .functions
            .get(&site.context)
            .is_some_and(|function| !function.type_params.is_empty());
        if !is_template_context {
            return Err(KuError::runtime(
                format!(
                    "native generic specialization cannot resolve concrete arguments for '{}'",
                    site.callee
                ),
                *span,
            ));
        }
    }
    let mut instances = Vec::with_capacity(checker.generic_state.instances.len());
    for request in checker.generic_state.instances.values() {
        let function = &checker.functions[&request.source];
        let bindings = request
            .bindings
            .iter()
            .map(|(name, ty)| Ok((name.clone(), native_type(ty, request.span, 0)?)))
            .collect::<KuResult<BTreeMap<_, _>>>()?;
        let parameters = function
            .params
            .iter()
            .map(|ty| native_type(&substitute_generics(ty, &request.bindings), request.span, 0))
            .collect::<KuResult<_>>()?;
        let returns = native_type(
            &substitute_generics(&function.returns, &request.bindings),
            request.span,
            0,
        )?;
        instances.push(NativeGenericInstance {
            source: request.source.clone(),
            symbol: request.symbol.clone(),
            bindings,
            parameters,
            returns,
        });
    }
    Ok(NativeSpecializationPlan {
        instances,
        calls: checker.generic_state.calls,
    })
}

pub(crate) fn native_local_generic_span(body: &[Stmt], span: Span) -> KuResult<Option<Span>> {
    budget::local_generic_span(body, span)
}

// AST/type payload accounting is shared with admission before any request clone.
mod budget;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_call_site_ledger_bounds_count_bytes_and_deduplicates_reads() {
        let span = Span::default();
        assert!(reserve_call_site(MAX_CALL_SITES, 0, 1, span).is_err());
        assert!(reserve_call_site(0, MAX_CALL_SITE_BYTES, 1, span).is_err());
        assert_eq!(
            reserve_call_site(MAX_CALL_SITES - 1, MAX_CALL_SITE_BYTES - 1, 1, span).unwrap(),
            MAX_CALL_SITE_BYTES
        );
        let mut checker = Checker::new();
        checker.generic_state.context = "main".into();
        checker
            .record_generic_site("Identity", span, Some("instance_one"))
            .unwrap();
        let bytes = checker.generic_state.call_bytes;
        checker
            .record_generic_site("Identity", span, Some("instance_one"))
            .unwrap();
        assert_eq!(checker.generic_state.call_bytes, bytes);
        assert_eq!(checker.generic_state.calls.len(), 1);
        assert!(!checker.generic_state.ambiguous_calls);
        checker
            .record_generic_site("Identity", span, Some("instance_two"))
            .unwrap();
        assert!(checker.generic_state.ambiguous_calls);
    }

    #[test]
    fn generic_unresolved_site_is_retained_and_cannot_silently_choose_an_instance() {
        let mut checker = Checker::new();
        let span = Span::default();
        checker.generic_state.context = "main".into();
        checker.record_generic_site("Identity", span, None).unwrap();
        checker
            .record_generic_site("Identity", span, Some("instance_one"))
            .unwrap();
        assert!(checker.generic_state.ambiguous_calls);
        assert_eq!(checker.generic_state.unresolved_calls.len(), 1);
        assert!(checker.generic_state.calls.is_empty());
    }
}
