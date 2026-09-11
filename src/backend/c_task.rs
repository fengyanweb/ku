//! Internal verified task-frame emitter, not a public Ku Task ABI.
//!
//! The caller supplies zeroed, aligned storage and serializes every operation.
//! A Pending frame must traverse its verified cleanup CFG before destruction.
//! Hosted frames use generated child Start/Await and adapter-owned scope drain.
//! User cleanup remains finite and cannot await; source admission stays separate.

use std::collections::HashSet;

use crate::error::KuResult;
use crate::ir::task::{
    SlotId, TaskBinaryOp, TaskConstant, TaskFramePlan, TaskFunction, TaskFunctionFrame, TaskLimits,
    TaskOp, TaskProgram, TaskScopeFrame, TaskSlotType, TaskTerminator, TaskUnaryOp,
};
use crate::ir::IrType;

use super::output::COutput;
use super::{
    c_drop_value, c_move_value, c_static_utf8_string, c_type, c_type_suffix, c_zero_initializer,
    unsupported,
};

const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_FRAME_SLOTS: usize = 64;

pub(super) fn uses_checked_integer(tasks: &TaskProgram) -> bool {
    tasks.functions.iter().any(|function| {
        function.states.iter().any(|state| {
            state.operations.iter().any(|operation| {
                matches!(
                    operation,
                    TaskOp::Unary {
                        op: TaskUnaryOp::Negate,
                        ..
                    } | TaskOp::Binary {
                        op: TaskBinaryOp::Add
                            | TaskBinaryOp::Subtract
                            | TaskBinaryOp::Multiply
                            | TaskBinaryOp::Divide
                            | TaskBinaryOp::Remainder,
                        ..
                    }
                )
            })
        })
    })
}

pub(super) fn emit_frames(
    out: &mut COutput,
    tasks: &TaskProgram,
    plan: &TaskFramePlan,
) -> KuResult<()> {
    if tasks.functions.is_empty() {
        return Ok(());
    }
    out.check()?;
    out.push_str(FRAME_ABI);
    super::task_control::emit_runtime(out)?;
    super::task_driver::emit_runtime(out)?;
    super::task_adapter::emit_host(out, tasks)?;
    for function in &tasks.functions {
        out.check()?;
        let frame = plan
            .functions
            .iter()
            .find(|frame| frame.function == function.id)
            .ok_or_else(|| unsupported("native task frame plan is missing a function"))?;
        FrameEmitter::new(function, frame)?.emit(out)?;
    }
    super::task_adapter::emit_adapters(out, tasks)?;
    out.check()
}

struct FrameEmitter<'a> {
    function: &'a TaskFunction,
    persistent: HashSet<usize>,
    prefix: String,
    frame_type: String,
    task_mask: u64,
    hosted: bool,
    exit_bridge: bool,
    scopes: &'a [TaskScopeFrame],
}

impl<'a> FrameEmitter<'a> {
    fn new(function: &'a TaskFunction, frame: &'a TaskFunctionFrame) -> KuResult<Self> {
        if function.slots.len() > MAX_FRAME_SLOTS {
            return Err(unsupported("native task frame exceeds its 64-slot bitmap"));
        }
        let persistent = frame
            .slots
            .iter()
            .map(|slot| slot.0)
            .collect::<HashSet<_>>();
        if persistent.len() != frame.slots.len()
            || persistent.iter().any(|slot| *slot >= function.slots.len())
            || function
                .parameters
                .iter()
                .any(|slot| !persistent.contains(&slot.0))
        {
            return Err(unsupported(
                "native task frame plan has invalid storage slots",
            ));
        }
        // These scans use the verifier's existing bounded slot/state lists;
        // the bridge adds no per-instance storage or recursive ownership tree.
        let exit_bridge = function
            .states
            .iter()
            .any(|state| matches!(state.terminator, TaskTerminator::Exit { .. }));
        let mut task_mask = 0u64;
        for (index, slot) in function.slots.iter().enumerate() {
            match &slot.ty {
                TaskSlotType::Value { ty, borrowed } => {
                    require_slot_type(ty)?;
                    if *borrowed && persistent.contains(&index) {
                        return Err(unsupported(
                            "borrowed values cannot enter a native task frame",
                        ));
                    }
                    if frame.hosted
                        && !*borrowed
                        && matches!(ty, IrType::Str | IrType::Result(_))
                        && !persistent.contains(&index)
                    {
                        return Err(unsupported(
                            "hosted owned values must remain in native task frame storage",
                        ));
                    }
                }
                TaskSlotType::Task { result } => {
                    require_result_type(result)?;
                    task_mask |= 1u64 << index;
                    if !persistent.contains(&index) {
                        return Err(unsupported("Task slots must be persistent"));
                    }
                }
            }
        }
        if frame.scope_task_mask != task_mask
            || frame.exit_bridge != exit_bridge
            || frame.hosted != (task_mask != 0 || exit_bridge)
        {
            return Err(unsupported(
                "native task frame plan has inconsistent exit ownership",
            ));
        }
        Self::validate_scope_plan(function, frame, task_mask, exit_bridge)?;
        require_result_type(&function.result)?;
        Ok(Self {
            function,
            persistent,
            prefix: format!("ku_task_frame_{}", function.id.0),
            frame_type: format!("KuTaskFrame_{}", function.id.0),
            task_mask,
            hosted: frame.hosted,
            exit_bridge,
            scopes: &frame.scopes,
        })
    }

    fn validate_scope_plan(
        function: &TaskFunction,
        frame: &TaskFunctionFrame,
        task_mask: u64,
        exit_bridge: bool,
    ) -> KuResult<()> {
        // The verifier already bounded these lists. Recheck exported descriptors
        // before indexing or allocating from a hand-supplied internal plan.
        if frame.scopes.len() > TaskLimits::default().max_operations {
            return Err(unsupported(
                "native task scope plan exceeds its operation bound",
            ));
        }
        if !frame.scopes.is_empty()
            && (!exit_bridge
                || function
                    .states
                    .iter()
                    .any(|state| matches!(state.terminator, TaskTerminator::Complete { .. })))
        {
            return Err(unsupported("native scoped frames require typed Exit"));
        }
        let mut associated = 0u64;
        for (index, descriptor) in frame.scopes.iter().enumerate() {
            if descriptor.scope.0 != index
                || descriptor.task_mask & !task_mask != 0
                || descriptor.task_mask & associated != 0
            {
                return Err(unsupported(
                    "native task scope plan has invalid ownership masks",
                ));
            }
            associated |= descriptor.task_mask;
        }
        let mut declared = vec![false; frame.scopes.len()];
        for state in &function.states {
            for operation in &state.operations {
                if let TaskOp::ScopeEnter { scope, tasks } = operation {
                    let descriptor = frame.scopes.get(scope.0).ok_or_else(|| {
                        unsupported("native task scope declaration is missing from its plan")
                    })?;
                    if declared[scope.0] || tasks.len() > MAX_FRAME_SLOTS {
                        return Err(unsupported("native task scope declaration is not unique"));
                    }
                    let mut actual = 0u64;
                    for slot in tasks {
                        if slot.0 >= function.slots.len()
                            || task_mask & (1u64 << slot.0) == 0
                            || actual & (1u64 << slot.0) != 0
                        {
                            return Err(unsupported(
                                "native task scope declaration has invalid members",
                            ));
                        }
                        actual |= 1u64 << slot.0;
                    }
                    if actual != descriptor.task_mask {
                        return Err(unsupported(
                            "native task scope plan disagrees with its declaration",
                        ));
                    }
                    declared[scope.0] = true;
                }
            }
            if let TaskTerminator::ScopeDrain {
                scope,
                ready,
                cleanup,
            } = state.terminator
            {
                if frame.scopes.get(scope.0).is_none()
                    || ready.0 >= function.states.len()
                    || cleanup.0 >= function.states.len()
                {
                    return Err(unsupported(
                        "native task scope request has an invalid descriptor",
                    ));
                }
            }
        }
        if declared.iter().any(|seen| !seen) {
            return Err(unsupported(
                "native task scope plan contains an undeclared scope",
            ));
        }
        Ok(())
    }

    fn slot_type(&self, slot: SlotId) -> KuResult<&IrType> {
        let slot = self
            .function
            .slots
            .get(slot.0)
            .ok_or_else(|| unsupported("native task operation references a missing slot"))?;
        match &slot.ty {
            TaskSlotType::Value { ty, .. } => Ok(ty),
            TaskSlotType::Task { result } => Ok(result),
        }
    }

    fn owns_slot(&self, slot: SlotId) -> bool {
        match &self.function.slots[slot.0].ty {
            TaskSlotType::Task { .. } => true,
            TaskSlotType::Value { ty, borrowed } => {
                !*borrowed && matches!(ty, IrType::Str | IrType::Result(_))
            }
        }
    }

    fn task_slot(&self, slot: SlotId) -> bool {
        self.task_mask & (1u64 << slot.0) != 0
    }

    fn place(&self, slot: SlotId) -> String {
        if self.persistent.contains(&slot.0) {
            format!("frame->s_{}", slot.0)
        } else {
            format!("slot_{}", slot.0)
        }
    }

    fn bitmap(&self, slot: SlotId) -> &'static str {
        if self.persistent.contains(&slot.0) {
            "frame->header.initialized"
        } else {
            "local_initialized"
        }
    }

    fn bit(slot: SlotId) -> String {
        format!("(UINT64_C(1) << {})", slot.0)
    }

    fn set_init(&self, out: &mut COutput, slot: SlotId, initialized: bool) {
        out.push_str(&format!(
            "  {} {} {};\n",
            self.bitmap(slot),
            if initialized { "|=" } else { "&= ~" },
            Self::bit(slot)
        ));
    }

    fn emit(&self, out: &mut COutput) -> KuResult<()> {
        let prefix = &self.prefix;
        let frame_type = &self.frame_type;
        out.push_str(&format!(
            "typedef struct {frame_type} {{\n  KuTaskFrameHeaderV1 header;\n"
        ));
        for (index, slot) in self.function.slots.iter().enumerate() {
            out.check()?;
            if self.persistent.contains(&index) {
                let ty = match &slot.ty {
                    TaskSlotType::Value { ty, .. } => c_type(ty)?,
                    TaskSlotType::Task { .. } => "KuTaskValueV1".to_string(),
                };
                out.push_str(&format!("  {ty} s_{index};\n"));
            }
        }
        out.push_str(&format!(
            "  {} result;\n}} {frame_type};\n\
             static size_t {prefix}_size(void) {{ return sizeof({frame_type}); }}\n\
             static size_t {prefix}_align(void) {{ return KU_TASK_FRAME_ALIGNOF({frame_type}); }}\n",
            c_type(&self.function.result)?
        ));
        self.emit_scope_descriptors(out)?;
        self.emit_check(out);
        self.emit_scope_helpers(out)?;
        self.emit_init(out)?;
        self.emit_drive(out)?;
        self.emit_resume(out);
        self.emit_finish_exit_values(out)?;
        self.emit_terminate(out)?;
        self.emit_take_result(out)?;
        self.emit_destroy(out)?;
        out.check()
    }

    fn emit_scope_descriptors(&self, out: &mut COutput) -> KuResult<()> {
        if self.scopes.is_empty() {
            return Ok(());
        }
        let prefix = &self.prefix;
        for (index, state) in self.function.states.iter().enumerate() {
            out.check()?;
            if let TaskTerminator::ScopeDrain {
                scope,
                ready,
                cleanup,
            } = state.terminator
            {
                let descriptor = &self.scopes[scope.0];
                out.push_str(&format!(
                    "static const KuTaskFrameScopeDescriptorV1 {prefix}_scope_{index} = {{ UINT64_C({scope_id}), UINT64_C({mask}), {index}u, {ready}u, {cleanup}u }};\n",
                    scope_id = descriptor.scope.0,
                    mask = descriptor.task_mask,
                    ready = ready.0,
                    cleanup = cleanup.0,
                ));
            }
        }
        out.push_str(&format!(
            "static const KuTaskFrameScopeDescriptorV1* {prefix}_scope_lookup(uint32_t state) {{\n  switch (state) {{\n"
        ));
        for (index, state) in self.function.states.iter().enumerate() {
            out.check()?;
            if matches!(state.terminator, TaskTerminator::ScopeDrain { .. }) {
                out.push_str(&format!(
                    "    case {index}u: return &{prefix}_scope_{index};\n"
                ));
            }
        }
        out.push_str("    default: return NULL;\n  }\n}\n");
        out.check()
    }

    fn emit_scope_helpers(&self, out: &mut COutput) -> KuResult<()> {
        if self.scopes.is_empty() {
            return Ok(());
        }
        let prefix = &self.prefix;
        let frame_type = &self.frame_type;
        out.check()?;
        out.push_str(&format!(
            r#"/* Non-owning static descriptor: output is a complete NULL-initialized
 * pointer slot. The exclusive raw caller also keeps it disjoint from every
 * Owned deep allocation; a frame interval check is not a hostile-pointer oracle. */
static uint32_t {prefix}_scope_request(void* storage, size_t bytes, uint32_t abi,
    const KuTaskFrameScopeDescriptorV1** output) {{
  uint32_t checked = {prefix}_check(storage, bytes, abi);
  if (checked != KU_TASK_FRAME_OK) return checked;
  if (!ku_task_frame_storage_valid(output, sizeof(*output), sizeof(*output),
          KU_TASK_FRAME_ALIGNOF(const KuTaskFrameScopeDescriptorV1*))
      || ku_task_frame_ranges_overlap(storage, bytes, output, sizeof(*output)))
    return KU_TASK_FRAME_INVALID_ARGUMENT;
  if (*output) return KU_TASK_FRAME_INVALID_ARGUMENT;
  {frame_type}* frame = ({frame_type}*)storage;
  if (frame->header.status != KU_TASK_FRAME_SCOPE_REQUEST) return KU_TASK_FRAME_INVALID_STATE;
  const KuTaskFrameScopeDescriptorV1* descriptor = {prefix}_scope_lookup(frame->header.state);
  if (!descriptor) return KU_TASK_FRAME_INVALID_STATE;
  *output = descriptor;
  return KU_TASK_FRAME_OK;
}}
/* Only the generated adapter supplies the actual successful end/empty witness.
 * No user code, clock, drop, control transition or recursive drive occurs here. */
static uint32_t {prefix}_scope_continue(void* storage, size_t bytes, uint32_t abi,
    uint64_t expected_scope_id) {{
  const KuTaskFrameScopeDescriptorV1* descriptor = NULL;
  uint32_t checked = {prefix}_scope_request(storage, bytes, abi, &descriptor);
  if (checked != KU_TASK_FRAME_OK) return checked;
  {frame_type}* frame = ({frame_type}*)storage;
  if (descriptor->scope_id != expected_scope_id
      || (frame->header.initialized & descriptor->task_mask))
    return KU_TASK_FRAME_INVALID_STATE;
  frame->header.state = descriptor->ready_state;
  frame->header.cleanup_state = UINT32_MAX;
  frame->header.status = KU_TASK_FRAME_PENDING;
  return KU_TASK_FRAME_OK;
}}
/* The adapter supplies a real scope-timeout witness and its original absolute D.
 * This helper never reads time or manufactures cancellation authority. */
static uint32_t {prefix}_scope_timeout(void* storage, size_t bytes, uint32_t abi,
    uint64_t expected_scope_id, uint64_t original_deadline) {{
  const KuTaskFrameScopeDescriptorV1* descriptor = NULL;
  uint32_t checked = {prefix}_scope_request(storage, bytes, abi, &descriptor);
  if (checked != KU_TASK_FRAME_OK) return checked;
  if (original_deadline == UINT64_MAX) return KU_TASK_FRAME_INVALID_ARGUMENT;
  {frame_type}* frame = ({frame_type}*)storage;
  if (descriptor->scope_id != expected_scope_id || frame->header.result_initialized)
    return KU_TASK_FRAME_INVALID_STATE;
"#
        ));
        if !self.exit_bridge {
            out.push_str("  return KU_TASK_FRAME_INVALID_STATE;\n}\n");
            return out.check();
        }
        let IrType::Result(inner) = &self.function.result else {
            return Err(unsupported("scope timeout requires Result"));
        };
        out.push_str(&format!(
            "  frame->result = ({}){{ false, {}, ku_error_make(ku_string_static((const uint8_t*)\"task\",4),ku_string_static((const uint8_t*)\"shutdown_timeout\",16),ku_string_static((const uint8_t*)\"owned child cleanup deadline expired\",36)) }};\n",
            c_type(&self.function.result)?,
            c_zero_initializer(inner)?,
        ));
        out.push_str(
            "  frame->header.result_initialized = 1;\n\
               frame->header.exit_class = KU_TASK_EXIT_RUNTIME_FAILURE;\n\
               frame->header.has_exit_deadline = 1;\n\
               frame->header.exit_deadline = original_deadline;\n",
        );
        self.emit_exit_staged(out);
        out.push_str("}\n");
        out.check()
    }

    fn emit_check(&self, out: &mut COutput) {
        let prefix = &self.prefix;
        let frame_type = &self.frame_type;
        let scope_check = if self.scopes.is_empty() {
            // No scope helper symbols or lookup are emitted in this function.
            "if (frame->header.status == KU_TASK_FRAME_SCOPE_REQUEST) return KU_TASK_FRAME_INVALID_STATE;".to_string()
        } else {
            format!(
                "if (frame->header.status == KU_TASK_FRAME_SCOPE_REQUEST) {{\n\
                   const KuTaskFrameScopeDescriptorV1* descriptor = {prefix}_scope_lookup(frame->header.state);\n\
                   if (!descriptor || descriptor->request_state != frame->header.state || frame->header.cleanup_state != descriptor->cleanup_state || frame->header.result_initialized || frame->header.exit_class || frame->header.has_exit_deadline || frame->header.exit_deadline || frame->header.cleanup_deadline_ms || frame->header.cleanup_timed_out) return KU_TASK_FRAME_INVALID_STATE;\n\
                 }}"
            )
        };
        out.push_str(&format!(
            "static uint32_t {prefix}_check(void* storage, size_t bytes, uint32_t abi) {{\n\
               if (abi != KU_TASK_FRAME_ABI_VERSION) return KU_TASK_FRAME_ABI_MISMATCH;\n\
               if (sizeof({frame_type}) > {MAX_FRAME_BYTES}u) return KU_TASK_FRAME_LIMIT;\n\
               if (!ku_task_frame_storage_valid(storage, bytes, sizeof({frame_type}), {prefix}_align())) return KU_TASK_FRAME_INVALID_STORAGE;\n\
               {frame_type}* frame = ({frame_type}*)storage;\n\
               if (frame->header.abi_version != KU_TASK_FRAME_ABI_VERSION) return KU_TASK_FRAME_ABI_MISMATCH;\n\
               if (frame->header.storage_size != sizeof({frame_type}) || frame->header.function_id != UINT64_C({id})) return KU_TASK_FRAME_INVALID_STORAGE;\n\
               if (frame->header.running || (frame->header.status > KU_TASK_FRAME_TIMED_OUT && frame->header.status != KU_TASK_FRAME_EXIT_STAGED && frame->header.status != KU_TASK_FRAME_SCOPE_REQUEST)) return KU_TASK_FRAME_INVALID_STATE;\n\
               if (frame->header.status == KU_TASK_FRAME_EXIT_STAGED && (!{exit_bridge} || frame->header.result_initialized != 1u || frame->header.cleanup_state != UINT32_MAX || (frame->header.exit_class != KU_TASK_EXIT_USER_RESULT && frame->header.exit_class != KU_TASK_EXIT_RUNTIME_FAILURE) || frame->header.has_exit_deadline > 1u || (!frame->header.has_exit_deadline && frame->header.exit_deadline) || (frame->header.has_exit_deadline && frame->header.exit_deadline == UINT64_MAX) || (frame->header.exit_class == KU_TASK_EXIT_USER_RESULT && (frame->header.has_exit_deadline || frame->header.exit_deadline)))) return KU_TASK_FRAME_INVALID_STATE;\n\
               if (frame->header.state >= {states}u || (frame->header.cleanup_state != UINT32_MAX && frame->header.cleanup_state >= {states}u)) return KU_TASK_FRAME_INVALID_STATE;\n\
               {scope_check}\n\
               return KU_TASK_FRAME_OK;\n\
             }}\n",
            id = self.function.id.0,
            states = self.function.states.len(),
            exit_bridge = if self.exit_bridge { "1" } else { "0" }
        ));
    }

    fn emit_init(&self, out: &mut COutput) -> KuResult<()> {
        let prefix = &self.prefix;
        let frame_type = &self.frame_type;
        out.push_str(&format!(
            "static uint32_t {prefix}_init(void* storage, size_t bytes, uint32_t abi"
        ));
        for (index, slot) in self.function.parameters.iter().enumerate() {
            out.push_str(&format!(
                ", {}* arg_{index}",
                c_type(self.slot_type(*slot)?)?
            ));
        }
        out.push_str(&format!(
            ") {{\n\
               if (abi != KU_TASK_FRAME_ABI_VERSION) return KU_TASK_FRAME_ABI_MISMATCH;\n\
               if (sizeof({frame_type}) > {MAX_FRAME_BYTES}u) return KU_TASK_FRAME_LIMIT;\n\
               if (!ku_task_frame_storage_valid(storage, bytes, sizeof({frame_type}), {prefix}_align())) return KU_TASK_FRAME_INVALID_STORAGE;\n\
               if (!ku_task_frame_zero_bytes(storage, sizeof({frame_type}))) return KU_TASK_FRAME_INVALID_STATE;\n"
        ));
        // All argument checks precede the first write/move. Header disjointness
        // is checked here; disjoint deep owned payloads remain the raw caller's
        // responsibility, just as for the existing generated native value ABI.
        for (index, slot) in self.function.parameters.iter().enumerate() {
            let ty = c_type(self.slot_type(*slot)?)?;
            out.push_str(&format!(
                "  if (!ku_task_frame_storage_valid(arg_{index}, sizeof({ty}), sizeof({ty}), KU_TASK_FRAME_ALIGNOF({ty})) || ku_task_frame_ranges_overlap(storage, bytes, arg_{index}, sizeof({ty}))) return KU_TASK_FRAME_INVALID_ARGUMENT;\n"
            ));
            for (previous, previous_slot) in self.function.parameters[..index].iter().enumerate() {
                if self.owns_slot(*slot) || self.owns_slot(*previous_slot) {
                    let previous_ty = c_type(self.slot_type(*previous_slot)?)?;
                    out.push_str(&format!(
                        "  if (ku_task_frame_ranges_overlap(arg_{index}, sizeof({ty}), arg_{previous}, sizeof({previous_ty}))) return KU_TASK_FRAME_INVALID_ARGUMENT;\n"
                    ));
                }
            }
        }
        out.push_str(&format!(
            "  {frame_type}* frame = ({frame_type}*)storage;\n\
               frame->header.abi_version = KU_TASK_FRAME_ABI_VERSION;\n\
               frame->header.storage_size = sizeof({frame_type});\n\
               frame->header.function_id = UINT64_C({id});\n\
               frame->header.state = {entry}u;\n\
               frame->header.cleanup_state = UINT32_MAX;\n",
            id = self.function.id.0,
            entry = self.function.entry.0
        ));
        for (index, slot) in self.function.parameters.iter().enumerate() {
            out.push_str(&format!(
                "  {} = {};\n",
                self.place(*slot),
                c_move_value(self.slot_type(*slot)?, &format!("(*arg_{index})"))?
            ));
            self.set_init(out, *slot, true);
        }
        out.push_str("  return KU_TASK_FRAME_OK;\n}\n");
        Ok(())
    }

    fn emit_drive(&self, out: &mut COutput) -> KuResult<()> {
        out.push_str(&format!(
            "static uint32_t {}_drive({}* frame, const KuTaskFrameClockV1* clock, int cleanup) {{\n  uint64_t local_initialized = 0;\n",
            self.prefix, self.frame_type
        ));
        for (index, slot) in self.function.slots.iter().enumerate() {
            out.check()?;
            if !self.persistent.contains(&index) {
                let TaskSlotType::Value { ty, .. } = &slot.ty else {
                    return Err(unsupported("Task slots must be persistent"));
                };
                out.push_str(&format!(
                    "  {} slot_{index} = {};\n",
                    c_type(ty)?,
                    c_zero_initializer(ty)?
                ));
            }
        }
        out.push_str(
            "  goto ku_task_dispatch;\nku_task_dispatch:;\n\
               if (cleanup && clock->now_ms(clock->context) >= frame->header.cleanup_deadline_ms) {\n\
                 frame->header.cleanup_timed_out = 1;\n\
                 goto ku_task_terminated;\n\
               }\n",
        );
        if self.hosted {
            out.push_str("  if (!cleanup && ku_task_control_atomic_load(&((KuTaskAdapterHostV1*)clock->host)->control->phase)!=KU_TASK_CONTROL_LIVE) { /* Mid-drive facts need bitmap cleanup, not an older suspension CFG. */ frame->header.cleanup_state=UINT32_MAX; frame->header.status=KU_TASK_FRAME_PENDING; frame->header.running=0; return KU_TASK_FRAME_PENDING; }\n");
        }
        out.push_str("  switch (frame->header.state) {\n");
        for index in 0..self.function.states.len() {
            out.push_str(&format!("  case {index}u: goto ku_task_state_{index};\n"));
        }
        out.push_str(
            "  default: frame->header.running = 0; return KU_TASK_FRAME_INVALID_STATE;\n  }\n",
        );
        for (index, state) in self.function.states.iter().enumerate() {
            out.check()?;
            out.push_str(&format!("ku_task_state_{index}:;\n"));
            for operation in &state.operations {
                out.check()?;
                self.emit_operation(out, operation)?;
            }
            match &state.terminator {
                TaskTerminator::Jump { target } => {
                    out.push_str(&format!(
                        "  frame->header.state = {}u;\n  goto ku_task_dispatch;\n",
                        target.0
                    ));
                }
                TaskTerminator::Branch {
                    condition,
                    then_state,
                    else_state,
                } => {
                    out.push_str(&format!(
                        "  frame->header.state = {} ? {}u : {}u;\n  goto ku_task_dispatch;\n",
                        self.place(*condition),
                        then_state.0,
                        else_state.0
                    ));
                }
                TaskTerminator::Suspend { resume, cleanup } => {
                    out.push_str(&format!(
                        "  if (cleanup) goto ku_task_terminated;\n\
                           frame->header.state = {}u;\n\
                           frame->header.cleanup_state = {}u;\n\
                           frame->header.status = KU_TASK_FRAME_PENDING;\n\
                           frame->header.running = 0;\n\
                           return KU_TASK_FRAME_PENDING;\n",
                        resume.0, cleanup.0
                    ));
                }
                TaskTerminator::ScopeDrain { .. } => {
                    out.push_str(&format!(
                        "  if (cleanup) goto ku_task_terminated;\n  {{ const KuTaskFrameScopeDescriptorV1* descriptor={}_scope_lookup(frame->header.state);\n  if (!descriptor) {{ frame->header.running=0; return KU_TASK_FRAME_INVALID_STATE; }}\n  frame->header.cleanup_state=descriptor->cleanup_state;\n  frame->header.status=KU_TASK_FRAME_SCOPE_REQUEST;\n  frame->header.running=0;\n  return KU_TASK_FRAME_SCOPE_REQUEST;\n  }}\n",
                        self.prefix
                    ));
                }
                TaskTerminator::Complete { value } => {
                    out.push_str("  if (cleanup) goto ku_task_terminated;\n");
                    out.push_str(&format!(
                        "  frame->result = {};\n",
                        c_move_value(self.slot_type(*value)?, &self.place(*value))?
                    ));
                    self.set_init(out, *value, false);
                    out.push_str("  frame->header.result_initialized = 1;\n");
                    self.emit_all_slot_drops(out, true)?;
                    out.push_str(
                        "  frame->header.status = KU_TASK_FRAME_READY;\n\
                           frame->header.running = 0;\n\
                           return KU_TASK_FRAME_READY;\n",
                    );
                }
                TaskTerminator::Exit { value } => {
                    out.push_str("  if (cleanup) goto ku_task_terminated;\n");
                    out.push_str(&format!(
                        "  frame->result = {};\n",
                        c_move_value(self.slot_type(*value)?, &self.place(*value))?
                    ));
                    self.set_init(out, *value, false);
                    out.push_str(
                        "  frame->header.result_initialized = 1;\n\
                           frame->header.exit_class = KU_TASK_EXIT_USER_RESULT;\n\
                           frame->header.has_exit_deadline = 0;\n\
                           frame->header.exit_deadline = 0;\n",
                    );
                    self.emit_exit_staged(out);
                }
                TaskTerminator::TryResult {
                    src,
                    ok_value,
                    err_result,
                    ok,
                    err,
                } => {
                    let suffix = c_type_suffix(match self.slot_type(*src)? {
                        IrType::Result(inner) => inner,
                        _ => return Err(unsupported("TryResult requires Result")),
                    })?;
                    out.push_str(&format!(
                        "  {{ {} taken=ku_result_move_{suffix}(&{});\n",
                        c_type(self.slot_type(*src)?)?,
                        self.place(*src)
                    ));
                    self.set_init(out, *src, false);
                    out.push_str("  if (taken.ok) {\n");
                    out.push_str(&format!(
                        "    {}={};\n",
                        self.place(*ok_value),
                        c_move_value(self.slot_type(*ok_value)?, "taken.value")?
                    ));
                    self.set_init(out, *ok_value, true);
                    out.push_str(&format!("    frame->header.state={}u;\n  }} else {{\n    {}.error=ku_error_move(&taken.error);\n",ok.0,self.place(*err_result)));
                    self.set_init(out, *err_result, true);
                    out.push_str(&format!("    frame->header.state={}u;\n  }}\n  ku_result_drop_{suffix}(&taken); }}\n  goto ku_task_dispatch;\n",err.0));
                }
                TaskTerminator::Await {
                    task,
                    dst,
                    ready,
                    cleanup,
                } => {
                    let field = super::task_adapter::outcome_field(self.slot_type(*dst)?)?;
                    let IrType::Result(inner) = self.slot_type(*dst)? else {
                        return Err(unsupported("Await output must be Result"));
                    };
                    let suffix = c_type_suffix(inner)?;
                    out.push_str(&format!("  {{\n  frame->header.cleanup_state={}u;\n  KuTaskAdapterOutcomeV1 received={{0}};\n  uint32_t awaited=ku_task_host_await((KuTaskAdapterHostV1*)clock->host,&{},&received);\n  if (awaited==KU_TASK_CONTROL_PENDING) {{ frame->header.status=KU_TASK_FRAME_PENDING; frame->header.running=0; return KU_TASK_FRAME_PENDING; }}\n  if (awaited!=KU_TASK_CONTROL_OK) {{ ku_task_outcome_drop(&received); frame->header.running=0; return KU_TASK_FRAME_INVALID_STATE; }}\n",cleanup.0,self.place(*task)));
                    self.set_init(out, *task, false);
                    out.push_str("  if (received.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE) {\n    frame->header.has_exit_deadline=received.has_cleanup_deadline;\n    frame->header.exit_deadline=received.cleanup_deadline;\n");
                    self.emit_runtime_error(
                        out,
                        &format!("ku_error_move(&received.value.{field}.error)"),
                    )?;
                    out.push_str("  }\n");
                    out.push_str(&format!("  {}=ku_result_move_{suffix}(&received.value.{field});\n  ku_task_outcome_drop(&received);\n",self.place(*dst)));
                    self.set_init(out, *dst, true);
                    out.push_str(&format!(
                        "  frame->header.state={}u;\n  goto ku_task_dispatch;\n  }}\n",
                        ready.0
                    ));
                }
                TaskTerminator::Terminate => {
                    out.push_str(
                        "  if (!cleanup) { frame->header.running = 0; return KU_TASK_FRAME_INVALID_STATE; }\n  goto ku_task_terminated;\n",
                    );
                }
            }
        }
        out.push_str(
            "ku_task_terminated:;\n  if (clock->now_ms(clock->context) >= frame->header.cleanup_deadline_ms) frame->header.cleanup_timed_out = 1;\n",
        );
        self.emit_all_slot_drops(out, true)?;
        out.push_str(
            "  frame->header.running = 0;\n\
               return frame->header.status;\n}\n",
        );
        Ok(())
    }

    fn emit_operation(&self, out: &mut COutput, operation: &TaskOp) -> KuResult<()> {
        match operation {
            // Verified lexical ownership annotation; no runtime stack or bits.
            TaskOp::ScopeEnter { .. } => {}
            TaskOp::Unary { dst, op, src } => match op {
                TaskUnaryOp::Not => {
                    out.push_str(&format!(
                        "  {} = !{};\n",
                        self.place(*dst),
                        self.place(*src)
                    ));
                    self.set_init(out, *dst, true);
                }
                TaskUnaryOp::Negate => self.emit_checked_integer(
                    out,
                    *dst,
                    &format!("ku_int_neg({}, &{})", self.place(*src), self.place(*dst)),
                )?,
            },
            TaskOp::Binary {
                dst,
                op,
                left,
                right,
            } => {
                let helper = match op {
                    TaskBinaryOp::Add => Some("ku_int_add"),
                    TaskBinaryOp::Subtract => Some("ku_int_sub"),
                    TaskBinaryOp::Multiply => Some("ku_int_mul"),
                    TaskBinaryOp::Divide => Some("ku_int_div"),
                    TaskBinaryOp::Remainder => Some("ku_int_rem"),
                    _ => None,
                };
                if let Some(helper) = helper {
                    self.emit_checked_integer(
                        out,
                        *dst,
                        &format!(
                            "{helper}({}, {}, &{})",
                            self.place(*left),
                            self.place(*right),
                            self.place(*dst)
                        ),
                    )?;
                } else {
                    let operator = match op {
                        TaskBinaryOp::Equal => "==",
                        TaskBinaryOp::NotEqual => "!=",
                        TaskBinaryOp::Less => "<",
                        TaskBinaryOp::LessEqual => "<=",
                        TaskBinaryOp::Greater => ">",
                        TaskBinaryOp::GreaterEqual => ">=",
                        _ => return Err(unsupported("unknown Task comparison")),
                    };
                    out.push_str(&format!(
                        "  {} = ({} {operator} {});\n",
                        self.place(*dst),
                        self.place(*left),
                        self.place(*right)
                    ));
                    self.set_init(out, *dst, true);
                }
            }
            TaskOp::Init { dst, value } => {
                out.push_str(&format!(
                    "  {} = {};\n",
                    self.place(*dst),
                    constant_expr(value, self.slot_type(*dst)?)?
                ));
                self.set_init(out, *dst, true);
            }
            TaskOp::Copy { dst, src } => {
                out.push_str(&format!("  {} = {};\n", self.place(*dst), self.place(*src)));
                self.set_init(out, *dst, true);
            }
            TaskOp::Move { dst, src } => {
                if self.task_slot(*src) {
                    out.push_str(&format!("  if (ku_task_value_move(&{}, &{}) != KU_TASK_DRIVER_OK) {{ frame->header.running=0; return KU_TASK_FRAME_INVALID_STATE; }}\n",self.place(*dst),self.place(*src)));
                    self.set_init(out, *src, false);
                    self.set_init(out, *dst, true);
                    return Ok(());
                }
                out.push_str(&format!(
                    "  {} = {};\n",
                    self.place(*dst),
                    c_move_value(self.slot_type(*src)?, &self.place(*src))?
                ));
                self.set_init(out, *src, false);
                self.set_init(out, *dst, true);
            }
            TaskOp::Read { slot } => {
                out.push_str(&format!("  (void)({});\n", self.place(*slot)));
            }
            TaskOp::WrapOk { dst, src } => {
                let payload = if self.owns_slot(*src) {
                    c_move_value(self.slot_type(*src)?, &self.place(*src))?
                } else {
                    self.place(*src)
                };
                out.push_str(&format!(
                    "  {} = ({}){{ true, {}, (KuError){{0}} }};\n",
                    self.place(*dst),
                    c_type(self.slot_type(*dst)?)?,
                    payload,
                ));
                if self.owns_slot(*src) {
                    self.set_init(out, *src, false);
                }
                self.set_init(out, *dst, true);
            }
            TaskOp::Drop { slot } | TaskOp::DropIfInit { slot } => {
                self.emit_slot_drop(out, *slot)?;
            }
            TaskOp::Start {
                dst,
                function,
                arguments,
            } => {
                out.push_str(&format!("  {{ uint32_t started=ku_task_{}_start_hosted((const KuTaskAdapterHostV1*)clock->host",function.0));
                for argument in arguments {
                    out.push_str(&format!(", &{}", self.place(*argument)));
                }
                out.push_str(&format!(", &{});\n  if (started!=KU_TASK_DRIVER_OK) {{ frame->header.running=0; return KU_TASK_FRAME_INVALID_STATE; }} }}\n",self.place(*dst)));
                for argument in arguments {
                    if self.owns_slot(*argument) {
                        self.set_init(out, *argument, false);
                    }
                }
                self.set_init(out, *dst, true);
            }
            TaskOp::Print { value, newline } => {
                let place = self.place(*value);
                let expression = match self.slot_type(*value)? {
                    IrType::Int => format!("printf(\"%lld\",(long long){place}) < 0"),
                    IrType::Bool => format!("fputs({place} ? \"true\" : \"false\",stdout) == EOF"),
                    IrType::Null => "fputs(\"null\",stdout) == EOF".to_string(),
                    IrType::Str => format!(
                        "({place}.len && fwrite({place}.ptr,1,{place}.len,stdout)!={place}.len)"
                    ),
                    _ => return Err(unsupported("Task print requires primitive value")),
                };
                out.push_str(&format!(
                    "  if ({expression}{} || fflush(stdout)==EOF) {{\n",
                    if *newline {
                        " || fputc('\\n',stdout)==EOF"
                    } else {
                        ""
                    }
                ));
                self.emit_runtime_error(out,"ku_error_make(ku_string_static((const uint8_t*)\"io\",2),ku_string_static((const uint8_t*)\"write_failed\",12),ku_string_static((const uint8_t*)\"output write failed\",19))")?;
                out.push_str("  }\n");
            }
        }
        Ok(())
    }

    fn emit_checked_integer(&self, out: &mut COutput, dst: SlotId, call: &str) -> KuResult<()> {
        // Inputs are already materialized Copy slots. The helper neither exits
        // nor writes the destination on failure; cleanup follows the same
        // outer-runtime-failure path as native Task output errors.
        out.push_str(&format!(
            "  {{ uint32_t arithmetic_status = {call};\n  if (arithmetic_status != KU_INT_OK) {{\n"
        ));
        self.emit_runtime_error(out,
            "ku_error_make((KuString){0},(KuString){0},arithmetic_status==KU_INT_DIV_ZERO ? ku_string_static((const uint8_t*)\"division by zero\",16) : ku_string_static((const uint8_t*)\"integer overflow\",16))")?;
        out.push_str("  } }\n");
        self.set_init(out, dst, true);
        Ok(())
    }

    fn emit_runtime_error(&self, out: &mut COutput, error: &str) -> KuResult<()> {
        let IrType::Result(inner) = &self.function.result else {
            return Err(unsupported("runtime exit requires Result"));
        };
        if self.exit_bridge {
            // Cleanup already has a genuine R2 reason. Do not evaluate/consume
            // a new error expression or replace that reason with a staged exit.
            out.push_str("    if (cleanup) goto ku_task_terminated;\n");
        }
        out.push_str(&format!("    frame->result=({}){{false,{}, {error}}};\n    frame->header.result_initialized=1;\n    frame->header.exit_class=KU_TASK_EXIT_RUNTIME_FAILURE;\n",c_type(&self.function.result)?,c_zero_initializer(inner)?));
        if self.exit_bridge {
            // Await has already copied the inherited absolute deadline. Keep
            // it unchanged here; arithmetic/Print do not create a new budget.
            self.emit_exit_staged(out);
        } else {
            self.emit_all_slot_drops(out, true)?;
            out.push_str("    frame->header.status=KU_TASK_FRAME_READY; frame->header.running=0; return KU_TASK_FRAME_READY;\n");
        }
        Ok(())
    }

    fn emit_exit_staged(&self, out: &mut COutput) {
        // The Result and current ownership bitmap no longer match any prior
        // Suspend/Await cleanup edge. Cancellation must use the bitmap fallback
        // after the adapter has durably handed off every remaining Task owner.
        out.push_str(
            "  frame->header.cleanup_state = UINT32_MAX;\n\
               frame->header.status = KU_TASK_FRAME_EXIT_STAGED;\n\
               frame->header.running = 0;\n\
               return KU_TASK_FRAME_EXIT_STAGED;\n",
        );
    }

    fn emit_slot_drop(&self, out: &mut COutput, slot: SlotId) -> KuResult<()> {
        if self.task_slot(slot) {
            return Ok(());
        }
        let bitmap = self.bitmap(slot);
        let bit = Self::bit(slot);
        out.push_str(&format!("  if ({bitmap} & {bit}) {{\n"));
        // Clear the ownership bit before destruction. Existing helpers clear
        // their header too; neither an error path nor later frame destruction
        // can become a second owner of the same payload.
        self.set_init(out, slot, false);
        if self.owns_slot(slot) {
            out.push_str(&format!(
                "  {}\n",
                drop_statement(self.slot_type(slot)?, &self.place(slot))?
            ));
        }
        out.push_str("  }\n");
        Ok(())
    }

    fn emit_all_slot_drops(&self, out: &mut COutput, include_stack: bool) -> KuResult<()> {
        for index in (0..self.function.slots.len()).rev() {
            out.check()?;
            if include_stack || self.persistent.contains(&index) {
                self.emit_slot_drop(out, SlotId(index))?;
            }
        }
        Ok(())
    }

    fn emit_resume(&self, out: &mut COutput) {
        let prefix = &self.prefix;
        let frame_type = &self.frame_type;
        out.push_str(&format!(
            "static uint32_t {prefix}_resume(void* storage, size_t bytes, uint32_t abi, const KuTaskFrameClockV1* clock) {{\n\
               uint32_t checked = {prefix}_check(storage, bytes, abi);\n\
               if (checked != KU_TASK_FRAME_OK) return checked;\n\
               {frame_type}* frame = ({frame_type}*)storage;\n\
               if (ku_task_frame_is_terminal(frame->header.status) || frame->header.status == KU_TASK_FRAME_EXIT_STAGED || frame->header.status == KU_TASK_FRAME_SCOPE_REQUEST) return frame->header.status;\n\
               if (!clock || !clock->now_ms || ({hosted} && !clock->host)) return KU_TASK_FRAME_INVALID_ARGUMENT;\n\
               frame->header.running = 1;\n\
               return {prefix}_drive(frame, clock, 0);\n\
             }}\n",
            hosted=if self.hosted { "1" } else { "0" }
        ));
    }

    fn emit_finish_exit_values(&self, out: &mut COutput) -> KuResult<()> {
        let prefix = &self.prefix;
        let frame_type = &self.frame_type;
        out.push_str(&format!(
            "static uint32_t {prefix}_finish_exit_values(void* storage, size_t bytes, uint32_t abi) {{\n\
               uint32_t checked = {prefix}_check(storage, bytes, abi);\n\
               if (checked != KU_TASK_FRAME_OK) return checked;\n"
        ));
        if !self.exit_bridge {
            out.push_str("  return KU_TASK_FRAME_INVALID_STATE;\n}\n");
            return out.check();
        }
        out.push_str(&format!(
            "  {frame_type}* frame = ({frame_type}*)storage;\n\
               if (frame->header.status != KU_TASK_FRAME_EXIT_STAGED || (frame->header.initialized & UINT64_C({task_mask}))) return KU_TASK_FRAME_INVALID_STATE;\n\
               frame->header.running = 1;\n",
            task_mask = self.task_mask
        ));
        // All owned Values are persistent in a bridge, including an empty
        // Task set. Typed drops cannot invoke Ku callbacks or fail halfway;
        // clear their bits once, retaining the independently staged Result.
        self.emit_all_slot_drops(out, false)?;
        out.push_str(
            "  frame->header.status = KU_TASK_FRAME_READY;\n\
               frame->header.running = 0;\n\
               return KU_TASK_FRAME_OK;\n}\n",
        );
        out.check()
    }

    fn emit_terminate(&self, out: &mut COutput) -> KuResult<()> {
        let prefix = &self.prefix;
        let frame_type = &self.frame_type;
        out.push_str(&format!(
            "static uint32_t {prefix}_terminate(void* storage, size_t bytes, uint32_t abi, uint32_t reason, uint64_t absolute_cleanup_deadline_ms, const KuTaskFrameClockV1* clock) {{\n\
               uint32_t checked = {prefix}_check(storage, bytes, abi);\n\
               if (checked != KU_TASK_FRAME_OK) return checked;\n\
               if (reason != KU_TASK_FRAME_CANCELLED && reason != KU_TASK_FRAME_TIMED_OUT) return KU_TASK_FRAME_INVALID_ARGUMENT;\n\
               {frame_type}* frame = ({frame_type}*)storage;\n\
               if (frame->header.initialized & UINT64_C({task_mask})) return KU_TASK_FRAME_INVALID_STATE;\n\
               if (ku_task_frame_is_terminal(frame->header.status)) return frame->header.status;\n\
               if (!clock || !clock->now_ms) return KU_TASK_FRAME_INVALID_ARGUMENT;\n\
               frame->header.status = reason;\n\
               frame->header.cleanup_deadline_ms = absolute_cleanup_deadline_ms;\n\
               frame->header.running = 1;\n\
               if (frame->header.cleanup_state != UINT32_MAX) {{\n\
                 frame->header.state = frame->header.cleanup_state;\n\
                 return {prefix}_drive(frame, clock, 1);\n\
               }}\n\
               /* Entry, mid-drive cancellation and staged exits use current bitmap facts. */\n\
               if (clock->now_ms(clock->context) >= absolute_cleanup_deadline_ms) frame->header.cleanup_timed_out = 1;\n"
        ,task_mask=self.task_mask));
        // Every owned value reaching this fallback is persistent. A staged
        // Result remains independently owned until the all-Task handoff check
        // above succeeds; cancellation then discards it exactly once.
        self.emit_all_slot_drops(out, false)?;
        if self.exit_bridge {
            out.push_str(&format!(
                "  if (frame->header.result_initialized) {{\n\
                     frame->header.result_initialized = 0;\n\
                     {}\n\
                   }}\n",
                drop_statement(&self.function.result, "frame->result")?
            ));
        }
        out.push_str(
            "  frame->header.running = 0;\n\
               return frame->header.status;\n}\n",
        );
        Ok(())
    }

    fn emit_take_result(&self, out: &mut COutput) -> KuResult<()> {
        let prefix = &self.prefix;
        let frame_type = &self.frame_type;
        let result_type = c_type(&self.function.result)?;
        out.push_str(&format!(
            "static uint32_t {prefix}_take_result(void* storage, size_t bytes, uint32_t abi, {result_type}* output) {{\n\
               uint32_t checked = {prefix}_check(storage, bytes, abi);\n\
               if (checked != KU_TASK_FRAME_OK) return checked;\n\
               {frame_type}* frame = ({frame_type}*)storage;\n\
               if (frame->header.status != KU_TASK_FRAME_READY || !frame->header.result_initialized) return KU_TASK_FRAME_INVALID_STATE;\n\
               if (!ku_task_frame_storage_valid(output, sizeof(*output), sizeof(*output), KU_TASK_FRAME_ALIGNOF({result_type})) || ku_task_frame_ranges_overlap(storage, bytes, output, sizeof(*output))) return KU_TASK_FRAME_INVALID_ARGUMENT;\n\
               if (!({})) return KU_TASK_FRAME_INVALID_ARGUMENT;\n\
               *output = {};\n\
               frame->header.result_initialized = 0;\n\
               return KU_TASK_FRAME_OK;\n\
             }}\n",
            empty_value_expr(&self.function.result, "(*output)")?,
            c_move_value(&self.function.result, "frame->result")?
        ));
        Ok(())
    }

    fn emit_destroy(&self, out: &mut COutput) -> KuResult<()> {
        let prefix = &self.prefix;
        let frame_type = &self.frame_type;
        out.push_str(&format!(
            "static uint32_t {prefix}_destroy(void* storage, size_t bytes, uint32_t abi) {{\n\
               uint32_t checked = {prefix}_check(storage, bytes, abi);\n\
               if (checked != KU_TASK_FRAME_OK) return checked;\n\
               {frame_type}* frame = ({frame_type}*)storage;\n\
               if (!ku_task_frame_is_terminal(frame->header.status)) return KU_TASK_FRAME_INVALID_STATE;\n"
        ));
        out.push_str(&format!(
            "  if (frame->header.initialized & UINT64_C({})) return KU_TASK_FRAME_INVALID_STATE;\n",
            self.task_mask
        ));
        self.emit_all_slot_drops(out, false)?;
        out.push_str(&format!(
            "  if (frame->header.result_initialized) {{\n\
                 frame->header.result_initialized = 0;\n\
                 {}\n\
               }}\n\
               frame->header.status = KU_TASK_FRAME_DESTROYED;\n\
               return KU_TASK_FRAME_OK;\n\
             }}\n",
            drop_statement(&self.function.result, "frame->result")?
        ));
        Ok(())
    }
}

fn require_slot_type(ty: &IrType) -> KuResult<()> {
    match ty {
        IrType::Int | IrType::Bool | IrType::Null | IrType::Str => Ok(()),
        IrType::Result(_) => require_result_type(ty),
        _ => Err(unsupported(
            "native task frame R1 does not support this slot type",
        )),
    }
}

fn require_result_type(ty: &IrType) -> KuResult<()> {
    if matches!(ty, IrType::Result(inner) if matches!(**inner, IrType::Int | IrType::Bool | IrType::Null | IrType::Str))
    {
        Ok(())
    } else {
        Err(unsupported(
            "native task frame R1 requires Result<int|bool|null|str>",
        ))
    }
}

pub(super) fn drop_statement(ty: &IrType, place: &str) -> KuResult<String> {
    if let IrType::Result(inner) = ty {
        Ok(format!(
            "ku_result_drop_{}(&{place});",
            c_type_suffix(inner)?
        ))
    } else {
        c_drop_value(ty, place)
    }
}

// Empty typed output is checked by fields, not indeterminate C struct padding.
// This neither dereferences payload pointers nor drops an existing output.
pub(super) fn empty_value_expr(ty: &IrType, place: &str) -> KuResult<String> {
    match ty {
        IrType::Int | IrType::Bool | IrType::Null => Ok(format!("({place}) == 0")),
        IrType::Str => Ok(format!(
            "({place}).ptr == NULL && ({place}).len == 0 && ({place}).capacity == 0 && ({place}).storage == 0"
        )),
        IrType::Result(inner) => Ok(format!(
            "!({place}).ok && ({}) && ({}) && ({}) && ({})",
            empty_value_expr(inner, &format!("({place}).value"))?,
            empty_value_expr(&IrType::Str, &format!("({place}).error.domain"))?,
            empty_value_expr(&IrType::Str, &format!("({place}).error.code"))?,
            empty_value_expr(&IrType::Str, &format!("({place}).error.message"))?
        )),
        _ => Err(unsupported("native task output has an unsupported type")),
    }
}

fn constant_expr(value: &TaskConstant, ty: &IrType) -> KuResult<String> {
    match value {
        TaskConstant::Int(i64::MIN) => Ok("INT64_MIN".to_string()),
        TaskConstant::Int(value) => Ok(format!("INT64_C({value})")),
        TaskConstant::Bool(value) => Ok(value.to_string()),
        TaskConstant::Null => Ok("0".to_string()),
        TaskConstant::Str(value) => Ok(c_static_utf8_string(value)),
        TaskConstant::Ok(value) => {
            let IrType::Result(inner) = ty else {
                return Err(unsupported(
                    "native task Ok constant requires a Result slot",
                ));
            };
            Ok(format!(
                "({}){{ true, {}, (KuError){{0}} }}",
                c_type(ty)?,
                constant_expr(value, inner)?
            ))
        }
        TaskConstant::Err {
            result,
            domain,
            code,
            message,
        } => {
            if result != ty {
                return Err(unsupported(
                    "native task Err constant has the wrong Result type",
                ));
            }
            let IrType::Result(inner) = ty else {
                return Err(unsupported(
                    "native task Err constant requires a Result slot",
                ));
            };
            Ok(format!(
                "({}){{ false, {}, ku_error_make({}, {}, {}) }}",
                c_type(ty)?,
                c_zero_initializer(inner)?,
                c_static_utf8_string(domain),
                c_static_utf8_string(code),
                c_static_utf8_string(message)
            ))
        }
    }
}

const FRAME_ABI: &str = r#"
/* Internal frame ABI v4: single owner/executor, externally serialized calls.
 * Storage must be zero-filled, suitably aligned, caller-owned and live until
 * destroy finishes; destroy drops payloads but never frees that storage. Do not
 * mutate/copy a live frame. Owned argument headers and their deep payloads must
 * have unique, disjoint ownership. A successful init consumes only Str/Result
 * parameters; failures consume nothing. Result output must be initialized empty and
 * disjoint. Pending -> terminate -> destroy is frame-layer cleanup, NOT a Ku
 * Task handle-drop implementation. Clock callbacks are trusted non-reentrant,
 * monotonic internal hooks. Hosted child owners must be transferred before
 * terminate/destroy; host pointers are borrowed only during a callback.
 * EXIT_STAGED is stopped but nonterminal: resume cannot replay it, take/destroy
 * reject it. After all Task owners are handed off, finish_exit_values drops
 * Values and makes the retained Result READY. Genuine cancellation may instead
 * terminate, dropping both current Values and the staged Result once.
 * SCOPE_REQUEST is another stopped, nonterminal boundary: repeated resume
 * returns the same request without replaying source operations. Its static
 * descriptor comes from the verified current state; no runtime scope stack is
 * allocated. Continue requires the selected Task bitmap empty and a trusted
 * adapter end/empty witness, then leaves PENDING for a later dispatch phase
 * check. Cancellation retains the request cleanup CFG. Only a real scope-timeout
 * witness stages RuntimeFailure with the supplied original D and invalidates
 * that CFG. Descriptor outputs are exclusive NULL-initialized pointer slots,
 * disjoint from frame and every Owned deep allocation; interval guards do not
 * authenticate arbitrary raw C storage. No helper changes R2 cancellation. */
#define KU_TASK_FRAME_ABI_VERSION 4u
#if defined(_MSC_VER)
#define KU_TASK_FRAME_ALIGNOF(T) __alignof(T)
#else
#define KU_TASK_FRAME_ALIGNOF(T) _Alignof(T)
#endif
enum {
  KU_TASK_FRAME_OK = 0u,
  KU_TASK_FRAME_PENDING = 1u,
  KU_TASK_FRAME_READY = 2u,
  KU_TASK_FRAME_CANCELLED = 3u,
  KU_TASK_FRAME_TIMED_OUT = 4u,
  KU_TASK_FRAME_ABI_MISMATCH = 5u,
  KU_TASK_FRAME_INVALID_STORAGE = 6u,
  KU_TASK_FRAME_INVALID_STATE = 7u,
  KU_TASK_FRAME_LIMIT = 8u,
  KU_TASK_FRAME_INVALID_ARGUMENT = 9u,
  KU_TASK_FRAME_DESTROYED = 10u,
  KU_TASK_FRAME_EXIT_STAGED = 11u,
  KU_TASK_FRAME_SCOPE_REQUEST = 12u
};
typedef struct KuTaskFrameScopeDescriptorV1 {
  uint64_t scope_id, task_mask;
  uint32_t request_state, ready_state, cleanup_state;
} KuTaskFrameScopeDescriptorV1;
typedef struct KuTaskFrameClockV1 {
  uint64_t (*now_ms)(void* context);
  void* context;
  void* host;
} KuTaskFrameClockV1;
typedef struct KuTaskFrameHeaderV1 {
  uint32_t abi_version;
  uint32_t status;
  size_t storage_size;
  uint64_t function_id;
  uint64_t initialized;
  uint64_t cleanup_deadline_ms;
  uint32_t state;
  uint32_t cleanup_state;
  uint32_t running;
  uint32_t result_initialized;
  uint32_t cleanup_timed_out;
  uint32_t exit_class;
  uint32_t has_exit_deadline;
  uint64_t exit_deadline;
} KuTaskFrameHeaderV1;
static int ku_task_frame_storage_valid(const void* storage, size_t bytes,
                                     size_t required, size_t alignment) {
  uintptr_t address = (uintptr_t)storage;
  return storage && alignment && address % alignment == 0 && bytes >= required
      && bytes <= UINTPTR_MAX - address;
}
static int ku_task_frame_ranges_overlap(const void* a, size_t a_size,
                                       const void* b, size_t b_size) {
  uintptr_t a_start = (uintptr_t)a, b_start = (uintptr_t)b;
  /* Callers validate each interval before reaching this helper. */
  return a_start < b_start + b_size && b_start < a_start + a_size;
}
static int ku_task_frame_zero_bytes(const void* storage, size_t bytes) {
  const uint8_t* data = (const uint8_t*)storage;
  for (size_t index = 0; index < bytes; index++) if (data[index]) return 0;
  return 1;
}
static int ku_task_frame_is_terminal(uint32_t status) {
  return status == KU_TASK_FRAME_READY || status == KU_TASK_FRAME_CANCELLED
      || status == KU_TASK_FRAME_TIMED_OUT;
}
"#;
