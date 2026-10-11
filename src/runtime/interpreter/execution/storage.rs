//! Bounded storage for a flat, private interpreter continuation stack.
//!
//! This accounts for the `Vec` backing storage, not allocator metadata, process
//! RSS, or allocations owned by `T`. A frame payload needs its own budget when
//! it owns a separately allocated buffer. No recursive parent links are added.

use std::mem::size_of;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameLimits {
    pub(super) max_frames: usize,
    pub(super) max_storage_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReserveError {
    FrameLimit,
    StorageLimit,
    ArithmeticOverflow,
    AllocationFailed,
}

pub(super) struct FrameStack<T> {
    frames: Vec<T>,
    limits: FrameLimits,
    peak_storage_bytes: usize,
    #[cfg(test)]
    fail_next_reservation: bool,
}

impl<T> FrameStack<T> {
    pub(super) fn new(limits: FrameLimits) -> Self {
        Self {
            frames: Vec::new(),
            limits,
            peak_storage_bytes: 0,
            #[cfg(test)]
            fail_next_reservation: false,
        }
    }

    /// Reserve space before moving an owned value into this stack.
    ///
    /// Growing uses a separate allocation: both buffers coexist until the old
    /// values have moved. Admission therefore charges old + new capacity, not
    /// only the final capacity. Failed admission leaves the old stack intact.
    pub(super) fn reserve(&mut self, additional: usize) -> Result<(), ReserveError> {
        self.reserve_with_budget(
            additional,
            self.limits.max_frames,
            self.limits.max_storage_bytes,
        )
    }

    /// Reserve within a shared operand budget after other live operands and
    /// retained argument buffers have been charged. `available_*` are the total
    /// allowance for this stack, including its current storage, not extra space.
    /// A popped stack still owns its capacity even when no growth is needed.
    pub(super) fn reserve_with_budget(
        &mut self,
        additional: usize,
        available_frames: usize,
        available_bytes: usize,
    ) -> Result<(), ReserveError> {
        let frame_limit = self.limits.max_frames.min(available_frames);
        let byte_limit = self.limits.max_storage_bytes.min(available_bytes);
        let required = self
            .len()
            .checked_add(additional)
            .ok_or(ReserveError::ArithmeticOverflow)?;
        if required > frame_limit {
            return Err(ReserveError::FrameLimit);
        }
        let old_bytes = self.charged_storage_bytes();
        if old_bytes > byte_limit {
            return Err(ReserveError::StorageLimit);
        }
        if required <= self.capacity() {
            return Ok(());
        }

        let minimum_peak = storage_peak::<T>(old_bytes, required)?;
        if minimum_peak > byte_limit {
            return Err(ReserveError::StorageLimit);
        }

        // Prefer geometric growth, but do not reject an otherwise admissible
        // operation merely because spare capacity would exceed its budget.
        let preferred = self
            .capacity()
            .checked_mul(2)
            .unwrap_or(frame_limit)
            .min(frame_limit)
            .max(required);
        let target = match storage_peak::<T>(old_bytes, preferred) {
            Ok(peak) if peak <= byte_limit => preferred,
            _ => required,
        };

        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_reservation) {
            return Err(ReserveError::AllocationFailed);
        }

        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(target)
            .map_err(|_| ReserveError::AllocationFailed)?;

        // `try_reserve_exact` is permitted to return extra capacity. Never
        // publish that allocation as an admitted buffer without checking its
        // actual capacity. A rejected empty candidate is freed immediately.
        // The allocator may already have given this candidate a larger visible
        // capacity than requested: even that transient Vec backing allocation,
        // not just hidden allocator metadata, is NOT a pre-allocation hard cap.
        // The measured peak includes it; only admitted storage stays in budget.
        let actual_peak = storage_peak::<T>(old_bytes, replacement.capacity())?;
        self.peak_storage_bytes = self.peak_storage_bytes.max(actual_peak);
        if actual_peak > byte_limit {
            return Err(ReserveError::StorageLimit);
        }
        if replacement.capacity() > frame_limit {
            return Err(ReserveError::FrameLimit);
        }

        // Capacity is already sufficient, so append cannot grow the candidate.
        // It transfers ownership without cloning or dropping any frame.
        replacement.append(&mut self.frames);
        self.frames = replacement;
        Ok(())
    }

    /// Never allocates. On misuse, ownership is returned to the caller so it
    /// can run the same explicit rollback/cleanup path as a failed reservation.
    pub(super) fn push_reserved(&mut self, value: T) -> Result<(), T> {
        if self.len() >= self.limits.max_frames || self.len() >= self.capacity() {
            return Err(value);
        }
        self.frames.push(value);
        Ok(())
    }

    pub(super) fn pop(&mut self) -> Option<T> {
        self.frames.pop()
    }

    pub(super) fn get(&self, index: usize) -> Option<&T> {
        self.frames.get(index)
    }

    pub(super) fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.frames.get_mut(index)
    }

    pub(super) fn len(&self) -> usize {
        self.frames.len()
    }

    /// Usable frame slots. Zero-sized frames have no backing allocation, but
    /// still have the same finite count limit as non-zero-sized frames.
    pub(super) fn capacity(&self) -> usize {
        if size_of::<T>() == 0 {
            self.limits.max_frames
        } else {
            self.frames.capacity()
        }
    }

    pub(super) fn charged_storage_bytes(&self) -> usize {
        // Every published non-ZST capacity passed checked arithmetic above.
        // Popping values intentionally does not release this capacity charge.
        self.capacity() * size_of::<T>()
    }

    pub(super) fn peak_storage_bytes(&self) -> usize {
        self.peak_storage_bytes
    }

    #[cfg(test)]
    fn fail_next_reservation(&mut self) {
        self.fail_next_reservation = true;
    }
}

impl<T> Drop for FrameStack<T> {
    fn drop(&mut self) {
        // Vec's ordinary element drop order is not the interpreter's unwind
        // order. Consume this flat stack explicitly, newest frame first.
        while let Some(frame) = self.frames.pop() {
            drop(frame);
        }
    }
}

fn storage_peak<T>(old_bytes: usize, new_capacity: usize) -> Result<usize, ReserveError> {
    let new_bytes = new_capacity
        .checked_mul(size_of::<T>())
        .ok_or(ReserveError::ArithmeticOverflow)?;
    old_bytes
        .checked_add(new_bytes)
        .ok_or(ReserveError::ArithmeticOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn limits(max_frames: usize, max_storage_bytes: usize) -> FrameLimits {
        FrameLimits {
            max_frames,
            max_storage_bytes,
        }
    }

    #[test]
    fn count_admission_and_unreserved_push_preserve_values() {
        let mut stack = FrameStack::new(limits(1, 32));
        assert_eq!(stack.push_reserved(7_u64), Err(7));
        stack.reserve(1).unwrap();
        stack.push_reserved(11).unwrap();
        assert_eq!(stack.reserve(1), Err(ReserveError::FrameLimit));
        assert_eq!(stack.push_reserved(13), Err(13));
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.pop(), Some(11));
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn shared_count_and_retained_bytes_reject_without_losing_operands() {
        let mut stack = FrameStack::new(limits(4, 64));
        stack.reserve(2).unwrap();
        stack.push_reserved(11_u64).unwrap();
        stack.push_reserved(13).unwrap();
        assert_eq!(
            stack.reserve_with_budget(0, 1, 64),
            Err(ReserveError::FrameLimit)
        );
        assert_eq!(stack.pop(), Some(13));
        assert_eq!(stack.pop(), Some(11));
        let retained = stack.charged_storage_bytes();
        assert!(retained >= 16);
        assert_eq!(
            stack.reserve_with_budget(1, 4, retained - 1),
            Err(ReserveError::StorageLimit)
        );
        assert_eq!(stack.len(), 0);
        assert_eq!(stack.charged_storage_bytes(), retained);
        stack.reserve_with_budget(1, 1, retained).unwrap();
        stack.push_reserved(17).unwrap();
        assert_eq!(stack.pop(), Some(17));
    }

    #[test]
    fn shared_growth_charges_old_plus_new_and_avoids_unaffordable_spare() {
        let mut stack = FrameStack::new(limits(8, 128));
        stack.reserve(2).unwrap();
        stack.push_reserved(11_u64).unwrap();
        stack.push_reserved(13).unwrap();
        let old = stack.charged_storage_bytes();
        let minimum = old + 3 * size_of::<u64>();
        assert_eq!(
            stack.reserve_with_budget(1, 8, minimum - 1),
            Err(ReserveError::StorageLimit)
        );
        assert_eq!(stack.get(0), Some(&11));
        assert_eq!(stack.get(1), Some(&13));
        assert_eq!(stack.charged_storage_bytes(), old);
        stack.reserve_with_budget(1, 8, minimum).unwrap();
        assert_eq!(stack.capacity(), 3);
        assert_eq!(stack.peak_storage_bytes(), minimum);
        stack.push_reserved(17).unwrap();
        assert_eq!(stack.pop(), Some(17));
        assert_eq!(stack.pop(), Some(13));
        assert_eq!(stack.pop(), Some(11));
    }

    #[test]
    fn shared_count_applies_to_live_frames_when_capacity_is_already_retained() {
        let mut stack = FrameStack::<()>::new(limits(4, 0));
        stack.reserve_with_budget(1, 1, 0).unwrap();
        stack.push_reserved(()).unwrap();
        assert_eq!(
            stack.reserve_with_budget(1, 1, 0),
            Err(ReserveError::FrameLimit)
        );
        assert_eq!(stack.pop(), Some(()));
        stack.reserve_with_budget(1, 1, 0).unwrap();
        stack.push_reserved(()).unwrap();
        assert_eq!(stack.pop(), Some(()));
        assert_eq!(stack.peak_storage_bytes(), 0);
    }

    #[test]
    fn indexed_access_preserves_source_order_without_removing_slots() {
        let mut stack = FrameStack::new(limits(3, 3 * size_of::<u64>()));
        stack.reserve(3).unwrap();
        for value in [11, 13, 17] {
            stack.push_reserved(value).unwrap();
        }
        assert_eq!(stack.get(0), Some(&11));
        assert_eq!(stack.get(2), Some(&17));
        assert_eq!(stack.get(3), None);
        assert_eq!(stack.get_mut(3), None);
        assert_eq!(std::mem::replace(stack.get_mut(1).unwrap(), 0), 13);
        assert_eq!(stack.len(), 3);
        assert_eq!(stack.pop(), Some(17));
        assert_eq!(stack.pop(), Some(0));
        assert_eq!(stack.pop(), Some(11));
    }

    #[test]
    fn zero_sized_frames_still_obey_count_limit() {
        let mut stack = FrameStack::<()>::new(limits(2, 0));
        stack.reserve(2).unwrap();
        stack.push_reserved(()).unwrap();
        stack.push_reserved(()).unwrap();
        assert_eq!(stack.reserve(1), Err(ReserveError::FrameLimit));
        assert_eq!(stack.push_reserved(()), Err(()));
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.capacity(), 2);
        assert_eq!(stack.charged_storage_bytes(), 0);
        assert_eq!(stack.peak_storage_bytes(), 0);
    }

    #[test]
    fn zero_count_and_zero_byte_budgets_reject_nonempty_storage() {
        let mut no_frames = FrameStack::<u8>::new(limits(0, 1024));
        assert_eq!(no_frames.reserve(1), Err(ReserveError::FrameLimit));
        let mut no_bytes = FrameStack::<u8>::new(limits(1, 0));
        assert_eq!(no_bytes.reserve(1), Err(ReserveError::StorageLimit));
        assert_eq!(no_bytes.capacity(), 0);
        assert_eq!(no_bytes.charged_storage_bytes(), 0);
        assert_eq!(no_bytes.peak_storage_bytes(), 0);
    }

    #[test]
    fn growth_rejects_old_plus_new_peak_even_if_final_buffer_would_fit() {
        let mut stack = FrameStack::<u8>::new(limits(4, 2));
        stack.reserve(1).unwrap();
        stack.push_reserved(41).unwrap();
        assert_eq!(stack.reserve(1), Err(ReserveError::StorageLimit));
        assert_eq!(stack.capacity(), 1);
        assert_eq!(stack.charged_storage_bytes(), 1);
        assert_eq!(stack.peak_storage_bytes(), 1);
        assert_eq!(stack.pop(), Some(41));
    }

    #[test]
    fn successful_growth_records_coexisting_capacity_and_pop_retains_charge() {
        let mut stack = FrameStack::<u8>::new(limits(4, 6));
        stack.reserve(1).unwrap();
        stack.push_reserved(1).unwrap();
        stack.reserve(1).unwrap();
        stack.push_reserved(2).unwrap();
        assert_eq!(stack.charged_storage_bytes(), 2);
        assert_eq!(stack.peak_storage_bytes(), 3);
        stack.reserve(1).unwrap();
        stack.push_reserved(3).unwrap();
        assert_eq!(stack.capacity(), 4);
        assert_eq!(stack.charged_storage_bytes(), 4);
        assert_eq!(stack.peak_storage_bytes(), 6);
        assert_eq!(stack.pop(), Some(3));
        assert_eq!(stack.pop(), Some(2));
        assert_eq!(stack.pop(), Some(1));
        assert_eq!(stack.len(), 0);
        assert_eq!(stack.charged_storage_bytes(), 4);
        assert_eq!(stack.peak_storage_bytes(), 6);
    }

    #[test]
    fn growth_uses_required_capacity_if_geometric_spare_does_not_fit() {
        let mut stack = FrameStack::<u8>::new(limits(8, 5));
        stack.reserve(2).unwrap();
        stack.push_reserved(1).unwrap();
        stack.push_reserved(2).unwrap();
        stack.reserve(1).unwrap();
        assert_eq!(stack.capacity(), 3);
        assert_eq!(stack.charged_storage_bytes(), 3);
        assert_eq!(stack.peak_storage_bytes(), 5);
    }

    #[test]
    fn checked_arithmetic_rejects_count_bytes_and_peak_overflow() {
        let mut stack = FrameStack::<u8>::new(limits(usize::MAX, usize::MAX));
        stack.reserve(1).unwrap();
        stack.push_reserved(5).unwrap();
        assert_eq!(
            stack.reserve(usize::MAX),
            Err(ReserveError::ArithmeticOverflow)
        );
        assert_eq!(stack.pop(), Some(5));

        let mut wide = FrameStack::<[u8; 2]>::new(limits(usize::MAX, usize::MAX));
        assert_eq!(
            wide.reserve(usize::MAX),
            Err(ReserveError::ArithmeticOverflow)
        );
        assert_eq!(wide.capacity(), 0);
        assert_eq!(
            storage_peak::<u8>(usize::MAX, 1),
            Err(ReserveError::ArithmeticOverflow)
        );
    }

    #[derive(Debug)]
    struct DropProbe {
        id: usize,
        trace: Arc<Mutex<Vec<usize>>>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.trace.lock().unwrap().push(self.id);
        }
    }

    #[test]
    fn injected_failure_keeps_old_stack_and_unpushed_owned_input() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let mut stack = FrameStack::new(limits(4, size_of::<DropProbe>() * 6));
        stack.reserve(1).unwrap();
        stack
            .push_reserved(DropProbe {
                id: 1,
                trace: Arc::clone(&trace),
            })
            .unwrap();
        let input = DropProbe {
            id: 2,
            trace: Arc::clone(&trace),
        };
        let charged = stack.charged_storage_bytes();
        let peak = stack.peak_storage_bytes();

        stack.fail_next_reservation();
        assert_eq!(stack.reserve(1), Err(ReserveError::AllocationFailed));
        let input = stack
            .push_reserved(input)
            .expect_err("failed reservation cannot admit a frame");
        assert_eq!(input.id, 2);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.charged_storage_bytes(), charged);
        assert_eq!(stack.peak_storage_bytes(), peak);
        assert!(trace.lock().unwrap().is_empty());

        // The injected failure is one-shot; a successful retry moves the same
        // input, without cloning it or releasing either owner prematurely.
        stack.reserve(1).unwrap();
        stack.push_reserved(input).unwrap();
        assert!(trace.lock().unwrap().is_empty());
        drop(stack);
        assert_eq!(*trace.lock().unwrap(), vec![2, 1]);
    }

    #[test]
    fn flat_teardown_is_lifo_and_drops_each_frame_once() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let mut stack = FrameStack::new(limits(4, size_of::<DropProbe>() * 4));
        stack.reserve(4).unwrap();
        for id in 0..4 {
            stack
                .push_reserved(DropProbe {
                    id,
                    trace: Arc::clone(&trace),
                })
                .unwrap();
        }
        drop(stack.pop());
        assert_eq!(*trace.lock().unwrap(), vec![3]);
        drop(stack);
        assert_eq!(*trace.lock().unwrap(), vec![3, 2, 1, 0]);
    }

    #[test]
    fn million_push_pop_cycles_reuse_one_admitted_slot() {
        let mut stack = FrameStack::new(limits(1, size_of::<usize>()));
        stack.reserve(1).unwrap();
        let capacity = stack.capacity();
        let charged = stack.charged_storage_bytes();
        let peak = stack.peak_storage_bytes();
        for value in 0..1_000_000_usize {
            stack.reserve(1).unwrap();
            stack.push_reserved(value).unwrap();
            assert_eq!(stack.pop(), Some(value));
        }
        assert_eq!(stack.len(), 0);
        assert_eq!(stack.capacity(), capacity);
        assert_eq!(stack.charged_storage_bytes(), charged);
        assert_eq!(stack.peak_storage_bytes(), peak);
    }
}
