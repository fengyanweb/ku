//! Generated, internal R4 typed factories over the existing R1/R2/R3 contracts.
//!
//! The bounded source Task path uses these adapters, without a manual Task API. A caller
//! exclusively owns each handle/header it passes, and every deep Owned buffer
//! remains valid, uniquely owned and disjoint from other live allocations. We
//! validate representable ranges and header aliases, not arbitrary raw pointers.
//! Source Start/Await and final scope drain reuse the fixed driver registration.
//! Active result charge follows successful hosted takes. Error-field projections
//! opt into typed active-byte reconciliation; other dynamic allocators and total
//! RSS accounting remain outside this implementation.

use crate::error::KuResult;
use crate::ir::task::{TaskFunction, TaskOp, TaskProgram, TaskSlotType, TaskTerminator};
use crate::ir::IrType;

use super::output::COutput;
use super::task::empty_value_expr;
use super::{c_move_value, c_type, c_type_suffix, unsupported};

#[path = "c_task_host.rs"]
mod host;

pub(super) fn emit_host(out: &mut COutput, tasks: &TaskProgram) -> KuResult<()> {
    host::emit(out, tasks)
}
pub(super) fn outcome_field(ty: &IrType) -> KuResult<&'static str> {
    host::field(ty)
}

pub(super) fn emit_adapters(out: &mut COutput, tasks: &TaskProgram) -> KuResult<()> {
    if tasks.functions.is_empty() {
        return Ok(());
    }
    out.check()?;
    out.push_str(ADAPTER_ABI);
    if tasks.functions.iter().any(|function|crate::ir::task::http_type(&function.result)
        || function.slots.iter().any(|slot|matches!(&slot.ty,TaskSlotType::Value{ty,..} if crate::ir::task::http_type(ty)))) {
        out.push_str(HTTP_ADAPTER_ABI);
    }
    if tasks.functions.iter().any(|function| {
        function.states.iter().any(|state| {
            state
                .operations
                .iter()
                .any(|operation| matches!(operation, TaskOp::FinallyEnter { .. }))
        })
    }) {
        out.push_str(FINALLY_CLOCK);
    }
    for function in &tasks.functions {
        out.check()?;
        emit_function(out, function)?;
    }
    emit_value_dispatch(out, tasks)?;
    out.check()
}

fn emit_value_dispatch(out: &mut COutput, tasks: &TaskProgram) -> KuResult<()> {
    out.push_str("static uint32_t ku_task_value_check(KuTaskValueV1* value) {\n  if (!ku_task_frame_storage_valid(value,sizeof(*value),sizeof(*value),KU_TASK_FRAME_ALIGNOF(KuTaskValueV1))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n  if (value->result_kind<1u || value->result_kind>8u) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n  if (value->tag==KU_TASK_VALUE_INLINE_FAILED) return !value->owner.lease.control && !value->ticket.driver ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_INVALID_STATE;\n  if (value->tag!=KU_TASK_VALUE_LIVE) return KU_TASK_DRIVER_INVALID_STATE;\n  uint32_t checked=ku_task_control_check_lease(&value->owner.lease);\n  if (checked!=KU_TASK_CONTROL_OK) return checked;\n  KuTaskControlV1* control=value->owner.lease.control;\n  switch(value->function_id) {\n");
    for function in &tasks.functions {
        out.check()?;
        let id = function.id.0;
        out.push_str(&format!("  case {id}: if (value->result_kind!={}u || control->context!=(void*)control || control->operations.resume!=ku_task_{id}_resume || control->operations.take_payload!=ku_task_{id}_take_payload) return KU_TASK_DRIVER_INVALID_ARGUMENT; break;\n",host::kind(&function.result)?));
    }
    out.push_str("  default: return KU_TASK_DRIVER_INVALID_ARGUMENT;\n  }\n  checked=ku_task_driver_check_ticket(&value->ticket);\n  if (checked!=KU_TASK_DRIVER_OK) return checked;\n  KuTaskDriverV1* driver=value->ticket.driver;\n  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;\n  KuTaskDriverSlotV1* slot=ku_task_driver_find(&value->ticket);\n  checked=slot && slot->binding==control ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_STALE;\n  ku_task_driver_unlock(driver);\n  return checked;\n}\n");
    out.check()
}

fn supported_type(ty: &IrType) -> bool {
    if crate::ir::task::error_type(ty)
        || crate::ir::task::http_type(ty)
        || crate::ir::task::local_pg_type(ty)
    {
        return true;
    }
    match ty {
        IrType::Int | IrType::Bool | IrType::Null | IrType::Str => true,
        IrType::Result(inner) => {
            matches!(
                **inner,
                IrType::Int | IrType::Bool | IrType::Null | IrType::Str
            ) || crate::ir::task::flat_array(inner)
        }
        _ => false,
    }
}

fn owned(ty: &IrType) -> bool {
    matches!(ty, IrType::Str | IrType::Result(_))
        || crate::ir::task::flat_array(ty)
        || crate::ir::task::pg_params_array(ty)
        || crate::ir::task::error_type(ty)
        || crate::ir::task::local_bytes_type(ty)
        || crate::ir::task::local_net_type(ty)
        || crate::ir::task::http_type(ty)
        || crate::ir::task::local_pg_type(ty)
}

// The generated branch itself guards the access: inactive Result fields must
// not be validated, charged or dereferenced, even if their bytes look unusual.
fn active_strings(ty: &IrType, place: &str, body: &impl Fn(&str) -> String) -> String {
    if crate::ir::task::http_request_type(ty) || crate::ir::task::http_response_type(ty) {
        let fields = if crate::ir::task::http_request_type(ty) {
            ["method", "path", "body"]
        } else {
            ["content_type", "body", "location"]
        };
        return fields
            .iter()
            .map(|name| body(&format!("({place}).{name}")))
            .collect();
    }
    match ty {
        IrType::Str => body(place),
        IrType::Array(inner) if inner.as_ref() == &IrType::Str => format!(
            "  for (size_t array_index=0;array_index<({place}).len;array_index++) {{\n{}  }}\n",
            body(&format!("({place}).data[array_index]"))
        ),
        IrType::Array(_) if crate::ir::task::pg_params_array(ty) => format!(
            "  for (size_t array_index=0;array_index<({place}).len;array_index++) {{ if (({place}).data[array_index].tag==KU_STR) {{\n{}  }} }}\n",
            body(&format!("({place}).data[array_index].as.s"))
        ),
        IrType::Result(inner) => format!(
            "  if (({place}).ok) {{\n{}  }} else {{\n{}{}{}  }}\n",
            active_strings(inner, &format!("({place}).value"), body),
            body(&format!("({place}).error.domain")),
            body(&format!("({place}).error.code")),
            body(&format!("({place}).error.message")),
        ),
        ty if crate::ir::task::error_type(ty) => format!(
            "{}{}{}",
            body(&format!("({place}).domain")),
            body(&format!("({place}).code")),
            body(&format!("({place}).message"))
        ),
        _ => String::new(),
    }
}

// Charging is separate from pure leaf traversal: vector bytes are counted
// once, while alias callbacks must never mutate any accounting variable.
fn active_charge(ty: &IrType, place: &str, total: &str) -> String {
    if matches!(ty, IrType::Named(name) if name == "__ku_pg_result") {
        return format!("  {{ size_t pg_bytes=0; if(!ku_pg_result_owned_bytes({place},&pg_bytes)) return KU_TASK_DRIVER_INVALID_ARGUMENT; if(pg_bytes>SIZE_MAX-{total}) return KU_TASK_DRIVER_LIMIT; {total}+=pg_bytes; }}\n");
    }
    if let IrType::Result(inner) = ty {
        return format!(
            "  if (({place}).ok) {{\n{}  }} else {{\n{}  }}\n",
            active_charge(inner, &format!("({place}).value"), total),
            ["domain", "code", "message"].iter().map(|field| format!("  checked=ku_task_adapter_string_charge(({place}).error.{field},&{total}); if (checked!=KU_TASK_DRIVER_OK) return checked;\n")).collect::<String>()
        );
    }
    if matches!(ty, IrType::Named(name) if name == "__ku_bytes") {
        return format!("  checked=ku_task_adapter_string_charge((KuString){{({place}).ptr,({place}).len,({place}).capacity,({place}).storage}},&{total}); if (checked!=KU_TASK_DRIVER_OK) return checked;\n");
    }
    if matches!(ty, IrType::Named(name) if name == "__ku_net_client") {
        return format!("  if ({place}) {{ if (sizeof(KuNetClient)>SIZE_MAX-{total}) return KU_TASK_DRIVER_LIMIT; {total}+=sizeof(KuNetClient); }}\n");
    }
    if crate::ir::task::http_request_type(ty) {
        let leaves = active_strings(ty, place, &|leaf| {
            format!("  checked=ku_task_adapter_string_charge({leaf},&{total}); if(checked!=KU_TASK_DRIVER_OK) return checked;\n")
        });
        let maps=["params","query","headers"].iter().map(|name|format!("  checked=ku_task_adapter_http_map_charge(({place}).{name},&{total}); if(checked!=KU_TASK_DRIVER_OK) return checked;\n")).collect::<String>();
        return leaves + &maps;
    }
    if let IrType::Array(element) = ty {
        let c_element = super::c_type(element).expect("flat element C type");
        let leaves = active_strings(ty, place, &|leaf| {
            format!("  checked=ku_task_adapter_string_charge({leaf},&{total}); if (checked!=KU_TASK_DRIVER_OK) return checked;\n")
        });
        let tags = if crate::ir::task::pg_params_array(ty) {
            format!("  for (size_t index=0;index<({place}).len;index++) {{ uint8_t tag=({place}).data[index].tag; if(tag!=KU_NULL && tag!=KU_INT && tag!=KU_BOOL && tag!=KU_STR) return KU_TASK_DRIVER_INVALID_ARGUMENT; }}\n")
        } else {
            String::new()
        };
        return format!("  {{ KuTaskAdapterBufferV1 vector={{0}}; checked=ku_task_adapter_array_buffer(({place}).data,({place}).len,({place}).capacity,sizeof({c_element}),KU_TASK_FRAME_ALIGNOF({c_element}),&vector); if (checked!=KU_TASK_DRIVER_OK) return checked;\n  if (vector.bytes>SIZE_MAX-{total}) return KU_TASK_DRIVER_LIMIT; {total}+=vector.bytes;\n{tags}{leaves}  }}\n");
    }
    active_strings(ty, place, &|leaf| {
        format!("  checked=ku_task_adapter_string_charge({leaf},&{total}); if (checked!=KU_TASK_DRIVER_OK) return checked;\n")
    })
}

// Each take stage excludes every header address known at that point before
// reading string leaves. The first stage protects the request fields themselves.
fn result_aliases(
    ty: &IrType,
    place: &str,
    extra: &str,
    extra_count: usize,
    body: &impl Fn(&str) -> String,
) -> KuResult<String> {
    if let IrType::Result(inner) = ty {
        if let IrType::Array(element) = inner.as_ref() {
            let c_element = c_type(element)?;
            let error_checks = ["domain", "code", "message"]
                .iter()
                .map(|field| body(&format!("({place}).error.{field}")))
                .collect::<String>();
            return Ok(format!(
                "  if (({place}).ok) {{\n    uint32_t alias_checked=ku_task_adapter_array_excludes(instance->ticket.driver,({place}).value.data,({place}).value.len,({place}).value.capacity,sizeof({c_element}),KU_TASK_FRAME_ALIGNOF({c_element}),{strings},{extra},{extra_count}u);\n    if (alias_checked!=KU_TASK_DRIVER_OK) return KU_TASK_CONTROL_INVALID_ARGUMENT;\n  }} else {{\n{error_checks}  }}\n",
                strings = usize::from(element.as_ref() == &IrType::Str)
            ));
        }
    }
    Ok(active_strings(ty, place, body))
}

pub(super) fn parameter_c_type(ty: &TaskSlotType) -> KuResult<String> {
    match ty {
        TaskSlotType::Value {
            ty,
            borrowed: false,
        } => c_type(ty),
        TaskSlotType::Callable { .. } => Ok("uint32_t".into()),
        _ => Err(unsupported(
            "native Task parameter is not an owned Value or callable",
        )),
    }
}

pub(super) fn parameter_move(ty: &TaskSlotType, place: &str) -> KuResult<String> {
    match ty {
        TaskSlotType::Value {
            ty,
            borrowed: false,
        } => c_move_value(ty, place),
        TaskSlotType::Callable { .. } => Ok(format!("ku_task_callable_move(&{place})")),
        _ => Err(unsupported("unsupported native Task parameter move")),
    }
}

fn parameter_owned(ty: &TaskSlotType) -> bool {
    match ty {
        TaskSlotType::Value { ty, .. } => owned(ty),
        TaskSlotType::Callable { .. } => true,
        _ => false,
    }
}

fn parameter_strings(ty: &TaskSlotType, place: &str, body: &impl Fn(&str) -> String) -> String {
    match ty {
        TaskSlotType::Value { ty, .. } => active_strings(ty, place, body),
        _ => String::new(),
    }
}

fn parameter_drop(ty: &TaskSlotType, place: &str) -> KuResult<String> {
    match ty {
        TaskSlotType::Value { ty, .. } => super::task::drop_statement(ty, place),
        TaskSlotType::Callable { .. } => Ok(format!("{place}=0;")),
        _ => Err(unsupported("unsupported native Task parameter drop")),
    }
}

// Parameter arrays have one fixed vector layer, optionally behind one Result.
// The bool describes the generated active-branch guard, not another traversal.
fn parameter_array(ty: &TaskSlotType) -> Option<(&IrType, bool)> {
    let TaskSlotType::Value {
        ty,
        borrowed: false,
    } = ty
    else {
        return None;
    };
    match ty {
        IrType::Array(element) if crate::ir::task::flat_array(ty) => Some((element, false)),
        IrType::Result(inner) if crate::ir::task::flat_array(inner) => {
            let IrType::Array(element) = inner.as_ref() else {
                unreachable!()
            };
            Some((element, true))
        }
        _ => None,
    }
}

fn array_input_guard(
    id: &str,
    parameters: &[(usize, &TaskSlotType)],
    declarations: &str,
) -> KuResult<String> {
    let range_capacity = parameters
        .iter()
        .map(|(_, ty)| match ty {
            TaskSlotType::Value {
                ty: IrType::Result(_),
                ..
            } => 3,
            TaskSlotType::Value {
                ty: IrType::Str | IrType::Array(_),
                ..
            } => 1,
            TaskSlotType::Value { ty, .. }
                if crate::ir::task::error_type(ty) || crate::ir::task::http_type(ty) =>
            {
                3
            }
            _ => 0,
        })
        .sum::<usize>()
        .max(1);
    let vector_capacity = parameters
        .iter()
        .filter(|(_, ty)| parameter_array(ty).is_some_and(|(element, _)| element == &IrType::Str))
        .count()
        .max(1);
    let mut code=format!("static uint32_t ku_task_{id}_inputs(KuTaskDriverV1* driver,const KuTaskAdapterBufferV1* extra,size_t extra_count,int read_leaves{declarations},size_t* owned_bytes) {{\n  KuTaskAdapterBufferV1 base[{range_capacity}]={{0}},headers[{}]={{0}};\n  KuTaskAdapterStringVectorV1 vectors[{vector_capacity}]={{0}};\n  size_t base_count=0,vector_count=0,total=0; uint32_t checked=KU_TASK_DRIVER_OK;\n",parameters.len().max(1));
    for index in 0..parameters.len() {
        code.push_str(&format!(
            "  headers[{index}]=(KuTaskAdapterBufferV1){{arg_{index},sizeof(*arg_{index}),0}};\n"
        ));
    }
    let validate_base=format!("  checked=ku_task_adapter_buffer_excludes(base[base_count-1u],headers,{}u,extra,extra_count,driver); if (checked!=KU_TASK_DRIVER_OK) return checked;\n  for (size_t prior=0;prior+1u<base_count;prior++) if (ku_task_adapter_buffers_overlap(base[base_count-1u],base[prior])) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n",parameters.len());
    for (index, (_, ty)) in parameters.iter().enumerate() {
        let register_string = |leaf: &str| {
            format!("  checked=ku_task_adapter_string_charge({leaf},&total); if (checked!=KU_TASK_DRIVER_OK) return checked;\n  base[base_count++]=ku_task_adapter_string_buffer({leaf});\n{validate_base}")
        };
        if let Some((element, result)) = parameter_array(ty) {
            let element_c = c_type(element)?;
            let place = if result {
                code.push_str(&format!("  if (arg_{index}->ok) {{\n"));
                format!("arg_{index}->value")
            } else {
                format!("(*arg_{index})")
            };
            // Read only the active vector header. Its complete allocation is
            // excluded from known ranges before any string leaf is inspected.
            code.push_str(&format!("  checked=ku_task_adapter_array_buffer(({place}).data,({place}).len,({place}).capacity,sizeof({element_c}),KU_TASK_FRAME_ALIGNOF({element_c}),&base[base_count]); if (checked!=KU_TASK_DRIVER_OK) return checked;\n  if (base[base_count].bytes>SIZE_MAX-total) return KU_TASK_DRIVER_LIMIT; total+=base[base_count++].bytes;\n"));
            code.push_str(&validate_base);
            if element == &IrType::Str {
                code.push_str(&format!("  vectors[vector_count++]=(KuTaskAdapterStringVectorV1){{({place}).data,({place}).len}};\n"));
            }
            if result {
                code.push_str("  } else {\n");
                for field in ["domain", "code", "message"] {
                    code.push_str(&register_string(&format!("arg_{index}->error.{field}")));
                }
                code.push_str("  }\n");
            }
        } else {
            code.push_str(&parameter_strings(
                ty,
                &format!("(*arg_{index})"),
                &register_string,
            ));
        }
        if matches!(ty,TaskSlotType::Value{ty,..} if crate::ir::task::http_request_type(ty)) {
            for field in ["params", "query", "headers"] {
                code.push_str(&format!("  checked=ku_task_adapter_http_map_charge(arg_{index}->{field},&total); if(checked!=KU_TASK_DRIVER_OK) return checked;\n"));
            }
        }
    }
    code.push_str(&format!("  for (size_t i=0;i<base_count;i++) {{\n    checked=ku_task_adapter_buffer_excludes(base[i],headers,{}u,extra,extra_count,driver); if (checked!=KU_TASK_DRIVER_OK) return checked;\n    for (size_t j=0;j<i;j++) if (ku_task_adapter_buffers_overlap(base[i],base[j])) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n  }}\n",parameters.len()));
    // The preliminary hosted pass knows parent/control ranges before their
    // fields can be read. Validate all leaves against those known ranges but
    // defer inter-leaf sorting until the final pass supplies the real driver
    // for its scratch reservation. No ownership/output mutation precedes it.
    code.push_str("  if (!read_leaves) return KU_TASK_DRIVER_OK;\n  size_t leaf_count=0;\n  for (size_t i=0;i<vector_count;i++) { if (vectors[i].len>640000u-leaf_count) return KU_TASK_DRIVER_LIMIT; leaf_count+=vectors[i].len; }\n  KuTaskAdapterBufferV1 local[64],*leaves=local; size_t scratch_bytes=0;\n  if (driver && leaf_count>64u) { checked=ku_task_adapter_scratch(driver,leaf_count,&leaves,&scratch_bytes); if (checked!=KU_TASK_DRIVER_OK) return checked; }\n  size_t at=0;\n  for (size_t vector=0;vector<vector_count && checked==KU_TASK_DRIVER_OK;vector++) {\n    for (size_t leaf=0;leaf<vectors[vector].len;leaf++) {\n      KuString value=vectors[vector].data[leaf];\n      checked=ku_task_adapter_string_charge(value,&total); if (checked!=KU_TASK_DRIVER_OK) break;\n      KuTaskAdapterBufferV1 buffer=ku_task_adapter_string_buffer(value);\n      checked=ku_task_adapter_buffer_excludes(buffer,headers,");
    code.push_str(&parameters.len().to_string());
    code.push_str("u,extra,extra_count,driver); if (checked!=KU_TASK_DRIVER_OK) break;\n      for (size_t i=0;i<base_count;i++) if (ku_task_adapter_buffers_overlap(buffer,base[i])) { checked=KU_TASK_DRIVER_INVALID_ARGUMENT; break; }\n      if (checked!=KU_TASK_DRIVER_OK) break;\n      if (driver) leaves[at++]=buffer;\n    }\n  }\n  if (driver && checked==KU_TASK_DRIVER_OK) checked=ku_task_adapter_ranges_disjoint(leaves,leaf_count);\n  if (scratch_bytes) checked=ku_task_adapter_scratch_free(driver,leaves,scratch_bytes,checked);\n  if (checked==KU_TASK_DRIVER_OK && owned_bytes) *owned_bytes=total; return checked;\n}\n");
    Ok(code)
}

fn emit_function(out: &mut COutput, function: &TaskFunction) -> KuResult<()> {
    let IrType::Result(inner) = &function.result else {
        return Err(unsupported(
            "native task adapter requires a supported Result",
        ));
    };
    if !supported_type(&function.result) {
        return Err(unsupported(
            "native task adapter requires a supported Result",
        ));
    }
    let mut parameters = Vec::with_capacity(function.parameters.len());
    for slot in &function.parameters {
        let Some(slot_type) = function.slots.get(slot.0) else {
            return Err(unsupported("native task adapter parameter slot is missing"));
        };
        match &slot_type.ty {
            TaskSlotType::Value {
                ty,
                borrowed: false,
            } if supported_type(ty) || crate::ir::task::flat_array(ty) => {}
            TaskSlotType::Callable { .. } => {}
            _ => return Err(unsupported("native task adapter parameter is unsupported")),
        }
        if slot.0 >= 64 {
            return Err(unsupported("native task adapter parameter is unsupported"));
        }
        parameters.push((slot.0, &slot_type.ty));
    }

    let id = function.id.0.to_string();
    let blocking = function.states.iter().any(|state| {
        matches!(
            state.terminator,
            TaskTerminator::BlockingWrite { .. }
                | TaskTerminator::BlockingRead { .. }
                | TaskTerminator::BlockingReadDir { .. }
                | TaskTerminator::BlockingExists { .. }
                | TaskTerminator::BlockingNet { .. }
                | TaskTerminator::PgQuery { .. }
        )
    });
    let dynamic_projection = blocking
        || crate::ir::task::array_result(&function.result) || crate::ir::task::http_type(&function.result)
        || function.slots.iter().any(|slot| matches!(&slot.ty,TaskSlotType::Value {ty,..} if crate::ir::task::flat_array(ty) || crate::ir::task::pg_params_array(ty) || crate::ir::task::array_result(ty) || crate::ir::task::http_type(ty)))
        || function.states.iter().any(|state| {
            state
                .operations
                .iter()
                .any(crate::ir::task::requires_string_allocation_host)
        })
        || function.states.iter().any(|state| {
            state.operations.windows(2).any(|pair| {
                crate::ir::task::replaces_owned_value(function, Some(&pair[0]), &pair[1])
            })
        });
    let mut reconcile_fields = if blocking {
        "  bytes=instance->blocking.charged_bytes;\n".to_owned()
    } else {
        String::new()
    };
    if dynamic_projection {
        reconcile_fields.push_str("  if (instance->frame_initialized) {\n");
        for (index, slot) in function.slots.iter().enumerate() {
            if let TaskSlotType::Value {
                ty,
                borrowed: false,
            } = &slot.ty
            {
                if owned(ty) {
                    reconcile_fields.push_str(&format!("  if (instance->frame.header.initialized & (UINT64_C(1)<<{index})) {{\n{}  }}\n",
                        active_charge(ty,&format!("instance->frame.s_{index}"),"bytes").replace("checked","status")));
                }
            }
        }
        reconcile_fields.push_str(&format!(
            "  if (instance->frame.header.result_initialized) {{\n{}  }}\n",
            active_charge(&function.result, "instance->frame.result", "bytes")
                .replace("checked", "status")
        ));
        reconcile_fields.push_str("  }\n");
        reconcile_fields.push_str(&format!(
            "  if (instance->payload_initialized) {{\n{}  }}\n",
            active_charge(&function.result, "instance->payload", "bytes")
                .replace("checked", "status")
        ));
    }
    let result_type = c_type(&function.result)?;
    let mut declarations = String::new();
    let mut arguments = String::new();
    let mut preflight = String::new();
    let mut callable_checks = String::new();
    let mut charge = String::new();
    let mut restore = String::new();
    let mut rejected_drops = String::new();
    let mut source_headers = String::new();
    let mut source_buffers = String::new();
    let mut hosted_headers = String::new();
    let mut hosted_charge = String::new();
    let mut hosted_ticket_aliases = String::new();
    for (index, (slot, ty)) in parameters.iter().enumerate() {
        let c_ty = parameter_c_type(ty)?;
        declarations.push_str(&format!(", {c_ty}* arg_{index}"));
        arguments.push_str(&format!(", arg_{index}"));
        preflight.push_str(&format!(
            "  if (!ku_task_driver_external_storage(driver, arg_{index}, sizeof(*arg_{index}), KU_TASK_FRAME_ALIGNOF({c_ty}))\n\
             || ku_task_frame_ranges_overlap(output, sizeof(*output), arg_{index}, sizeof(*arg_{index}))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n"
        ));
        for (other, (_, other_ty)) in parameters.iter().enumerate().take(index) {
            if parameter_owned(ty) || parameter_owned(other_ty) {
                preflight.push_str(&format!(
                    "  if (ku_task_frame_ranges_overlap(arg_{index}, sizeof(*arg_{index}), arg_{other}, sizeof(*arg_{other}))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n"
                ));
            }
        }
        charge.push_str(&parameter_strings(
            ty,
            &format!("(*arg_{index})"),
            &|place| {
                format!(
                    "  checked = ku_task_adapter_string_charge({place}, &charge);\n\
                 if (checked != KU_TASK_DRIVER_OK) return checked;\n"
                )
            },
        ));
        // Also prevent a move/handle publication from overwriting a buffer that
        // is being moved. This is cheap and bounded by the 64-slot IR limit.
        charge.push_str(&parameter_strings(ty, &format!("(*arg_{index})"), &|place| {
            let mut checks = format!(
                "  if (ku_task_adapter_string_overlaps({place}, output, sizeof(*output))\n\
                 || ku_task_adapter_string_overlaps_runtime({place}, driver)"
            );
            for other in 0..parameters.len() {
                checks.push_str(&format!(
                    "\n || ku_task_adapter_string_overlaps({place}, arg_{other}, sizeof(*arg_{other}))"
                ));
            }
            checks.push_str(") return KU_TASK_DRIVER_INVALID_ARGUMENT;\n");
            checks
        }));
        // Hosted entry cannot read host->ticket until every argument header
        // and active buffer has been proved disjoint from the complete host.
        hosted_headers.push_str(&format!(
            "  if (!ku_task_frame_storage_valid(arg_{index},sizeof(*arg_{index}),sizeof(*arg_{index}),KU_TASK_FRAME_ALIGNOF({c_ty}))\n\
             || ku_task_frame_ranges_overlap(host,sizeof(*host),arg_{index},sizeof(*arg_{index}))\n\
             || ku_task_frame_ranges_overlap(output,sizeof(*output),arg_{index},sizeof(*arg_{index}))\n\
             || ku_task_frame_ranges_overlap(&source,sizeof(source),arg_{index},sizeof(*arg_{index}))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n"
        ));
        for (other, (_, other_ty)) in parameters.iter().enumerate().take(index) {
            if parameter_owned(ty) || parameter_owned(other_ty) {
                hosted_headers.push_str(&format!(
                    "  if (ku_task_frame_ranges_overlap(arg_{index},sizeof(*arg_{index}),arg_{other},sizeof(*arg_{other}))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n"
                ));
            }
        }
        source_headers.push_str(&format!(
            "  if (source && ku_task_frame_ranges_overlap(source,sizeof(*source),arg_{index},sizeof(*arg_{index}))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n"
        ));
        hosted_ticket_aliases.push_str(&format!(
            "  if (ku_task_frame_ranges_overlap(parent,sizeof(*parent),arg_{index},sizeof(*arg_{index}))\n\
             || ku_task_frame_ranges_overlap(host->control,sizeof(KuTaskControlV1),arg_{index},sizeof(*arg_{index}))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n"
        ));
        hosted_charge.push_str(&parameter_strings(ty, &format!("(*arg_{index})"), &|place| {
            let mut checks = format!(
                "  checked=ku_task_adapter_string_charge({place},&charge);\n\
                 if (checked!=KU_TASK_DRIVER_OK) return checked;\n\
                 if (ku_task_adapter_string_overlaps({place},host,sizeof(*host))\n\
                 || ku_task_adapter_string_overlaps({place},output,sizeof(*output))\n\
                 || ku_task_adapter_string_overlaps({place},&source,sizeof(source))"
            );
            for other in 0..parameters.len() {
                checks.push_str(&format!(
                    "\n || ku_task_adapter_string_overlaps({place},arg_{other},sizeof(*arg_{other}))"
                ));
            }
            checks.push_str(") return KU_TASK_DRIVER_INVALID_ARGUMENT;\n");
            checks
        }));
        source_buffers.push_str(&parameter_strings(ty, &format!("(*arg_{index})"), &|place| {
            format!("  if (source && ku_task_adapter_string_overlaps({place},source,sizeof(*source))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n")
        }));
        hosted_ticket_aliases.push_str(&parameter_strings(ty, &format!("(*arg_{index})"), &|place| {
            format!(
                "  if (ku_task_adapter_string_overlaps({place},parent,sizeof(*parent))\n\
                 || ku_task_adapter_string_overlaps({place},host->control,sizeof(KuTaskControlV1))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n"
            )
        }));
        if matches!(ty, TaskSlotType::Callable { .. }) {
            let check = format!("  if (!ku_task_frame_{id}_callable_{slot}(*arg_{index})) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n");
            callable_checks.push_str(&check);
        }
        if parameter_owned(ty) {
            rejected_drops.push_str(&format!(
                "  {}\n",
                parameter_drop(ty, &format!("(*arg_{index})"))?
            ));
            restore.push_str(&format!(
                "  *arg_{index} = {};\n\
                 instance->frame.header.initialized &= ~(UINT64_C(1) << {slot});\n",
                parameter_move(ty, &format!("instance->frame.s_{slot}"))?
            ));
        }
    }
    let has_array_parameters = parameters.iter().any(|(_, ty)| {
        parameter_array(ty).is_some()
            || matches!(ty,TaskSlotType::Value{ty,..} if crate::ir::task::http_type(ty))
    });
    let mut hosted_runtime_inputs = String::new();
    if has_array_parameters {
        out.push_str(&array_input_guard(&id, &parameters, &declarations)?);
        charge=format!("  size_t input_charge=0;\n  checked=ku_task_{id}_inputs(driver,(KuTaskAdapterBufferV1[]){{{{output,sizeof(*output),0}},{{source,source ? sizeof(*source) : 0,0}}}},2u,1{arguments},&input_charge);\n  if (checked!=KU_TASK_DRIVER_OK) return checked;\n  if (input_charge>SIZE_MAX-charge) return KU_TASK_DRIVER_LIMIT; charge+=input_charge;\n");
        source_buffers.clear();
        hosted_charge=format!("  checked=ku_task_{id}_inputs(NULL,(KuTaskAdapterBufferV1[]){{{{host,sizeof(*host),0}},{{output,sizeof(*output),0}},{{&source,sizeof(source),0}}}},3u,0{arguments},NULL);\n  if (checked!=KU_TASK_DRIVER_OK) return checked;\n");
        hosted_ticket_aliases.clear();
        for index in 0..parameters.len() {
            hosted_ticket_aliases.push_str(&format!("  if (ku_task_frame_ranges_overlap(parent,sizeof(*parent),arg_{index},sizeof(*arg_{index})) || ku_task_frame_ranges_overlap(host->control,sizeof(KuTaskControlV1),arg_{index},sizeof(*arg_{index}))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n"));
        }
        let extra="(KuTaskAdapterBufferV1[]){{host,sizeof(*host),0},{output,sizeof(*output),0},{&source,sizeof(source),0},{parent,sizeof(*parent),0},{host->control,sizeof(KuTaskControlV1),0}}";
        hosted_ticket_aliases.push_str(&format!("  checked=ku_task_{id}_inputs(NULL,{extra},5u,1{arguments},NULL); if (checked!=KU_TASK_DRIVER_OK) return checked;\n"));
        hosted_runtime_inputs=format!("  checked=ku_task_{id}_inputs(driver,{extra},5u,1{arguments},&charge); if (checked!=KU_TASK_DRIVER_OK) return checked;\n");
    }
    let payload_aliases = result_aliases(&function.result, "instance->payload",
        "(KuTaskAdapterBufferV1[]){{instance,sizeof(*instance),0},{request,sizeof(*request),0},{outcome,sizeof(*outcome),0},{request->parent,request->parent ? sizeof(*request->parent) : 0,0}}", 4, &|place| {
        format!(
            "  if (ku_task_adapter_string_overlaps({place}, outcome, sizeof(*outcome)) || ku_task_adapter_string_overlaps({place}, request, sizeof(*request)) || (request->parent && ku_task_adapter_string_overlaps({place},request->parent,sizeof(*request->parent)))) return KU_TASK_CONTROL_INVALID_ARGUMENT;\n"
        )
    })?;
    let request_aliases = result_aliases(
        &function.result,
        "instance->payload",
        "(KuTaskAdapterBufferV1[]){{instance,sizeof(*instance),0},{request,sizeof(*request),0}}",
        2,
        &|place| {
            format!("  if (ku_task_adapter_string_overlaps({place},request,sizeof(*request))) return KU_TASK_CONTROL_INVALID_ARGUMENT;\n")
        },
    )?;
    let payload_charge = active_charge(&function.result, "instance->payload", "payload_charge");
    let mut task_mask = 0u64;
    let mut task_count = 0usize;
    let mut transfers = String::new();
    let scoped = function.states.iter().any(|state| {
        state
            .operations
            .iter()
            .any(|op| matches!(op, TaskOp::ScopeEnter { .. }))
    });
    let scope_exits = function.states.iter().any(|state| {
        state
            .operations
            .iter()
            .any(|operation| matches!(operation, TaskOp::ScopeExitBegin { .. }))
    });
    let finally = function.states.iter().any(|state| {
        state
            .operations
            .iter()
            .any(|op| matches!(op, TaskOp::FinallyEnter { .. }))
    });
    let compute_banks = !blocking
        && !finally
        && !function
            .slots
            .iter()
            .any(|slot| matches!(slot.ty, TaskSlotType::Value { borrowed: true, .. }));
    let mut exit_deadline = String::new();
    if scope_exits {
        exit_deadline.push_str(EXIT_DEADLINE);
    }
    if finally {
        exit_deadline.push_str(FINALLY_DEADLINE);
    }
    let empty_started = match (scope_exits, finally) {
        (true, true) => "(instance->drain_started && !instance->frame.scope_exit_active && !instance->frame.finally_unwind_active)",
        (true, false) => "(instance->drain_started && !instance->frame.scope_exit_active)",
        (false, true) => "(instance->drain_started && !instance->frame.finally_unwind_active)",
        (false, false) => "instance->drain_started",
    };
    let mut scope_preflight = String::new();
    for (index, slot) in function.slots.iter().enumerate() {
        if matches!(slot.ty, TaskSlotType::Task { .. }) {
            task_mask |= 1u64 << index;
            if scoped {
                scope_preflight.push_str(&format!(
                    "  if (instance->frame.header.initialized & task_selection & (UINT64_C(1)<<{index})) {{\n    KuTaskValueV1* child=&instance->frame.s_{index};\n    uint32_t checked=ku_task_value_check(child);\n    if (checked!=KU_TASK_DRIVER_OK) return checked;\n    if (child->tag==KU_TASK_VALUE_LIVE) {{\n      if ((instance->scope_issued_mask & (UINT64_C(1)<<{task_count})) || !ku_task_driver_scope_receipt_empty(&instance->receipts[{task_count}])) return KU_TASK_DRIVER_INVALID_STATE;\n      scope_live_mask |= UINT64_C(1)<<{task_count};\n    }}\n  }}\n"
                ));
            }
            let selection = if scoped { "task_selection & " } else { "" };
            let issued = if scoped {
                format!("      if (instance->scope_mode) instance->scope_issued_mask |= UINT64_C(1)<<{task_count};\n")
            } else {
                String::new()
            };
            transfers.push_str(&format!("  if (instance->frame.header.initialized & {selection}(UINT64_C(1)<<{index})) {{\n    KuTaskValueV1* child=&instance->frame.s_{index};\n    uint32_t moved=ku_task_value_check(child);\n    if (moved!=KU_TASK_DRIVER_OK) return moved;\n    if (child->tag==KU_TASK_VALUE_LIVE) {{\n      moved=ku_task_driver_owner_drop_receipt(&child->ticket,&child->owner,instance->drain_deadline,&instance->receipts[{task_count}]);\n      if (moved!=KU_TASK_DRIVER_OK) return moved;\n{issued}      instance->receipt_mask |= UINT64_C(1)<<{task_count};\n    }}\n    *child=(KuTaskValueV1){{0}};\n    instance->frame.header.initialized &= ~(UINT64_C(1)<<{index});\n  }}\n"));
            task_count += 1;
        }
    }
    // A function with no child Task slots cannot issue a cleanup receipt.
    // Keep its wait/timer and empty-scope state, but omit the unused span and
    // every C expression that would refer to it (including zero-trip loops).
    let has_children = task_count != 0;
    let source = ADAPTER_FUNCTION
        .replace("@BLOCKING_CANCEL@", if blocking { "  if (instance->blocking.pool && ku_task_blocking_cancel(&instance->ticket,&instance->blocking)!=KU_TASK_DRIVER_OK) return KU_TASK_CONTROL_INVALID_STATE;" } else { "" })
        .replace("@BLOCKING_FIELD@", if blocking { "  KuTaskBlockingTokenV1 blocking;" } else { "" })
        .replace("@BLOCKING_HOST@", if blocking { "&instance->blocking" } else { "NULL" })
        .replace("@BLOCKING_CLEANUP@", if blocking { BLOCKING_CLEANUP } else { "" })
        .replace("@RECONCILE_HELPER@", if dynamic_projection { RECONCILE_HELPER } else { "" })
        .replace("@RECONCILE_FIELDS@", &reconcile_fields)
        .replace("@RECONCILE_CALL@", if blocking { "ku_task_blocking_reconcile_charge(&instance->ticket,&instance->blocking,sizeof(*instance),bytes)" } else { "ku_task_driver_reconcile_charge(&instance->ticket,sizeof(*instance),bytes)" })
        .replace("@ALLOCATION_HOST@", if dynamic_projection { "sizeof(KuTaskAdapterHostV1),KU_TASK_ALLOCATION_HOST_ABI_VERSION,instance,ku_task_@ID@_reconcile_charge" } else { "0,0,NULL,NULL" })
        .replace("@RECONCILE_RETURN@", if dynamic_projection { RECONCILE_AFTER_RESUME } else { "" })
        .replace("@SCALAR_RECONCILE_SKIP@", if blocking || finally { "0" } else {
            "status==KU_TASK_FRAME_PENDING && instance->frame_initialized && !instance->payload_initialized\n    && !instance->frame.header.result_initialized && !(instance->frame.header.initialized & UINT64_C(@TASK_MASK@))\n    && !instance->receipt_mask && !instance->wait.driver && !instance->drain_started\n    && ku_task_driver_scalar_quantum(&instance->ticket,&instance->control)"
        })
        .replace("@COMPUTE_BANKS_ALLOWED@", if compute_banks {
            "instance->frame_initialized && !instance->payload_initialized\n    && !instance->frame.header.result_initialized && !(instance->frame.header.initialized & UINT64_C(@TASK_MASK@))\n    && !instance->receipt_mask && !instance->wait.driver && !instance->timer.driver && !instance->drain_started"
        } else { "0" })
        .replace("@RECONCILE_DRAIN@", if dynamic_projection { "if (ku_task_@ID@_reconcile_charge(instance)!=KU_TASK_DRIVER_OK) return KU_TASK_DRIVER_INTERNAL;" } else { "" })
        .replace("@RECONCILE_TAKE@", if dynamic_projection { "if (ku_task_@ID@_reconcile_charge(instance)!=KU_TASK_DRIVER_OK) ku_task_adapter_fault(instance->ticket.driver,0);" } else { "" })
        .replace("@RECONCILE_DROP@", if dynamic_projection { "if (ku_task_@ID@_reconcile_charge(instance)!=KU_TASK_DRIVER_OK) ku_task_adapter_fault(instance->ticket.driver,0);" } else { "" })
        .replace("@SCOPE_FIELDS@", if scoped { SCOPE_FIELDS } else { "" })
        .replace("@SCOPE_SELECT@", if scoped { SCOPE_SELECT } else { "" })
        .replace("@SCOPE_PREFLIGHT@", &scope_preflight)
        .replace("@SCOPE_REGISTER@", if scoped { SCOPE_REGISTER } else { "" })
        .replace("@SCOPE_RECEIPT_REGISTER@", if has_children { SCOPE_RECEIPT_REGISTER } else { "" })
        .replace("@SCOPE_RESUME_HELPER@", if scoped { SCOPE_RESUME_HELPER } else { "" })
        .replace("@RESET_RECEIPTS@", if has_children { "  memset(instance->receipts,0,sizeof(instance->receipts));" } else { "" })
        .replace("@SCOPE_RESUME@", if scoped { SCOPE_RESUME } else { "" })
        .replace("@EXIT_DEADLINE@", &exit_deadline)
        .replace("@FINALLY_FINISH@", if finally { FINALLY_FINISH } else { "" })
        .replace("@CLEANUP_CLOCK@", if finally { "ku_task_adapter_finally_now" } else { "ku_task_adapter_now" })
        .replace("@EXIT_EMPTY_STARTED@", empty_started)
        .replace("@DRAIN_HAS_OWNERS@", if scoped { "scope_live_mask || instance->receipt_mask" } else { "(instance->frame.header.initialized & UINT64_C(@TASK_MASK@)) || instance->receipt_mask" })
        .replace("@RECEIPT_FIELD@", if has_children { "  KuTaskDriverCleanupReceiptV1 receipts[@TASK_COUNT@];" } else { "" })
        .replace("@CANCEL_RECEIPTS@", if has_children { CANCEL_RECEIPTS } else { "" })
        .replace("@READ_RECEIPTS@", if has_children { READ_RECEIPTS } else { "" })
        .replace("@WAIT_RECEIPTS@", if has_children { WAIT_RECEIPTS } else { "" })
        .replace("@ID@", &id)
        .replace("@RESULT@", &result_type)
        .replace("@SUFFIX@", &c_type_suffix(inner)?)
        .replace("@PARAM_DECLS@", &declarations)
        .replace("@PARAM_ARGS@", &arguments)
        .replace("@PREFLIGHT@", &preflight)
        .replace("@CALLABLE_CHECKS@", &callable_checks)
        .replace("@CHARGE@", &charge)
        .replace("@SOURCE_HEADERS@", &source_headers)
        .replace("@SOURCE_BUFFERS@", &source_buffers)
        .replace("@HOSTED_HEADERS@", &hosted_headers)
        .replace("@HOSTED_CHARGE@", &hosted_charge)
        .replace("@HOSTED_TICKET_ALIASES@", &hosted_ticket_aliases)
        .replace("@HOSTED_RUNTIME_INPUTS@", &hosted_runtime_inputs)
        .replace("@RESTORE@", &restore)
        .replace("@REJECTED_DROPS@", &rejected_drops)
        .replace("@PAYLOAD_CHARGE@", &payload_charge)
        .replace("@FIELD@", host::field(&function.result)?)
        .replace("@KIND@", &host::kind(&function.result)?.to_string())
        .replace("@TASK_MASK@", &task_mask.to_string())
        .replace("@TASK_COUNT@", &task_count.to_string())
        .replace("@TRANSFERS@", &transfers)
        .replace("@PAYLOAD_ALIASES@", &payload_aliases)
        .replace("@REQUEST_ALIASES@", &request_aliases)
        .replace(
            "@EMPTY_RESULT@",
            &empty_value_expr(&function.result, "(*output)")?,
        );
    out.push_str(&source);
    out.check()
}

const ADAPTER_ABI: &str = r#"
/* Internal typed adapter ABI; not source Task syntax or a stable C FFI.
 * Handle/header calls are exclusive, not concurrently copied raw owner calls.
 * A valid deep Owned allocation is unique; integer-range checks cannot prove
 * that an arbitrary C pointer is mapped, writable or genuinely owns malloc.
 * All argument/result/handle headers are complete independent caller-owned
 * objects, disjoint from Task-owned deep allocations and alive through calls.
 * A live handle remains valid until a successful move or owner transfer. The
 * known start/take buffer checks are extra guards, not a general alias oracle:
 * move must not inspect a concurrently running frame to guess its deep ranges.
 * Scope drop transfers ownership to R3; it is not a cleanup/retirement ACK.
 * take is a single nonblocking attempt; hosted Await registers the fixed link.
 */
enum { KU_TASK_ADAPTER_OUT_OF_MEMORY = 48u };
static uint32_t ku_task_adapter_frame_status(uint32_t status) {
  switch (status) {
    case KU_TASK_FRAME_OK: return KU_TASK_CONTROL_OK;
    case KU_TASK_FRAME_ABI_MISMATCH: return KU_TASK_CONTROL_ABI_MISMATCH;
    case KU_TASK_FRAME_INVALID_STORAGE:
    case KU_TASK_FRAME_INVALID_ARGUMENT: return KU_TASK_CONTROL_INVALID_ARGUMENT;
    case KU_TASK_FRAME_LIMIT: return KU_TASK_CONTROL_LIMIT;
    default: return KU_TASK_CONTROL_INVALID_STATE;
  }
}
static size_t ku_task_adapter_string_extent(KuString value) {
  return value.storage == KU_STRING_OWNED && value.ptr
      ? (value.capacity ? value.capacity : 1) : value.len;
}
static uint32_t ku_task_adapter_string_charge(KuString value, size_t* charge) {
  if (value.storage == KU_STRING_STATIC) {
    if (value.capacity || (value.len && !value.ptr)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  } else if (value.storage == KU_STRING_OWNED) {
    if (value.len > value.capacity || (!value.ptr && (value.len || value.capacity)))
      return KU_TASK_DRIVER_INVALID_ARGUMENT;
    /* Existing concat(empty,empty) owns malloc(1) with len/capacity both zero. */
    size_t bytes = value.ptr ? (value.capacity ? value.capacity : 1) : 0;
    if (bytes > SIZE_MAX - *charge) return KU_TASK_DRIVER_LIMIT;
    *charge += bytes;
  } else return KU_TASK_DRIVER_INVALID_ARGUMENT;
  size_t extent = ku_task_adapter_string_extent(value);
  if (extent && (!value.ptr || extent > UINTPTR_MAX - (uintptr_t)value.ptr))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return KU_TASK_DRIVER_OK;
}
static int ku_task_adapter_string_overlaps(KuString value, const void* pointer, size_t bytes) {
  size_t extent = ku_task_adapter_string_extent(value);
  return extent && ku_task_frame_ranges_overlap(value.ptr, extent, pointer, bytes);
}
static int ku_task_adapter_string_overlaps_runtime(KuString value, KuTaskDriverV1* driver) {
  return ku_task_adapter_string_overlaps(value, driver, sizeof(*driver))
      || ku_task_adapter_string_overlaps(value, driver->slots, driver->capacity * sizeof(*driver->slots))
      || ku_task_adapter_string_overlaps(value, driver->ring, driver->capacity * sizeof(*driver->ring))
      || (driver->binding_keys && ku_task_adapter_string_overlaps(value,driver->binding_keys,driver->binding_cells*sizeof(*driver->binding_keys)))
      || (driver->blocking_pool && ku_task_adapter_string_overlaps(value,driver->blocking_pool,driver->blocking_pool_bytes))
      || (driver->blocking_storage && ku_task_adapter_string_overlaps(value,driver->blocking_storage,driver->blocking_storage_bytes));
}
/* Bounded flat arrays retain the 64-element literal limit in verified IR.
 * Runtime directory outputs may carry up to 10000 entries. */
typedef struct KuTaskAdapterBufferV1 { const void* pointer; size_t bytes; uint32_t owned; } KuTaskAdapterBufferV1;
typedef struct KuTaskAdapterStringVectorV1 { const KuString* data; size_t len; } KuTaskAdapterStringVectorV1;
static uint32_t ku_task_adapter_array_buffer(const void* data,size_t len,size_t capacity,
    size_t element_bytes,size_t alignment,KuTaskAdapterBufferV1* output) {
  if (len>capacity || capacity>10000u || !element_bytes || capacity>SIZE_MAX/element_bytes
      || (!capacity && data)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  size_t bytes=capacity*element_bytes;
  if (bytes && !ku_task_frame_storage_valid(data,bytes,bytes,alignment)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  *output=(KuTaskAdapterBufferV1){data,bytes,1u}; return KU_TASK_DRIVER_OK;
}
static KuTaskAdapterBufferV1 ku_task_adapter_string_buffer(KuString value) {
  return (KuTaskAdapterBufferV1){value.ptr,ku_task_adapter_string_extent(value),value.storage==KU_STRING_OWNED};
}
static int ku_task_adapter_buffers_overlap(KuTaskAdapterBufferV1 left,KuTaskAdapterBufferV1 right) {
  return (left.owned || right.owned) && ku_task_frame_ranges_overlap(left.pointer,left.bytes,right.pointer,right.bytes);
}
static uint32_t ku_task_adapter_buffer_excludes(KuTaskAdapterBufferV1 buffer,
    const KuTaskAdapterBufferV1* headers,size_t header_count,
    const KuTaskAdapterBufferV1* extra,size_t extra_count,KuTaskDriverV1* driver) {
  for (size_t i=0;i<header_count;i++) if (ku_task_frame_ranges_overlap(buffer.pointer,buffer.bytes,headers[i].pointer,headers[i].bytes)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  for (size_t i=0;i<extra_count;i++) if (ku_task_frame_ranges_overlap(buffer.pointer,buffer.bytes,extra[i].pointer,extra[i].bytes)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (driver && buffer.bytes && !ku_task_driver_external_storage(driver,buffer.pointer,buffer.bytes,1u)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_adapter_scratch(KuTaskDriverV1* driver,size_t count,
    KuTaskAdapterBufferV1** output,size_t* bytes) {
  *output=NULL; *bytes=0;
  /* 64 parameters, each holding at most one 10000-entry directory result. */
  if (count>640000u || count>SIZE_MAX/sizeof(**output)) return KU_TASK_DRIVER_LIMIT;
  if (!count) return KU_TASK_DRIVER_OK;
  size_t size=count*sizeof(**output);
  uint32_t status=ku_task_driver_scratch_reserve(driver,size);
  if (status!=KU_TASK_DRIVER_OK) return status;
  void* pointer=malloc(size);
  if (!pointer) {
    status=ku_task_driver_scratch_refund(driver,size);
    return status==KU_TASK_DRIVER_OK ? KU_TASK_ADAPTER_OUT_OF_MEMORY : status;
  }
  *output=(KuTaskAdapterBufferV1*)pointer; *bytes=size; return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_adapter_scratch_free(KuTaskDriverV1* driver,
    KuTaskAdapterBufferV1* pointer,size_t bytes,uint32_t status) {
  free(pointer);
  if (bytes) {
    uint32_t refunded=ku_task_driver_scratch_refund(driver,bytes);
    if (refunded!=KU_TASK_DRIVER_OK) return refunded;
  }
  return status;
}
static int ku_task_adapter_range_compare(const void* left,const void* right) {
  uintptr_t a=(uintptr_t)((const KuTaskAdapterBufferV1*)left)->pointer;
  uintptr_t b=(uintptr_t)((const KuTaskAdapterBufferV1*)right)->pointer;
  return a<b ? -1 : a>b ? 1 : 0;
}
static uint32_t ku_task_adapter_ranges_disjoint(KuTaskAdapterBufferV1* ranges,size_t count) {
  if (count>1u) qsort(ranges,count,sizeof(*ranges),ku_task_adapter_range_compare);
  uintptr_t all_end=0,owned_end=0;
  for (size_t i=0;i<count;i++) {
    KuTaskAdapterBufferV1 range=ranges[i];
    if (!range.bytes) continue;
    uintptr_t start=(uintptr_t)range.pointer;
    if (!start || range.bytes>UINTPTR_MAX-start) return KU_TASK_DRIVER_INVALID_ARGUMENT;
    if (start<owned_end || (range.owned && start<all_end)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
    uintptr_t end=start+range.bytes;
    if (end>all_end) all_end=end;
    if (range.owned && end>owned_end) owned_end=end;
  }
  return KU_TASK_DRIVER_OK;
}
/* All pointed-to typed extents remain a raw caller precondition. Validate the
 * vector's full capacity against known headers/runtime storage before reading
 * any leaf metadata; stream prior leaves to keep temporary storage bounded. */
static uint32_t ku_task_adapter_array_excludes(KuTaskDriverV1* driver,
    const void* data,size_t len,size_t capacity,size_t element_bytes,size_t alignment,
    int string_leaves,const KuTaskAdapterBufferV1* extra,size_t extra_count) {
  KuTaskAdapterBufferV1 vector={0};
  uint32_t checked=ku_task_adapter_array_buffer(data,len,capacity,element_bytes,alignment,&vector);
  if (checked!=KU_TASK_DRIVER_OK) return checked;
  checked=ku_task_adapter_buffer_excludes(vector,NULL,0,extra,extra_count,driver);
  if (checked!=KU_TASK_DRIVER_OK || !string_leaves) return checked;
  KuTaskAdapterBufferV1 local[64],*ranges=local;
  size_t scratch_bytes=0;
  if (len>64u) {
    checked=ku_task_adapter_scratch(driver,len,&ranges,&scratch_bytes);
    if (checked!=KU_TASK_DRIVER_OK) return checked;
  }
  const KuString* strings=(const KuString*)data;
  size_t charge=vector.bytes;
  for (size_t leaf=0;leaf<len;leaf++) {
    KuString value=strings[leaf];
    checked=ku_task_adapter_string_charge(value,&charge);
    if (checked!=KU_TASK_DRIVER_OK) break;
    ranges[leaf]=ku_task_adapter_string_buffer(value);
    checked=ku_task_adapter_buffer_excludes(ranges[leaf],NULL,0,extra,extra_count,driver);
    if (checked!=KU_TASK_DRIVER_OK) break;
    if (ku_task_adapter_buffers_overlap(ranges[leaf],vector)) {
      checked=KU_TASK_DRIVER_INVALID_ARGUMENT; break;
    }
  }
  if (checked==KU_TASK_DRIVER_OK) checked=ku_task_adapter_ranges_disjoint(ranges,len);
  return scratch_bytes ? ku_task_adapter_scratch_free(driver,ranges,scratch_bytes,checked) : checked;
}
static void ku_task_adapter_fault(KuTaskDriverV1* driver, int clock_fault) {
  /* Callbacks run outside the queue mutex. The registered task or BUILDING
   * reservation protects driver storage while recording a sticky fault. */
  if (ku_task_driver_lock(driver)) return;
  if (clock_fault) ku_task_driver_enter_clock_fault(driver);
  else { driver->fault = KU_TASK_DRIVER_INTERNAL; ku_task_driver_signal(driver); }
  ku_task_driver_unlock(driver);
}
typedef struct KuTaskAdapterClockV1 {
  KuTaskDriverV1* driver;
  KuTaskControlV1* budget;
} KuTaskAdapterClockV1;
static uint64_t ku_task_adapter_now(void* raw) {
  KuTaskAdapterClockV1* clock = (KuTaskAdapterClockV1*)raw;
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX) {
    ku_task_adapter_fault(clock->driver, 1);
    return UINT64_MAX;
  }
  /* Re-read the live minimum at EVERY cleanup safepoint. Do not mutate the
   * frame deadline from this callback: C comparison operand order is unspecified.
   * Returning MAX forces the existing frame drop-glue path after a tightening. */
  if (clock->budget && now >= ku_task_control_cleanup_deadline(clock->budget))
    return UINT64_MAX;
  return now;
}
static uint32_t ku_task_adapter_rollback(KuTaskDriverTicketV1* ticket, uint32_t failure) {
  uint32_t result = ku_task_driver_rollback(ticket);
  if (result == KU_TASK_DRIVER_OK) return failure;
  /* A broken OS mutex may prevent budget return. Keep the actual reservation
   * visible as a fault; do not forge zero accounting or terminate the process. */
  ku_task_adapter_fault(ticket->driver, 0);
  return KU_TASK_DRIVER_INTERNAL;
}
"#;

// Only scoped functions carry session state. Reuse the same receipt span and
// drain implementation; no per-scope heap allocation or runtime scope stack.
const SCOPE_FIELDS: &str = r#"
  uint32_t scope_mode; /* 0 NONE, 1 NORMAL, 2 FINAL */
  uint64_t scope_expected_mask, scope_issued_mask;
  KuTaskDriverScopeTokenV1 scope_token;
"#;
const SCOPE_SELECT: &str = r#"
  const KuTaskFrameScopeDescriptorV1* normal_scope=NULL;
  uint64_t task_selection=UINT64_C(@TASK_MASK@), scope_live_mask=0;
  if (!reason && instance->frame.header.status==KU_TASK_FRAME_SCOPE_REQUEST) {
    uint32_t status=ku_task_frame_@ID@_scope_request(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,&normal_scope);
    if (status!=KU_TASK_FRAME_OK || instance->scope_mode==2u) return KU_TASK_DRIVER_INVALID_STATE;
    if (instance->scope_mode==1u && instance->scope_token.scope_id!=normal_scope->scope_id) return KU_TASK_DRIVER_INVALID_STATE;
    task_selection=normal_scope->task_mask;
  }
  /* Validate the entire new LIVE set before begin/promotion or any handoff.
   * Fixed receipt indices are declaration-ordered, not raw sparse SlotIds. */
@SCOPE_PREFLIGHT@
"#;
const SCOPE_REGISTER: &str = r#"
  @SCOPE_RECEIPT_REGISTER@if (!normal_scope && instance->scope_mode) {
    /* Explicitly re-min even after a sticky timeout: ordinary driver refresh
     * then returns early. The real promotion preserves ACK and wait epochs. */
    uint64_t effective=UINT64_MAX;
    uint32_t status=ku_task_driver_scope_promote_final(&instance->ticket,&instance->scope_token,
        scope_live_mask,instance->drain_deadline,&effective);
    if (status!=KU_TASK_DRIVER_OK) return status;
    instance->scope_expected_mask |= scope_live_mask;
    instance->scope_mode=2u; instance->drain_deadline=effective;
  }
"#;
const SCOPE_RECEIPT_REGISTER: &str = r#"if (normal_scope && !instance->scope_mode && scope_live_mask) {
    uint32_t status=ku_task_driver_scope_begin(&instance->ticket,normal_scope->scope_id,
        instance->receipts,@TASK_COUNT@u,scope_live_mask,instance->drain_deadline,&instance->scope_token);
    if (status!=KU_TASK_DRIVER_OK) return status;
    instance->scope_mode=1u; instance->scope_expected_mask=scope_live_mask;
  } else "#;
// A multi-scope exit owns a single absolute budget in private frame storage.
// Session close still retires its receipts normally. The next session imports
// this same (possibly tightened) D; even an empty scope cannot renew it.
const EXIT_DEADLINE: &str = r#"
  if (instance->frame.scope_exit_active) {
    uint64_t deadline=ku_task_host_deadline(&host,instance->frame.scope_exit_deadline,0);
    if (instance->drain_started) deadline=ku_task_driver_min(deadline,instance->drain_deadline);
    instance->drain_deadline=deadline; instance->drain_started=1;
    instance->frame.scope_exit_deadline=deadline;
  }
"#;
const FINALLY_DEADLINE: &str = r#"
  if (instance->frame.finally_unwind_active) {
    uint64_t deadline=ku_task_host_deadline(&host,instance->frame.finally_unwind_deadline,0);
    if (instance->drain_started) deadline=ku_task_driver_min(deadline,instance->drain_deadline);
    instance->drain_deadline=deadline; instance->drain_started=1;
    instance->frame.finally_unwind_deadline=deadline;
  }
"#;
const FINALLY_CLOCK: &str = r#"
/* A protected body cannot renew its inherited D. Read a later control or
 * driver-close minimum at every dispatch, including staged timeout cleanup. */
static uint64_t ku_task_adapter_finally_now(void* raw) {
  KuTaskAdapterClockV1* clock=(KuTaskAdapterClockV1*)raw;
  uint64_t now=ku_task_adapter_now(raw);
  if (now==UINT64_MAX) return now;
  if (ku_task_driver_lock(clock->driver)) {
    ku_task_adapter_fault(clock->driver,0); return UINT64_MAX;
  }
  uint64_t deadline=clock->driver->closing ? clock->driver->shutdown_deadline : UINT64_MAX;
  ku_task_driver_unlock(clock->driver);
  return now>=deadline ? UINT64_MAX : now;
}
"#;
const FINALLY_FINISH: &str = r#"
    if (instance->frame.header.cleanup_state!=UINT32_MAX) {
      KuTaskAdapterClockV1 bridge={instance->ticket.driver,&instance->control};
      KuTaskFrameClockV1 clock={ku_task_adapter_finally_now,&bridge,&host};
      uint64_t deadline=instance->frame.header.exit_deadline;
      if (instance->drain_started) deadline=ku_task_driver_min(deadline,instance->drain_deadline);
      uint32_t finished=ku_task_frame_@ID@_finish_finally(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,deadline,&clock);
      if (finished!=KU_TASK_FRAME_OK) return KU_TASK_DRIVER_INTERNAL;
    }
"#;
const SCOPE_RESUME_HELPER: &str = r#"
static uint32_t ku_task_@ID@_scope_resume(KuTaskInstance_@ID@* instance) {
  const KuTaskFrameScopeDescriptorV1* descriptor=NULL;
  uint32_t status=ku_task_frame_@ID@_scope_request(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,&descriptor);
  if (status!=KU_TASK_FRAME_OK) return status;
  status=ku_task_@ID@_drain(instance,0,NULL);
  if (status==KU_TASK_DRIVER_PENDING || status==KU_TASK_DRIVER_WAIT_ABORTED) return KU_TASK_FRAME_PENDING;
  if (status==KU_TASK_DRIVER_OK && instance->scope_mode==1u) {
    if (instance->scope_issued_mask!=instance->scope_expected_mask || instance->receipt_mask || instance->wait.driver)
      return KU_TASK_FRAME_INVALID_STATE;
    /* drain has read/detached the last NOTIFIED wait. A zero pending bitmap
     * alone is not a close witness: driver checks the entire real manifest. */
    status=ku_task_driver_scope_end(&instance->ticket,&instance->scope_token);
    if (status==KU_TASK_DRIVER_WAIT_ABORTED) return KU_TASK_FRAME_PENDING;
    /* ACK cannot revert for these issued identities. PENDING here means the
     * manifest contract broke; do not turn it into an unbounded YIELD loop. */
    if (status==KU_TASK_DRIVER_PENDING) return KU_TASK_FRAME_INVALID_STATE;
  }
  if (status==KU_TASK_DRIVER_CLEANUP_TIMEOUT) {
    if (instance->scope_mode!=1u || !instance->drain_started) return KU_TASK_FRAME_INVALID_STATE;
    return ku_task_frame_@ID@_scope_timeout(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,
        descriptor->scope_id,instance->drain_deadline);
  }
  if (status!=KU_TASK_DRIVER_OK || instance->scope_mode==2u) return KU_TASK_FRAME_INVALID_STATE;
  if (!instance->scope_mode) {
    /* Explicit empty/INLINE_FAILED witness: all selected headers were checked
     * and cleared without a receipt, session or new D. This is not scope_end.
     * A concurrent cancellation still must pass R2 before any ready user code. */
    if (@EXIT_EMPTY_STARTED@ || instance->scope_expected_mask || instance->scope_issued_mask
        || instance->receipt_mask || instance->wait.driver
        || (instance->frame.header.initialized & descriptor->task_mask)) return KU_TASK_FRAME_INVALID_STATE;
    if (ku_task_control_atomic_load(&instance->control.phase)!=KU_TASK_CONTROL_LIVE) return KU_TASK_FRAME_PENDING;
  }
  status=ku_task_frame_@ID@_scope_continue(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,descriptor->scope_id);
  if (status!=KU_TASK_FRAME_OK) return status;
  /* Only successful normal end/empty continuation permits receipt reuse and a
   * fresh independent scope budget. Do not reset external cancellation state. */
  instance->scope_mode=0;
  instance->scope_expected_mask=0; instance->scope_issued_mask=0;
  instance->drain_started=0; instance->drain_deadline=0; instance->drain_failure=0;
  instance->drain_deadline_published=0; instance->drain_published_deadline=0;
  instance->receipt_mask=0;
@RESET_RECEIPTS@
  if (ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_YIELD)!=KU_TASK_DRIVER_OK) return KU_TASK_FRAME_INVALID_STATE;
  return KU_TASK_FRAME_PENDING;
}
"#;
const SCOPE_RESUME: &str = r#"
  if (status==KU_TASK_FRAME_SCOPE_REQUEST && !instance->payload_initialized)
    status=ku_task_@ID@_scope_resume(instance);
"#;

const RECONCILE_HELPER: &str = r#"
/* Opt-in typed scans cover dynamic projections, blocking I/O and owned Value
 * replacements. Every owned slot in these hosted frames is persistent. */
static uint32_t ku_task_@ID@_reconcile_charge(void* raw) {
  KuTaskInstance_@ID@* instance=(KuTaskInstance_@ID@*)raw;
  if (!instance->registered) return KU_TASK_DRIVER_OK;
  size_t bytes=0;
  uint32_t status=KU_TASK_DRIVER_OK;
@RECONCILE_FIELDS@
  status=@RECONCILE_CALL@;
  return status;
}
"#;
const RECONCILE_AFTER_RESUME: &str = r#"
  /* Only a proved scalar-only budget yield can omit this redundant scan.
   * Final/failed/waiting/mixed callbacks and every other reconcile call stay. */
  if(!pure_ready
      && ku_task_@ID@_reconcile_charge(instance)!=KU_TASK_DRIVER_OK)return KU_TASK_CONTROL_INVALID_STATE;
"#;

const HTTP_ADAPTER_ABI: &str = r#"
/* HTTP parser-produced maps contain only owned string keys/string values.
 * Their allocations move with the request; no yyjson/foreign node is borrowed.
 * Header/body/target limits bound this finite table scan. */
static uint32_t ku_task_adapter_http_map_charge(KuObject* map,size_t* bytes) {
  if (!map) return KU_TASK_DRIVER_OK;
  if (!map->entries || map->len>map->cap || map->cap>65536u
      || map->cap>(SIZE_MAX-sizeof(*map))/sizeof(KuEntry)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  size_t fixed=sizeof(*map)+map->cap*sizeof(KuEntry);
  if (fixed>SIZE_MAX-*bytes) return KU_TASK_DRIVER_LIMIT;
  *bytes+=fixed;
  for(size_t i=0;i<map->cap;i++) if(map->entries[i].used) {
    KuEntry* entry=&map->entries[i];
    if(entry->value.tag!=KU_STR) return KU_TASK_DRIVER_INVALID_ARGUMENT;
    uint32_t status=ku_task_adapter_string_charge(entry->key,bytes);
    if(status==KU_TASK_DRIVER_OK) status=ku_task_adapter_string_charge(entry->value.as.s,bytes);
    if(status!=KU_TASK_DRIVER_OK) return status;
  }
  return KU_TASK_DRIVER_OK;
}
"#;

const CANCEL_RECEIPTS: &str = r#"    for (size_t i=0;i<@TASK_COUNT@u;i++) if (instance->receipt_mask & (UINT64_C(1)<<i)) {
      uint32_t status=ku_task_driver_cancel_receipt(&instance->receipts[i],instance->drain_deadline);
      if (status==KU_TASK_DRIVER_CLEANUP_ACK) instance->receipt_mask &= ~(UINT64_C(1)<<i);
      else if (status!=KU_TASK_DRIVER_OK) { instance->drain_failure=status; return status; }
    }"#;
const READ_RECEIPTS: &str = r#"  for (size_t i=0;i<@TASK_COUNT@u;i++) if (instance->receipt_mask & (UINT64_C(1)<<i)) {
    uint32_t status=ku_task_driver_cleanup_receipt_read(&instance->receipts[i]);
    if (status==KU_TASK_DRIVER_CLEANUP_ACK) { instance->receipt_mask &= ~(UINT64_C(1)<<i); continue; }
    if (status!=KU_TASK_DRIVER_PENDING) { instance->drain_failure=status; return status; }
  }"#;
const WAIT_RECEIPTS: &str = r#"  for (size_t i=0;i<@TASK_COUNT@u;i++) if (instance->receipt_mask & (UINT64_C(1)<<i)) {
    uint32_t status=ku_task_driver_cleanup_wait_arm(&instance->ticket,&instance->receipts[i],instance->drain_deadline,&instance->wait);
    if (status==KU_TASK_DRIVER_CLEANUP_ACK) { instance->receipt_mask &= ~(UINT64_C(1)<<i); continue; }
    if (status==KU_TASK_DRIVER_PENDING) return status;
    instance->drain_failure=status; return status;
  }"#;

const ADAPTER_FUNCTION: &str = r#"
typedef struct KuTaskInstance_@ID@ {
  KuTaskControlV1 control;
  KuTaskFrame_@ID@ frame;
  @RESULT@ payload;
  KuTaskDriverTicketV1 ticket;
  bool frame_initialized, payload_initialized, registered;
  uint32_t exit_class, has_cleanup_deadline;
  uint64_t cleanup_deadline;
  uint32_t drain_started, values_cleaned, drain_failure;
  uint64_t drain_deadline, receipt_mask;
  uint32_t drain_deadline_published;
  uint64_t drain_published_deadline;
@RECEIPT_FIELD@
  KuTaskDriverWaitTokenV1 wait;
  KuTaskDriverTimerTokenV1 timer;
@BLOCKING_FIELD@
@SCOPE_FIELDS@
} KuTaskInstance_@ID@;
typedef struct KuTaskHandle_@ID@ {
  KuTaskControlOwnerV1 owner;
  KuTaskDriverTicketV1 ticket;
} KuTaskHandle_@ID@;
@RECONCILE_HELPER@

/* The sole executor calls this only after a READY frame. A staged bridge
 * exit is not READY until every Task owner was durably handed off and the
 * generated finish-exit glue cleaned the remaining typed Values. */
static uint32_t ku_task_@ID@_take_frame_payload(KuTaskInstance_@ID@* instance) {
  if (!instance->frame_initialized || instance->payload_initialized) return KU_TASK_FRAME_INVALID_STATE;
  uint32_t status=ku_task_frame_@ID@_take_result(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,&instance->payload);
  if (status==KU_TASK_FRAME_OK) {
    instance->payload_initialized=true;
    instance->exit_class=instance->frame.header.exit_class ? instance->frame.header.exit_class : KU_TASK_EXIT_USER_RESULT;
    instance->has_cleanup_deadline=instance->frame.header.has_exit_deadline;
    instance->cleanup_deadline=instance->frame.header.exit_deadline;
  }
  return status;
}

static uint32_t ku_task_@ID@_drain(KuTaskInstance_@ID@* instance, uint32_t reason, KuTaskControlV1* budget) {
  KuTaskAdapterHostV1 host={&instance->ticket,&instance->control,&instance->wait,sizeof(*instance),&instance->timer,@ALLOCATION_HOST@,@BLOCKING_HOST@};
  if (instance->timer.driver) {
    uint32_t timer_status=ku_task_driver_timer_detach(&instance->timer);
    if (timer_status!=KU_TASK_DRIVER_OK && timer_status!=KU_TASK_DRIVER_STALE) return timer_status;
    if (timer_status==KU_TASK_DRIVER_STALE) instance->timer=(KuTaskDriverTimerTokenV1){0};
  }
  if (instance->wait.driver) {
    KuTaskDriverWaitSnapshotV1 snapshot={0};
    uint32_t status=ku_task_driver_wait_read(&instance->wait,&snapshot);
    if (status!=KU_TASK_DRIVER_OK) return status;
    if (snapshot.kind==KU_TASK_DRIVER_WAIT_KIND_RESULT) {
      if (!reason) return KU_TASK_DRIVER_INTERNAL;
      status=ku_task_driver_wait_detach(&instance->wait);
      if (status!=KU_TASK_DRIVER_OK) return status;
    }
  }
@SCOPE_SELECT@
  if (!instance->drain_started && (@DRAIN_HAS_OWNERS@)) {
    instance->drain_deadline=ku_task_host_deadline(&host,instance->has_cleanup_deadline ? instance->cleanup_deadline : UINT64_MAX,1);
    instance->drain_started=1;
  }
  if (instance->drain_started) instance->drain_deadline=ku_task_host_deadline(&host,instance->drain_deadline,0);
  /* R2 entered cleanup only after the original cancellation was published.
   * A concurrent tightening may now be PUBLISHING again: the already-stored
   * old/new budget is still valid and must be consumed before child transfer.
   * R2 retries reconciliation if its pre-callback snapshot subsequently changes. */
  if (reason && budget && instance->drain_started)
    instance->drain_deadline=ku_task_driver_min(instance->drain_deadline,ku_task_control_cleanup_deadline(budget));
@EXIT_DEADLINE@
@SCOPE_REGISTER@
@TRANSFERS@
  /* Every sibling was durably transferred before any Value cleanup/ACK wait. */
  if (!reason && instance->frame.header.status==KU_TASK_FRAME_EXIT_STAGED) {
@FINALLY_FINISH@
    uint32_t status=ku_task_frame_@ID@_finish_exit_values(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION);
    if (status!=KU_TASK_FRAME_OK) return KU_TASK_DRIVER_INTERNAL;
    instance->values_cleaned=1;
    status=ku_task_@ID@_take_frame_payload(instance);
    if (status!=KU_TASK_FRAME_OK) return KU_TASK_DRIVER_INTERNAL;
  }
  /* Private payload discard obeys the same all-owner handoff checkpoint as
   * Value cleanup. Never discard it before an unfinished/failed transfer. */
  if (reason && instance->payload_initialized) {
    instance->payload_initialized=false;
    ku_result_drop_@SUFFIX@(&instance->payload);
  }
  /* Capture may have copied an older frame deadline. Reapply the actual
   * drain minimum AFTER capture, even when every receipt is already ACKed.
   * Ordinary USER_RESULT does not carry this completed scope's D outward. */
  if (instance->exit_class==KU_TASK_EXIT_RUNTIME_FAILURE && instance->drain_started) {
    instance->has_cleanup_deadline=1;
    instance->cleanup_deadline=instance->drain_deadline;
  }
  if (instance->drain_started && (!instance->drain_deadline_published
      || instance->drain_deadline < instance->drain_published_deadline)) {
    /* A later ancestor cancellation can tighten an already-transferred scope.
     * Receipts protect logical identity, and the driver uses its live registry
     * lease: never reconstruct a child owner or renew its original reason. */
@CANCEL_RECEIPTS@
    instance->drain_published_deadline=instance->drain_deadline;
    instance->drain_deadline_published=1;
  }
  if (reason && !instance->values_cleaned && instance->frame_initialized) {
    KuTaskAdapterClockV1 bridge={instance->ticket.driver,budget};
    KuTaskFrameClockV1 clock={@CLEANUP_CLOCK@,&bridge,&host};
    uint32_t status;
    if (instance->frame.header.status==KU_TASK_FRAME_READY) {
      status=ku_task_frame_@ID@_destroy(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION);
      if (status!=KU_TASK_FRAME_OK) return KU_TASK_DRIVER_INTERNAL;
      instance->frame_initialized=false;
    } else {
      uint32_t frame_reason=reason==KU_TASK_CONTROL_CANCELLED ? KU_TASK_FRAME_CANCELLED : KU_TASK_FRAME_TIMED_OUT;
      uint64_t value_deadline=ku_task_control_cleanup_deadline(budget);
      if (instance->drain_started) value_deadline=ku_task_driver_min(value_deadline,instance->drain_deadline);
      status=ku_task_frame_@ID@_terminate(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,frame_reason,value_deadline,&clock);
      if (status!=KU_TASK_FRAME_CANCELLED && status!=KU_TASK_FRAME_TIMED_OUT) return KU_TASK_DRIVER_INTERNAL;
    }
    instance->values_cleaned=1;
  }
@RECONCILE_DRAIN@
  if (instance->wait.driver) {
    KuTaskDriverWaitSnapshotV1 snapshot={0};
    uint32_t status=ku_task_driver_wait_read(&instance->wait,&snapshot);
    if (status!=KU_TASK_DRIVER_OK) return status;
    if (snapshot.state==KU_TASK_DRIVER_WAIT_ARMED) {
      if (ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT)!=KU_TASK_DRIVER_OK) return KU_TASK_DRIVER_INTERNAL;
      return KU_TASK_DRIVER_PENDING;
    }
    status=ku_task_driver_wait_detach(&instance->wait);
    if (status!=KU_TASK_DRIVER_OK) return status;
    if (snapshot.outcome!=KU_TASK_DRIVER_CLEANUP_ACK) instance->drain_failure=snapshot.outcome;
  }
  if (instance->drain_failure) return instance->drain_failure;
  /* Bounded by declared Task slots; no repeated polling or recursive child drive. */
@READ_RECEIPTS@
  if (!instance->receipt_mask) return KU_TASK_DRIVER_OK;
@WAIT_RECEIPTS@
  return KU_TASK_DRIVER_OK;
}
@SCOPE_RESUME_HELPER@
static uint32_t ku_task_@ID@_resume(void* raw) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  if (!instance->frame_initialized) return KU_TASK_CONTROL_INVALID_STATE;
  KuTaskAdapterClockV1 bridge = { instance->ticket.driver, NULL };
  KuTaskAdapterHostV1 host={&instance->ticket,&instance->control,&instance->wait,sizeof(*instance),&instance->timer,@ALLOCATION_HOST@,@BLOCKING_HOST@};
  KuTaskFrameClockV1 clock = { ku_task_adapter_now, &bridge, &host };
  uint32_t status=KU_TASK_FRAME_READY;
  if (!instance->payload_initialized) {
    if (ku_task_driver_begin_quantum(&instance->ticket,&instance->control)!=KU_TASK_DRIVER_OK) return KU_TASK_CONTROL_INVALID_STATE;
    /* The same complete eligibility gates B2 loans and private slice capacity;
     * the frame still proves a bounded pure entry before reserving any zone. */
    ku_task_driver_compute_begin(&instance->ticket,&instance->control,(@COMPUTE_BANKS_ALLOWED@));
    status = ku_task_frame_@ID@_resume(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, &clock);
    if (!ku_task_driver_compute_end(&instance->ticket,&instance->control)) return KU_TASK_CONTROL_INVALID_STATE;
  }
  /* One complete adapter proof serves both charge reconciliation and the
   * private executor segment. A frame's charge-only witness alone does not
   * exclude active waits, children/receipts, drain, blocking or finally. */
  int pure_ready=(@SCALAR_RECONCILE_SKIP@);
@RECONCILE_RETURN@
@SCOPE_RESUME@
  if (status == KU_TASK_FRAME_PENDING) {
    if (pure_ready) ku_task_driver_note_pure_ready(&instance->ticket,&instance->control);
    return KU_TASK_CONTROL_PENDING;
  } else if (status == KU_TASK_FRAME_READY && !instance->payload_initialized) {
    status=ku_task_@ID@_take_frame_payload(instance);
  } else if (status==KU_TASK_FRAME_EXIT_STAGED && !instance->payload_initialized) {
    /* A verified generated frame is stopped with a typed final-exit witness,
     * not a private/public payload. Import an inherited failure budget before
     * drain can create any new one; only drain may finish Values and take it. */
    instance->exit_class=instance->frame.header.exit_class;
    instance->has_cleanup_deadline=instance->frame.header.has_exit_deadline;
    instance->cleanup_deadline=instance->frame.header.exit_deadline;
  }
  if (instance->payload_initialized || status==KU_TASK_FRAME_EXIT_STAGED) {
    status=ku_task_@ID@_drain(instance,0,NULL);
    if (status==KU_TASK_DRIVER_PENDING) return KU_TASK_CONTROL_PENDING;
    if (status==KU_TASK_DRIVER_CLEANUP_TIMEOUT) {
      ku_result_drop_@SUFFIX@(&instance->payload);
      instance->payload.error=ku_error_make(ku_string_static((const uint8_t*)"task",4),ku_string_static((const uint8_t*)"shutdown_timeout",16),ku_string_static((const uint8_t*)"owned child cleanup deadline expired",36));
      instance->exit_class=KU_TASK_EXIT_RUNTIME_FAILURE;
      instance->has_cleanup_deadline=1; instance->cleanup_deadline=instance->drain_deadline;
      return KU_TASK_CONTROL_FAILED;
    }
    if (status==KU_TASK_DRIVER_OK) return instance->payload.ok ? KU_TASK_CONTROL_COMPLETED : KU_TASK_CONTROL_FAILED;
  }
  ku_task_adapter_fault(instance->ticket.driver, 0);
  return KU_TASK_CONTROL_INVALID_STATE;
}
static uint32_t ku_task_@ID@_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  uint32_t frame_reason = reason == KU_TASK_CONTROL_CANCELLED ? KU_TASK_FRAME_CANCELLED
      : reason == KU_TASK_CONTROL_TIMED_OUT ? KU_TASK_FRAME_TIMED_OUT : KU_TASK_FRAME_INVALID_ARGUMENT;
  if (frame_reason == KU_TASK_FRAME_INVALID_ARGUMENT) return KU_TASK_CONTROL_INVALID_ARGUMENT;
@BLOCKING_CANCEL@
  uint32_t status=ku_task_@ID@_drain(instance,reason,budget);
@BLOCKING_CLEANUP@
  if (status==KU_TASK_DRIVER_OK || status==KU_TASK_DRIVER_CLEANUP_TIMEOUT) return KU_TASK_CONTROL_OK;
  if (status==KU_TASK_DRIVER_PENDING) return KU_TASK_CONTROL_PENDING;
  /* READY is only possible when R2 already staged/dropped the frame itself;
   * it must never be reinterpreted here as cancellation cleanup success. */
  ku_task_adapter_fault(instance->ticket.driver, 0);
  return KU_TASK_CONTROL_INVALID_STATE;
}
static void ku_task_@ID@_drop_frame(void* raw) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  if (!instance->frame_initialized) return;
  uint32_t status = ku_task_frame_@ID@_destroy(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION);
  if (status != KU_TASK_FRAME_OK) { ku_task_adapter_fault(instance->ticket.driver, 0); return; }
  instance->frame_initialized = false;
@RECONCILE_DROP@
}
static void ku_task_@ID@_drop_payload(void* raw) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  if (instance->payload_initialized) {
    instance->payload_initialized = false;
    ku_result_drop_@SUFFIX@(&instance->payload);
  }
@RECONCILE_DROP@
}
static uint32_t ku_task_@ID@_take_payload(void* raw, void* destination) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  KuTaskAdapterTakeRequestV1* request=(KuTaskAdapterTakeRequestV1*)destination;
  if (!ku_task_driver_external_storage(instance->ticket.driver,request,sizeof(*request),KU_TASK_FRAME_ALIGNOF(KuTaskAdapterTakeRequestV1))
      || ku_task_frame_ranges_overlap(instance,sizeof(*instance),request,sizeof(*request))) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (!instance->payload_initialized) return KU_TASK_CONTROL_RESULT_TAKEN;
@REQUEST_ALIASES@
  /* The request may alias a smaller payload allocation. Reject that address
   * before reading even its private HTTP receipt fields. */
  if(request->retained_bytes || request->retain_payload_charge>1u
      || (request->parent && request->retain_payload_charge)) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  KuTaskAdapterOutcomeV1* outcome=request->outcome;
  if (!ku_task_driver_external_storage(instance->ticket.driver,outcome,sizeof(*outcome),KU_TASK_FRAME_ALIGNOF(KuTaskAdapterOutcomeV1))
      || ku_task_frame_ranges_overlap(instance,sizeof(*instance),outcome,sizeof(*outcome))
      || ku_task_frame_ranges_overlap(request,sizeof(*request),outcome,sizeof(*outcome))) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (request->parent && (!ku_task_driver_external_storage(instance->ticket.driver,request->parent,sizeof(*request->parent),KU_TASK_FRAME_ALIGNOF(KuTaskDriverTicketV1))
      || ku_task_frame_ranges_overlap(request->parent,sizeof(*request->parent),outcome,sizeof(*outcome))
      || ku_task_frame_ranges_overlap(request->parent,sizeof(*request->parent),request,sizeof(*request)))) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  @RESULT@* output=&outcome->value.@FIELD@;
@PAYLOAD_ALIASES@
  /* Reject deep alias before reading a typed header from that address: the
   * aliased allocation itself may be smaller than sizeof(*output). */
  if (outcome->result_kind || outcome->exit_class || outcome->has_cleanup_deadline || outcome->cleanup_deadline || !(@EMPTY_RESULT@)) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  size_t payload_charge=0;
  uint32_t checked=KU_TASK_DRIVER_OK;
@PAYLOAD_CHARGE@
  if (request->parent) {
    checked=ku_task_driver_transfer_charge(&instance->ticket,request->parent,payload_charge,sizeof(*instance));
    if (checked!=KU_TASK_DRIVER_OK) return checked;
  } else if(request->retain_payload_charge) {
    checked=ku_task_driver_extract_charge(&instance->ticket,payload_charge,sizeof(*instance));
    if(checked!=KU_TASK_DRIVER_OK) return checked;
    request->retained_bytes=payload_charge;
  }
  *output = ku_result_move_@SUFFIX@(&instance->payload);
  outcome->result_kind=@KIND@u; outcome->exit_class=instance->exit_class;
  outcome->has_cleanup_deadline=instance->has_cleanup_deadline; outcome->cleanup_deadline=instance->cleanup_deadline;
  instance->payload_initialized = false;
@RECONCILE_TAKE@
  return KU_TASK_CONTROL_OK;
}
static void ku_task_@ID@_dispose(KuTaskControlV1* control, void* raw) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  (void)control;
  KuTaskDriverTicketV1 ticket = instance->ticket;
  bool registered = instance->registered;
  if (instance->timer.driver) {
    uint32_t status=ku_task_driver_timer_detach(&instance->timer);
    if (status!=KU_TASK_DRIVER_OK && status!=KU_TASK_DRIVER_STALE) {
      ku_task_adapter_fault(ticket.driver,0); return;
    }
  }
  free(instance);
  /* Last access to the instance was BEFORE free. Slot/bytes remain reserved
   * until this separate ticket acknowledgement, including late observer refs. */
  if (registered && ku_task_driver_disposed(&ticket) != KU_TASK_DRIVER_OK)
    ku_task_adapter_fault(ticket.driver, 0);
}
static const KuTaskControlOpsV1 ku_task_@ID@_ops = {
  ku_task_@ID@_resume, ku_task_@ID@_cleanup, ku_task_@ID@_drop_frame,
  ku_task_@ID@_drop_payload, ku_task_@ID@_take_payload, ku_task_@ID@_dispose
};
static uint32_t ku_task_@ID@_check(KuTaskHandle_@ID@* handle) {
  if (!ku_task_frame_storage_valid(handle, sizeof(*handle), sizeof(*handle), KU_TASK_FRAME_ALIGNOF(KuTaskHandle_@ID@)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  uint32_t checked = ku_task_driver_check_ticket(&handle->ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(handle->ticket.driver, handle, sizeof(*handle), KU_TASK_FRAME_ALIGNOF(KuTaskHandle_@ID@)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  checked = ku_task_control_check_lease(&handle->owner.lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskControlV1* control = handle->owner.lease.control;
  /* Immutable callback identity validates the concrete layout before its cast.
   * The first instance field is control, so context must have that same address. */
  if (control->context != (void*)control || control->operations.resume != ku_task_@ID@_resume
      || control->operations.take_payload != ku_task_@ID@_take_payload || control->operations.dispose != ku_task_@ID@_dispose)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)control;
  if (!ku_task_driver_external_storage(handle->ticket.driver, instance, sizeof(*instance), KU_TASK_FRAME_ALIGNOF(KuTaskInstance_@ID@))
      || ku_task_frame_ranges_overlap(handle, sizeof(*handle), instance, sizeof(*instance))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (!instance->registered || instance->ticket.driver != handle->ticket.driver
      || instance->ticket.slot != handle->ticket.slot || instance->ticket.generation != handle->ticket.generation)
    return KU_TASK_DRIVER_INVALID_STATE;
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_@ID@_move(KuTaskHandle_@ID@* output, KuTaskHandle_@ID@* source) {
  uint32_t checked = ku_task_@ID@_check(source);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(source->ticket.driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(KuTaskHandle_@ID@))
      || ku_task_frame_ranges_overlap(source, sizeof(*source), output, sizeof(*output))
      || ku_task_frame_ranges_overlap(source->owner.lease.control, sizeof(KuTaskInstance_@ID@), output, sizeof(*output)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (output->owner.lease.control || output->ticket.driver || output->ticket.slot || output->ticket.generation)
    return KU_TASK_DRIVER_INVALID_STATE;
  checked = ku_task_control_owner_move(&output->owner, &source->owner);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  output->ticket = source->ticket; source->ticket = (KuTaskDriverTicketV1){0};
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_@ID@_take(KuTaskHandle_@ID@* handle, KuTaskAdapterOutcomeV1* output) {
  uint32_t checked = ku_task_@ID@_check(handle);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(handle->ticket.driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(KuTaskAdapterOutcomeV1))
      || ku_task_frame_ranges_overlap(handle, sizeof(*handle), output, sizeof(*output))
      || ku_task_frame_ranges_overlap(handle->owner.lease.control, sizeof(KuTaskInstance_@ID@), output, sizeof(*output)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  /* R3 notifies only AFTER R2 has published TAKEN or restored AVAILABLE. Deep
   * payload aliases are checked inside the exclusive R2 take callback. */
  KuTaskAdapterTakeRequestV1 request={output,NULL};
  return ku_task_driver_take_result(&handle->ticket, &handle->owner.lease, &request);
}
static uint32_t ku_task_@ID@_drop(KuTaskHandle_@ID@* handle, uint64_t absolute_deadline) {
  uint32_t checked = ku_task_@ID@_check(handle);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  checked = ku_task_driver_owner_drop(&handle->ticket, &handle->owner, absolute_deadline);
  if (checked == KU_TASK_DRIVER_OK) handle->ticket = (KuTaskDriverTicketV1){0};
  return checked;
}
static uint32_t ku_task_@ID@_discard_private(KuTaskControlOwnerV1* owner) {
  /* Exclusively owned, never published, never resumed, no external leases.
   * Valid generated entry cleanup is finite and cannot return Pending. No
   * retry/ABORT registration, manual ref decrement or force-free bypasses R2. */
  KuTaskDriverV1* driver = ((KuTaskInstance_@ID@*)owner->lease.control)->ticket.driver;
  uint32_t status = ku_task_control_request_cancel(&owner->lease, KU_TASK_CONTROL_CANCELLED, 0);
  if (status != KU_TASK_CONTROL_OK) { ku_task_adapter_fault(driver, 0); return KU_TASK_DRIVER_INTERNAL; }
  status = ku_task_control_poll(&owner->lease);
  if (status != KU_TASK_CONTROL_CANCELLED) { ku_task_adapter_fault(driver, 0); return KU_TASK_DRIVER_INTERNAL; }
  status = ku_task_control_owner_drop(owner, 0);
  if (status != KU_TASK_CONTROL_OK) { ku_task_adapter_fault(driver, 0); return KU_TASK_DRIVER_INTERNAL; }
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_@ID@_try_start_impl(KuTaskDriverV1* driver,
    const KuTaskDriverStartChargeV1* source@PARAM_DECLS@, KuTaskHandle_@ID@* output) {
  uint32_t checked = ku_task_driver_check(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(KuTaskHandle_@ID@)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (source && (!ku_task_driver_external_storage(driver,source,sizeof(*source),KU_TASK_FRAME_ALIGNOF(KuTaskDriverStartChargeV1))
      || ku_task_frame_ranges_overlap(source,sizeof(*source),output,sizeof(*output))))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
@PREFLIGHT@
@SOURCE_HEADERS@
  size_t charge = sizeof(KuTaskInstance_@ID@);
@CHARGE@
@SOURCE_BUFFERS@
@CALLABLE_CHECKS@
  if (source && source->owned_bytes != charge-sizeof(KuTaskInstance_@ID@)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  /* Header/deep-alias rejection precedes typed output reads. A pointer into
   * an input int/string buffer need not actually contain a full Handle. */
  if (output->owner.lease.control || output->ticket.driver || output->ticket.slot || output->ticket.generation)
    return KU_TASK_DRIVER_INVALID_STATE;
  KuTaskDriverTicketV1 ticket = {0};
  checked = source ? ku_task_driver_reserve_child(driver, source, sizeof(KuTaskInstance_@ID@), &ticket)
      : ku_task_driver_reserve(driver, charge, &ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)calloc(1, sizeof(*instance));
  if (!instance) return ku_task_adapter_rollback(&ticket, KU_TASK_ADAPTER_OUT_OF_MEMORY);
  instance->ticket = ticket;
  KuTaskControlOwnerV1 owner = {0};
  checked = ku_task_control_init(&instance->control, sizeof(instance->control), KU_TASK_CONTROL_ABI_VERSION, &ku_task_@ID@_ops, instance, &owner);
  if (checked != KU_TASK_CONTROL_OK) {
    free(instance);
    return ku_task_adapter_rollback(&ticket, checked);
  }
  checked = ku_task_frame_@ID@_init(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION@PARAM_ARGS@);
  if (checked != KU_TASK_FRAME_OK) {
    uint32_t failure = ku_task_adapter_frame_status(checked);
    if (ku_task_@ID@_discard_private(&owner) != KU_TASK_DRIVER_OK) return KU_TASK_DRIVER_INTERNAL;
    return ku_task_adapter_rollback(&ticket, failure);
  }
  instance->frame_initialized = true;
  /* Publish immutable metadata BEFORE commit can make this instance runnable. */
  instance->registered = true;
  checked = source ? ku_task_driver_commit_child(&ticket, &owner, source, sizeof(*instance))
      : ku_task_driver_commit(&ticket, &owner, KU_TASK_DRIVER_START, UINT64_MAX);
  if (checked != KU_TASK_DRIVER_OK) {
    /* A failed R3 commit did not publish any registry/execution lease. Restore
     * only Owned entry arguments; Copy caller headers were never consumed. */
    instance->registered = false;
@RESTORE@
    if (ku_task_@ID@_discard_private(&owner) != KU_TASK_DRIVER_OK) return KU_TASK_DRIVER_INTERNAL;
    return ku_task_adapter_rollback(&ticket, checked);
  }
  /* No fallible operation remains. Local owner protects a fast completion. */
  output->owner = owner;
  output->ticket = ticket;
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_@ID@_try_start(KuTaskDriverV1* driver@PARAM_DECLS@, KuTaskHandle_@ID@* output) {
  return ku_task_@ID@_try_start_impl(driver,NULL@PARAM_ARGS@,output);
}
static uint32_t ku_task_@ID@_start_value_impl(KuTaskDriverV1* driver,
    const KuTaskDriverStartChargeV1* source@PARAM_DECLS@, KuTaskValueV1* output) {
  uint32_t checked=ku_task_driver_check(driver);
  if (checked!=KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(driver,output,sizeof(*output),KU_TASK_FRAME_ALIGNOF(KuTaskValueV1))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (source && (!ku_task_driver_external_storage(driver,source,sizeof(*source),KU_TASK_FRAME_ALIGNOF(KuTaskDriverStartChargeV1))
      || ku_task_frame_ranges_overlap(source,sizeof(*source),output,sizeof(*output))))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
@PREFLIGHT@
@SOURCE_HEADERS@
  size_t charge=0;
@CHARGE@
@SOURCE_BUFFERS@
@CALLABLE_CHECKS@
  (void)charge;
  if (output->tag || output->owner.lease.control || output->ticket.driver) return KU_TASK_DRIVER_INVALID_STATE;
  KuTaskHandle_@ID@ handle={0};
  uint32_t status=ku_task_@ID@_try_start_impl(driver,source@PARAM_ARGS@,&handle);
  if (status!=KU_TASK_DRIVER_OK) {
    if (status!=KU_TASK_DRIVER_LIMIT && status!=KU_TASK_DRIVER_CLOSED && status!=KU_TASK_ADAPTER_OUT_OF_MEMORY) return status;
@REJECTED_DROPS@
    output->tag=KU_TASK_VALUE_INLINE_FAILED; output->result_kind=@KIND@u; output->function_id=@ID@u; output->rejection_code=status;
    return KU_TASK_DRIVER_OK;
  }
  output->tag=KU_TASK_VALUE_LIVE; output->result_kind=@KIND@u; output->function_id=@ID@u;
  output->owner=handle.owner; output->ticket=handle.ticket;
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_@ID@_start_value(KuTaskDriverV1* driver@PARAM_DECLS@, KuTaskValueV1* output) {
  return ku_task_@ID@_start_value_impl(driver,NULL@PARAM_ARGS@,output);
}
static uint32_t ku_task_@ID@_start_hosted(const KuTaskAdapterHostV1* host@PARAM_DECLS@, KuTaskValueV1* output) {
  KuTaskDriverStartChargeV1 source={0};
  /* The raw caller provides complete readable typed headers and vector
   * extents. Reject known header/base aliases first, then discover the host's
   * parent/control pointers. Array leaves are checked against those ranges
   * before parent fields are read; driver backing becomes known afterwards.
   * Output fields are inspected only after the final complete input guard.
   * Integer-range checks do not establish arbitrary pointer mappings. */
  if (!ku_task_frame_storage_valid(host,sizeof(*host),sizeof(*host),KU_TASK_FRAME_ALIGNOF(KuTaskAdapterHostV1))
      || !ku_task_frame_storage_valid(output,sizeof(*output),sizeof(*output),KU_TASK_FRAME_ALIGNOF(KuTaskValueV1))
      || ku_task_frame_ranges_overlap(host,sizeof(*host),output,sizeof(*output))
      || ku_task_frame_ranges_overlap(&source,sizeof(source),host,sizeof(*host))
      || ku_task_frame_ranges_overlap(&source,sizeof(source),output,sizeof(*output)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
@HOSTED_HEADERS@
  uint32_t checked=KU_TASK_DRIVER_OK;
  size_t charge=0;
@HOSTED_CHARGE@
  /* The raw caller supplies a complete mapped host header. Array vectors are
   * checked against known ranges before leaf metadata. parent/control pointer
   * values become known here; their deep aliases are rejected before reading
   * parent fields. Driver backing ranges are known only after that read and
   * are rechecked in the final pass. Integer ranges are not a mapping oracle. */
  const KuTaskDriverTicketV1* parent=host->ticket;
  if (!ku_task_frame_storage_valid(parent,sizeof(*parent),sizeof(*parent),KU_TASK_FRAME_ALIGNOF(KuTaskDriverTicketV1))
      || !ku_task_frame_storage_valid(host->control,sizeof(KuTaskControlV1),sizeof(KuTaskControlV1),KU_TASK_FRAME_ALIGNOF(KuTaskControlV1))
      || ku_task_frame_ranges_overlap(parent,sizeof(*parent),host,sizeof(*host))
      || ku_task_frame_ranges_overlap(parent,sizeof(*parent),output,sizeof(*output))
      || ku_task_frame_ranges_overlap(parent,sizeof(*parent),&source,sizeof(source))
      || ku_task_frame_ranges_overlap(host->control,sizeof(KuTaskControlV1),parent,sizeof(*parent))
      || ku_task_frame_ranges_overlap(host->control,sizeof(KuTaskControlV1),host,sizeof(*host))
      || ku_task_frame_ranges_overlap(host->control,sizeof(KuTaskControlV1),output,sizeof(*output))
      || ku_task_frame_ranges_overlap(host->control,sizeof(KuTaskControlV1),&source,sizeof(source))
      || !host->minimum_charge) return KU_TASK_DRIVER_INVALID_ARGUMENT;
@HOSTED_TICKET_ALIASES@
  KuTaskDriverV1* driver=parent->driver;
  checked=ku_task_driver_check(driver);
  if (checked!=KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(driver,host,sizeof(*host),KU_TASK_FRAME_ALIGNOF(KuTaskAdapterHostV1))
      || !ku_task_driver_external_storage(driver,parent,sizeof(*parent),KU_TASK_FRAME_ALIGNOF(KuTaskDriverTicketV1))
      || !ku_task_driver_external_storage(driver,host->control,sizeof(KuTaskControlV1),KU_TASK_FRAME_ALIGNOF(KuTaskControlV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
@HOSTED_RUNTIME_INPUTS@
@CALLABLE_CHECKS@
  source=(KuTaskDriverStartChargeV1){*parent,host->control,host->minimum_charge,charge};
  return ku_task_@ID@_start_value_impl(driver,&source@PARAM_ARGS@,output);
}
"#;

// This gate precedes every R2 acknowledgement, including an expired child-drain
// deadline. It does not postpone finally/Value cleanup or renew that deadline.
const BLOCKING_CLEANUP: &str = r#"
  if (instance->blocking.pool) {
    uint32_t retired=ku_task_blocking_discard(&instance->ticket,&instance->blocking);
    if (retired==KU_TASK_DRIVER_PENDING) return KU_TASK_CONTROL_PENDING;
    if (retired!=KU_TASK_DRIVER_OK
        || ku_task_@ID@_reconcile_charge(instance)!=KU_TASK_DRIVER_OK)
      return KU_TASK_CONTROL_INVALID_STATE;
  }
"#;
