use crate::{ast::input::Template, stdlib::errors, value::Value};

#[derive(Clone, Copy)]
pub(super) enum ReadError {
    Eof,
    Io,
    Integer,
    Allocation,
    Unavailable,
}

impl ReadError {
    pub(super) fn value(self) -> Value {
        let (code, message) = match self {
            Self::Eof => ("eof", "input ended"),
            Self::Io => ("io_error", "failed to read input or write its prompt"),
            Self::Integer => (
                "invalid_integer",
                "expected a signed 64-bit decimal integer",
            ),
            Self::Allocation => ("allocation_failed", "could not allocate input"),
            Self::Unavailable => (
                "unavailable",
                "input is available only in synchronous CLI code",
            ),
        };
        errors::error_object("input", code, message)
    }
}

#[cfg(any(unix, windows))]
pub(super) use cli::read;

#[cfg(not(any(unix, windows)))]
pub(super) fn read(_prompt: &str, _template: Option<&Template>) -> Result<Vec<Value>, ReadError> {
    Err(ReadError::Unavailable)
}

#[cfg(any(unix, windows))]
mod cli {
    use super::*;
    use crate::ast::input::Field;
    use std::ffi::c_char;
    use std::io::Write;
    use std::ptr::null;
    use std::sync::Mutex;

    // The C runtime uses process stdin/stdout and a temporary terminal mode.
    static INPUT: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    #[repr(C)]
    struct FieldValue {
        bytes: *mut u8,
        len: usize,
        integer: i64,
    }

    unsafe extern "C" {
        fn ku_input_read(
            prompt: *const c_char,
            prompt_len: usize,
            segments: *const *const c_char,
            segment_lens: *const usize,
            field_kinds: *const u8,
            count: usize,
            fields: *mut FieldValue,
        ) -> i32;
        fn ku_input_free(fields: *mut FieldValue, count: usize);
    }

    struct Fields(Vec<FieldValue>);

    impl Drop for Fields {
        fn drop(&mut self) {
            // The C helper owns the byte allocations stored in these fields.
            unsafe { ku_input_free(self.0.as_mut_ptr(), self.0.len()) };
        }
    }

    pub(in crate::runtime) fn read(
        prompt: &str,
        template: Option<&Template>,
    ) -> Result<Vec<Value>, ReadError> {
        let (mut segments, mut segment_lens, mut kinds) = (Vec::new(), Vec::new(), Vec::new());
        if let Some(template) = template {
            reserve(&mut segments, template.segments.len())?;
            reserve(&mut segment_lens, template.segments.len())?;
            reserve(&mut kinds, template.fields.len())?;
            for segment in &template.segments {
                segments.push(segment.as_ptr().cast::<c_char>());
                segment_lens.push(segment.len());
            }
            kinds.extend(
                template
                    .fields
                    .iter()
                    .map(|field| u8::from(*field == Field::Int)),
            );
        }
        let count = template.map_or(1, |template| template.fields.len());
        let mut fields = Fields(Vec::new());
        let mut values = Vec::new();
        reserve(&mut fields.0, count)?;
        reserve(&mut values, count)?;
        fields.0.resize_with(count, FieldValue::default);
        let _guard = INPUT.lock().map_err(|_| ReadError::Io)?;
        std::io::stdout().flush().map_err(|_| ReadError::Io)?;
        // Slices and pointers remain valid throughout the synchronous read.
        let status = unsafe {
            ku_input_read(
                prompt.as_ptr().cast(),
                prompt.len(),
                if template.is_some() {
                    segments.as_ptr()
                } else {
                    null()
                },
                segment_lens.as_ptr(),
                if kinds.is_empty() {
                    null()
                } else {
                    kinds.as_ptr()
                },
                count,
                fields.0.as_mut_ptr(),
            )
        };
        match status {
            0 => {}
            1 => return Err(ReadError::Eof),
            3 => return Err(ReadError::Integer),
            4 => return Err(ReadError::Allocation),
            _ => return Err(ReadError::Io),
        }
        for (i, field) in fields.0.iter().enumerate() {
            if kinds.get(i) == Some(&1) {
                values.push(Value::Int(field.integer));
                continue;
            }
            let mut bytes = Vec::new();
            if field.len != 0 {
                // On success, a nonempty text field has a live C allocation.
                let source = unsafe { std::slice::from_raw_parts(field.bytes, field.len) };
                reserve(&mut bytes, source.len())?;
                bytes.extend_from_slice(source);
            }
            values.push(Value::String(
                String::from_utf8(bytes).map_err(|_| ReadError::Io)?,
            ));
        }
        Ok(values)
    }

    fn reserve<T>(items: &mut Vec<T>, count: usize) -> Result<(), ReadError> {
        items
            .try_reserve_exact(count)
            .map_err(|_| ReadError::Allocation)
    }
}
