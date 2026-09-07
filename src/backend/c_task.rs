//! Internal verified task-frame emitter, not a public Ku Task ABI.
//!
//! The caller supplies zeroed, aligned storage and serializes every operation.
//! A Pending frame must traverse its verified cleanup CFG before destruction.
//! Hosted frames use generated child Start/Await and adapter-owned scope drain.
//! User cleanup remains finite and cannot await; source admission stays separate.

use std::collections::HashSet;

use crate::error::KuResult;
use crate::ir::task::{
    SlotId, TaskConstant, TaskFramePlan, TaskFunction, TaskFunctionFrame, TaskOp, TaskProgram,
    TaskSlotType, TaskTerminator,
};
use crate::ir::IrType;

use super::output::COutput;
use super::{
    c_drop_value, c_move_value, c_static_utf8_string, c_type, c_type_suffix, c_zero_initializer,
    unsupported,
};

const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_FRAME_SLOTS: usize = 64;

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
}

impl<'a> FrameEmitter<'a> {
    fn new(function: &'a TaskFunction, frame: &TaskFunctionFrame) -> KuResult<Self> {
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
        for (index, slot) in function.slots.iter().enumerate() {
            match &slot.ty {
                TaskSlotType::Value { ty, borrowed } => {
                    require_slot_type(ty)?;
                    if *borrowed && persistent.contains(&index) {
                        return Err(unsupported(
                            "borrowed values cannot enter a native task frame",
                        ));
                    }
                }
                TaskSlotType::Task { result } => {
                    require_result_type(result)?;
                }
            }
        }
        require_result_type(&function.result)?;
        Ok(Self {
            function,
            persistent,
            prefix: format!("ku_task_frame_{}", function.id.0),
            frame_type: format!("KuTaskFrame_{}", function.id.0),
            task_mask: frame.scope_task_mask,
            hosted: frame.hosted,
        })
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
        self.emit_check(out);
        self.emit_init(out)?;
        self.emit_drive(out)?;
        self.emit_resume(out);
        self.emit_terminate(out)?;
        self.emit_take_result(out)?;
        self.emit_destroy(out)?;
        out.check()
    }

    fn emit_check(&self, out: &mut COutput) {
        let prefix = &self.prefix;
        let frame_type = &self.frame_type;
        out.push_str(&format!(
            "static uint32_t {prefix}_check(void* storage, size_t bytes, uint32_t abi) {{\n\
               if (abi != KU_TASK_FRAME_ABI_VERSION) return KU_TASK_FRAME_ABI_MISMATCH;\n\
               if (sizeof({frame_type}) > {MAX_FRAME_BYTES}u) return KU_TASK_FRAME_LIMIT;\n\
               if (!ku_task_frame_storage_valid(storage, bytes, sizeof({frame_type}), {prefix}_align())) return KU_TASK_FRAME_INVALID_STORAGE;\n\
               {frame_type}* frame = ({frame_type}*)storage;\n\
               if (frame->header.abi_version != KU_TASK_FRAME_ABI_VERSION) return KU_TASK_FRAME_ABI_MISMATCH;\n\
               if (frame->header.storage_size != sizeof({frame_type}) || frame->header.function_id != UINT64_C({id})) return KU_TASK_FRAME_INVALID_STORAGE;\n\
               if (frame->header.running || frame->header.status > KU_TASK_FRAME_TIMED_OUT) return KU_TASK_FRAME_INVALID_STATE;\n\
               if (frame->header.state >= {states}u || (frame->header.cleanup_state != UINT32_MAX && frame->header.cleanup_state >= {states}u)) return KU_TASK_FRAME_INVALID_STATE;\n\
               return KU_TASK_FRAME_OK;\n\
             }}\n",
            id = self.function.id.0,
            states = self.function.states.len()
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
                out.push_str(&format!("  {{ uint32_t started=ku_task_{}_start_value(((KuTaskAdapterHostV1*)clock->host)->ticket->driver",function.0));
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

    fn emit_runtime_error(&self, out: &mut COutput, error: &str) -> KuResult<()> {
        let IrType::Result(inner) = &self.function.result else {
            return Err(unsupported("runtime exit requires Result"));
        };
        out.push_str(&format!("    frame->result=({}){{false,{}, {error}}};\n    frame->header.result_initialized=1;\n    frame->header.exit_class=KU_TASK_EXIT_RUNTIME_FAILURE;\n",c_type(&self.function.result)?,c_zero_initializer(inner)?));
        self.emit_all_slot_drops(out, true)?;
        out.push_str("    frame->header.status=KU_TASK_FRAME_READY; frame->header.running=0; return KU_TASK_FRAME_READY;\n");
        Ok(())
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
               if (ku_task_frame_is_terminal(frame->header.status)) return frame->header.status;\n\
               if (!clock || !clock->now_ms || ({hosted} && !clock->host)) return KU_TASK_FRAME_INVALID_ARGUMENT;\n\
               frame->header.running = 1;\n\
               return {prefix}_drive(frame, clock, 0);\n\
             }}\n",
            hosted=if self.hosted { "1" } else { "0" }
        ));
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
               /* Before the first suspend, only owned entry parameters exist. */\n\
               if (clock->now_ms(clock->context) >= absolute_cleanup_deadline_ms) frame->header.cleanup_timed_out = 1;\n"
        ,task_mask=self.task_mask));
        // Entry parameters are always persistent, so no stack slot exists here.
        self.emit_all_slot_drops(out, false)?;
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
/* Internal frame ABI v2: single owner/executor, externally serialized calls.
 * Storage must be zero-filled, suitably aligned, caller-owned and live until
 * destroy finishes; destroy drops payloads but never frees that storage. Do not
 * mutate/copy a live frame. Owned argument headers and their deep payloads must
 * have unique, disjoint ownership. A successful init consumes only Str/Result
 * parameters; failures consume nothing. Result output must be initialized empty and
 * disjoint. Pending -> terminate -> destroy is frame-layer cleanup, NOT a Ku
 * Task handle-drop implementation. Clock callbacks are trusted non-reentrant,
 * monotonic internal hooks. Hosted child owners must be transferred before
 * terminate/destroy; host pointers are borrowed only during a callback. */
#define KU_TASK_FRAME_ABI_VERSION 2u
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
  KU_TASK_FRAME_DESTROYED = 10u
};
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
