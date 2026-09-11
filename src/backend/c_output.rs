//! Bounded storage for the complete generated C file.
//!
//! The first error is sticky so fixed runtime emitters need not turn every
//! static fragment into a fallible call. Emitter/loop checkpoints stop further
//! work, and finish never returns a truncated success. This limits output bytes,
//! not total compiler RSS: expression/formatting scratch strings are separate.

use crate::error::{KuError, KuResult};

#[derive(Clone, Copy)]
enum OutputFailure {
    Limit,
    Allocation,
}

pub(super) struct COutput {
    text: String,
    limit: usize,
    failure: Option<OutputFailure>,
}

fn checked_length(current: usize, additional: usize, limit: usize) -> Option<usize> {
    current
        .checked_add(additional)
        .filter(|length| *length <= limit)
}

impl COutput {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
            failure: None,
        }
    }

    pub(super) fn push_str(&mut self, text: &str) {
        if self.failed() {
            return;
        }
        let Some(length) = checked_length(self.text.len(), text.len(), self.limit) else {
            self.failure = Some(OutputFailure::Limit);
            return;
        };
        if length > self.text.capacity() {
            // Avoid String's geometric growth requesting capacity beyond the
            // byte limit. Allocator bookkeeping/rounding is not an RSS promise.
            let capacity = self
                .text
                .capacity()
                .max(1024)
                .saturating_mul(2)
                .min(self.limit)
                .max(length);
            if self
                .text
                .try_reserve_exact(capacity - self.text.len())
                .is_err()
            {
                self.failure = Some(OutputFailure::Allocation);
                return;
            }
        }
        self.text.push_str(text);
    }

    pub(super) fn push(&mut self, value: char) {
        let mut bytes = [0; 4];
        self.push_str(value.encode_utf8(&mut bytes));
    }

    pub(super) fn failed(&self) -> bool {
        self.failure.is_some()
    }

    pub(super) fn check(&self) -> KuResult<()> {
        match self.failure {
            None => Ok(()),
            Some(OutputFailure::Limit) => Err(KuError::message(format!(
                "native C output limit exceeded: maximum {} bytes",
                self.limit
            ))),
            Some(OutputFailure::Allocation) => {
                Err(KuError::message("native C output allocation failed"))
            }
        }
    }

    pub(super) fn finish(self) -> KuResult<String> {
        self.check()?;
        Ok(self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_c_output_accepts_exact_byte_limit() {
        let mut output = COutput::new(5);
        output.push_str("ab");
        output.push('界');
        assert_eq!(output.finish().unwrap(), "ab界");
    }

    #[test]
    fn native_c_output_rejects_before_reserving_or_appending() {
        let mut output = COutput::new(4);
        output.push_str("abc");
        let capacity = output.text.capacity();
        output.push_str("de");
        assert_eq!(output.text, "abc");
        assert_eq!(output.text.capacity(), capacity);
        let first = output.check().unwrap_err();
        output.push('x');
        output.push_str("");
        assert_eq!(output.text, "abc");
        assert_eq!(output.text.capacity(), capacity);
        assert_eq!(output.finish().unwrap_err(), first);
    }

    #[test]
    fn native_c_output_zero_budget_and_length_overflow_are_bounded() {
        assert_eq!(COutput::new(0).finish().unwrap(), "");
        let mut output = COutput::new(0);
        output.push('x');
        assert_eq!(output.text.capacity(), 0);
        assert!(output
            .finish()
            .unwrap_err()
            .message
            .contains("limit exceeded"));
        assert_eq!(checked_length(usize::MAX, 1, usize::MAX), None);
        assert_eq!(
            checked_length(usize::MAX - 1, 1, usize::MAX),
            Some(usize::MAX)
        );
    }

    #[test]
    fn native_c_output_allocation_failure_cannot_return_success() {
        let output = COutput {
            text: "prefix".into(),
            limit: 64,
            failure: Some(OutputFailure::Allocation),
        };
        assert!(output
            .finish()
            .unwrap_err()
            .message
            .contains("allocation failed"));
    }
}
