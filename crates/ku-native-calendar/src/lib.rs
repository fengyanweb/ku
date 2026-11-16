//! Thin native ABI for the same calendar engine used by std.time. No Ku value,
//! interpreter, compiler, scheduler or process is invoked by these operations.
use std::{
    alloc::{alloc, dealloc, Layout},
    borrow::Cow,
    ffi::c_void,
    mem::{align_of, size_of},
    panic::{catch_unwind, AssertUnwindSafe},
    ptr, slice,
};

#[path = "../../../src/stdlib/time_calendar.rs"]
pub mod calendar;
use calendar::{CalendarError, CalendarResult};

#[cfg(not(panic = "unwind"))]
compile_error!("ku-native-calendar requires panic=unwind so panics cannot cross its C ABI");

const BUILD_ID: &[u8] = b"ku-native-calendar/0.1.0;abi=1;chrono=0.4.45;engine=shared";
const OK: u32 = 0;
const ERROR: u32 = 1;
const INVALID_ABI: u32 = 2;
const PANIC: u32 = 255;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KuCalendarSliceV1 {
    pub data: *const u8,
    pub len: usize,
}

impl KuCalendarSliceV1 {
    const EMPTY: Self = Self {
        data: ptr::null(),
        len: 0,
    };
    fn view(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            Self::EMPTY
        } else {
            Self {
                data: bytes.as_ptr(),
                len: bytes.len(),
            }
        }
    }
}

#[repr(C)]
pub struct KuCalendarInputV1 {
    pub size: u32,
    pub op: u32,
    pub argc: u32,
    pub reserved: u32,
    pub numbers: [i64; 8],
    pub text: [KuCalendarSliceV1; 3],
}

#[repr(C)]
pub struct KuCalendarOutputV1 {
    pub size: u32,
    pub status: u32,
    pub numbers: [i64; 10],
    pub text: KuCalendarSliceV1,
    pub code: KuCalendarSliceV1,
    pub message: KuCalendarSliceV1,
    pub owner: *mut c_void,
}

impl KuCalendarOutputV1 {
    fn empty() -> Self {
        Self {
            size: size_of::<Self>() as u32,
            status: OK,
            numbers: [0; 10],
            text: KuCalendarSliceV1::EMPTY,
            code: KuCalendarSliceV1::EMPTY,
            message: KuCalendarSliceV1::EMPTY,
            owner: ptr::null_mut(),
        }
    }
}

struct OwnedOutput {
    text: String,
    message: Cow<'static, str>,
}

struct Produced {
    numbers: [i64; 10],
    text: String,
}

impl Produced {
    fn numeric(value: i64) -> Self {
        let mut numbers = [0; 10];
        numbers[0] = value;
        Self {
            numbers,
            text: String::new(),
        }
    }
    fn date(date: calendar::Date) -> Self {
        let mut result = Self::numeric(i64::from(date.year));
        result.numbers[1] = i64::from(date.month);
        result.numbers[2] = i64::from(date.day);
        result
    }
}

unsafe fn text(input: &KuCalendarInputV1, index: usize) -> CalendarResult<&str> {
    let bytes = input.text[index];
    if bytes.len > isize::MAX as usize || (bytes.len != 0 && bytes.data.is_null()) {
        return Err(CalendarError::new(
            "invalid_arguments",
            "invalid calendar text range",
        ));
    }
    let data = if bytes.len == 0 {
        &[][..]
    }
    // SAFETY: The C ABI requires a readable input range for each nonempty slice.
    else {
        unsafe { slice::from_raw_parts(bytes.data, bytes.len) }
    };
    std::str::from_utf8(data)
        .map_err(|_| CalendarError::new("invalid_arguments", "calendar text is not UTF-8"))
}

fn valid_shape(op: u32, argc: u32) -> bool {
    matches!(
        (op, argc),
        (1, 0) | (2, 1..=2) | (3, 3) | (4, 6..=7) | (5, 1..=3) | (6, 1..=3) | (7..=9, 1..=2)
    )
}

fn used_text(op: u32, argc: u32, index: usize) -> bool {
    match op {
        2 | 7..=9 => argc == 2 && index == 0,
        4 => argc == 7 && index == 0,
        5 => argc >= 2 && index == 0 || argc == 3 && index == 1,
        6 => index < argc as usize,
        _ => false,
    }
}

fn overlaps(first: usize, len: usize, second: usize, second_len: usize) -> bool {
    match (first.checked_add(len), second.checked_add(second_len)) {
        (Some(end), Some(second_end)) => first < second_end && second < end,
        _ => true,
    }
}

unsafe fn evaluate(input: &KuCalendarInputV1) -> CalendarResult<Produced> {
    let n = input.numbers;
    // SAFETY: Entry validated shape, and every text remains borrowed only
    // inside this synchronous call.
    unsafe {
        match input.op {
            1 => Ok(Produced::date(calendar::date_now())),
            2 => Ok(Produced::date(calendar::date_at(
                n[0],
                if input.argc == 2 {
                    Some(text(input, 0)?)
                } else {
                    None
                },
            )?)),
            3 => Ok(Produced::date(calendar::date(
                calendar::expect_i32(n[0])?,
                calendar::expect_u32(n[1])?,
                calendar::expect_u32(n[2])?,
            )?)),
            4 => Ok(Produced::numeric(calendar::datetime(
                calendar::expect_i32(n[0])?,
                calendar::expect_u32(n[1])?,
                calendar::expect_u32(n[2])?,
                calendar::expect_u32(n[3])?,
                calendar::expect_u32(n[4])?,
                calendar::expect_u32(n[5])?,
                if input.argc == 7 {
                    Some(text(input, 0)?)
                } else {
                    None
                },
            )?)),
            5 => Ok(Produced {
                numbers: [0; 10],
                text: calendar::format(
                    n[0],
                    if input.argc >= 2 {
                        text(input, 0)?
                    } else {
                        calendar::DEFAULT_LAYOUT
                    },
                    if input.argc == 3 {
                        Some(text(input, 1)?)
                    } else {
                        None
                    },
                )?,
            }),
            6 => Ok(Produced::numeric(calendar::parse(
                text(input, 0)?,
                if input.argc >= 2 {
                    text(input, 1)?
                } else {
                    calendar::DEFAULT_LAYOUT
                },
                if input.argc == 3 {
                    Some(text(input, 2)?)
                } else {
                    None
                },
            )?)),
            7 => {
                let parts = calendar::parts(
                    n[0],
                    if input.argc == 2 {
                        Some(text(input, 0)?)
                    } else {
                        None
                    },
                )?;
                let mut result = Produced::date(parts.date);
                result.numbers[3..8].copy_from_slice(&[
                    i64::from(parts.hour),
                    i64::from(parts.minute),
                    i64::from(parts.second),
                    i64::from(parts.millis),
                    i64::from(parts.weekday),
                ]);
                result.text = parts.zone;
                Ok(result)
            }
            8 => Ok(Produced::numeric(calendar::weekday_time(
                n[0],
                if input.argc == 2 {
                    Some(text(input, 0)?)
                } else {
                    None
                },
            )?)),
            9 => {
                // The public Date branch validates the argument's string type,
                // but intentionally does not reinterpret the Date in that zone.
                if input.argc == 2 {
                    let _ = text(input, 0)?;
                }
                Ok(Produced::numeric(calendar::weekday_date(
                    calendar::expect_i32(n[0])?,
                    calendar::expect_u32(n[1])?,
                    calendar::expect_u32(n[2])?,
                )?))
            }
            _ => unreachable!(),
        }
    }
}

fn static_failure(
    output: &mut KuCalendarOutputV1,
    status: u32,
    code: &'static str,
    message: &'static str,
) -> u32 {
    output.status = status;
    output.code = KuCalendarSliceV1::view(code.as_bytes());
    output.message = KuCalendarSliceV1::view(message.as_bytes());
    status
}

fn install(output: &mut KuCalendarOutputV1, result: CalendarResult<Produced>) -> u32 {
    install_with_allocator(output, result, |layout| {
        // SAFETY: The matching owner release uses precisely this Layout.
        unsafe { alloc(layout) }
    })
}

fn install_with_allocator(
    output: &mut KuCalendarOutputV1,
    result: CalendarResult<Produced>,
    allocate: impl FnOnce(Layout) -> *mut u8,
) -> u32 {
    let (text, message) = match result {
        Ok(value) => {
            output.numbers = value.numbers;
            (value.text, Cow::Borrowed(""))
        }
        Err(error) => {
            output.status = ERROR;
            output.code = KuCalendarSliceV1::view(error.code.as_bytes());
            (String::new(), error.message)
        }
    };
    if text.is_empty() && matches!(message, Cow::Borrowed(_)) {
        output.message = KuCalendarSliceV1::view(message.as_bytes());
        return output.status;
    }
    // Allocation is fallible; no Box::new or Rust allocator mismatch reaches C.
    // SAFETY: The allocation's exact Layout is paired by release_v1 below.
    let owner = allocate(Layout::new::<OwnedOutput>()).cast::<OwnedOutput>();
    if owner.is_null() {
        output.numbers = [0; 10];
        return static_failure(output, ERROR, "out_of_memory", "time allocation failed");
    }
    // SAFETY: owner is a fresh aligned allocation for this exact object.
    unsafe {
        ptr::write(owner, OwnedOutput { text, message });
        output.text = KuCalendarSliceV1::view((*owner).text.as_bytes());
        output.message = KuCalendarSliceV1::view((*owner).message.as_bytes());
    }
    output.owner = owner.cast();
    output.status
}

#[no_mangle]
pub extern "C" fn __ku_calendar_abi_version() -> u32 {
    1
}

#[no_mangle]
/// Return a process-lifetime build identity view.
///
/// # Safety
/// `data` and `len` must be writable, aligned and non-overlapping when nonnull.
pub unsafe extern "C" fn __ku_calendar_v1_build_id(data: *mut *const u8, len: *mut usize) -> u32 {
    if data.is_null() || len.is_null() {
        return INVALID_ABI;
    }
    // SAFETY: The caller provides the two valid output locations.
    unsafe {
        ptr::write(data, BUILD_ID.as_ptr());
        ptr::write(len, BUILD_ID.len());
    }
    OK
}

#[no_mangle]
/// Execute one calendar operation without unwinding across C.
///
/// # Safety
/// Both objects and every nonempty input slice are live and readable/writable
/// as appropriate. Objects and output byte views do not overlap input storage.
/// Output is initialized, correctly sized, and has no live owner.
pub unsafe extern "C" fn __ku_calendar_call_v1(
    input: *const KuCalendarInputV1,
    output: *mut KuCalendarOutputV1,
) -> u32 {
    if input.is_null()
        || output.is_null()
        || (input as usize) % align_of::<KuCalendarInputV1>() != 0
        || (output as usize) % align_of::<KuCalendarOutputV1>() != 0
    {
        return INVALID_ABI;
    }
    if overlaps(
        input as usize,
        size_of::<KuCalendarInputV1>(),
        output as usize,
        size_of::<KuCalendarOutputV1>(),
    ) {
        return INVALID_ABI;
    }
    // SAFETY: The ABI contract guarantees initialized full nonoverlapping objects.
    let (input, current) = unsafe { (&*input, &*output) };
    if current.size != size_of::<KuCalendarOutputV1>() as u32 || !current.owner.is_null() {
        return INVALID_ABI;
    }
    let valid = input.size == size_of::<KuCalendarInputV1>() as u32
        && input.reserved == 0
        && valid_shape(input.op, input.argc);
    let valid_text = valid
        && input.text.iter().enumerate().all(|(index, text)| {
            !used_text(input.op, input.argc, index)
                || text.len == 0
                || !text.data.is_null()
                    && text.len <= isize::MAX as usize
                    && !overlaps(
                        text.data as usize,
                        text.len,
                        output as *const _ as usize,
                        size_of::<KuCalendarOutputV1>(),
                    )
        });
    // SAFETY: The readonly current borrow has ended and no input byte view
    // aliases output storage. Invalid calls construct no borrowed text slices.
    let output = unsafe { &mut *output };
    *output = KuCalendarOutputV1::empty();
    if !valid || !valid_text {
        return static_failure(
            output,
            INVALID_ABI,
            "invalid_arguments",
            "calendar ABI mismatch",
        );
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: evaluate only borrows the validated call's input storage.
        install(output, unsafe { evaluate(input) })
    }));
    match result {
        Ok(status) => status,
        Err(_) => static_failure(
            output,
            PANIC,
            "native_calendar_panic",
            "calendar operation failed",
        ),
    }
}

#[no_mangle]
/// Release output using the same Rust allocator that created it.
///
/// # Safety
/// Output is live, correctly sized, and unmodified since call_v1 or release_v1.
/// A nonnull owner came from this library and has not been independently freed.
pub unsafe extern "C" fn __ku_calendar_release_v1(output: *mut KuCalendarOutputV1) -> u32 {
    if output.is_null() || (output as usize) % align_of::<KuCalendarOutputV1>() != 0 {
        return INVALID_ABI;
    }
    // SAFETY: The caller guarantees an initialized output object.
    let output = unsafe { &mut *output };
    if output.size != size_of::<KuCalendarOutputV1>() as u32 {
        return INVALID_ABI;
    }
    let owner = output.owner.cast::<OwnedOutput>();
    *output = KuCalendarOutputV1::empty();
    if owner.is_null() {
        return OK;
    }
    match catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: This exact owner/Layout was allocated by install. Resetting
        // the caller first makes repeated release of the output idempotent.
        unsafe {
            ptr::drop_in_place(owner);
            dealloc(owner.cast(), Layout::new::<OwnedOutput>());
        }
    })) {
        Ok(()) => OK,
        Err(_) => PANIC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(op: u32, argc: u32) -> KuCalendarInputV1 {
        KuCalendarInputV1 {
            size: size_of::<KuCalendarInputV1>() as u32,
            op,
            argc,
            reserved: 0,
            numbers: [0; 8],
            text: [KuCalendarSliceV1::EMPTY; 3],
        }
    }

    fn string(view: KuCalendarSliceV1) -> String {
        if view.len == 0 {
            return String::new();
        }
        // SAFETY: Tests inspect views only while their owner is live.
        String::from_utf8(unsafe { slice::from_raw_parts(view.data, view.len) }.to_vec()).unwrap()
    }

    fn call(input: &KuCalendarInputV1, output: &mut KuCalendarOutputV1) -> u32 {
        // SAFETY: Both objects are live and disjoint; test input views remain live.
        unsafe { __ku_calendar_call_v1(input, output) }
    }

    fn release(output: &mut KuCalendarOutputV1) {
        // SAFETY: All test outputs were initialized or returned by this ABI.
        assert_eq!(unsafe { __ku_calendar_release_v1(output) }, OK);
    }

    #[test]
    fn abi_layout_and_build_identity_are_frozen() {
        assert_eq!(__ku_calendar_abi_version(), 1);
        assert_eq!(align_of::<KuCalendarInputV1>(), align_of::<i64>());
        assert_eq!(align_of::<KuCalendarOutputV1>(), align_of::<i64>());
        if size_of::<usize>() == 8 {
            assert_eq!(size_of::<KuCalendarInputV1>(), 128);
            assert_eq!(size_of::<KuCalendarOutputV1>(), 144);
        }
        let mut data = ptr::null();
        let mut len = 0;
        // SAFETY: Valid distinct outputs receive a process-lifetime view.
        assert_eq!(
            unsafe { __ku_calendar_v1_build_id(&mut data, &mut len) },
            OK
        );
        assert_eq!(
            string(KuCalendarSliceV1 { data, len }),
            "ku-native-calendar/0.1.0;abi=1;chrono=0.4.45;engine=shared"
        );
        // SAFETY: NULL is explicitly rejected without dereference.
        assert_eq!(
            unsafe { __ku_calendar_v1_build_id(ptr::null_mut(), &mut len) },
            INVALID_ABI
        );
    }

    #[test]
    fn every_operation_uses_the_shared_engine() {
        let mut out = KuCalendarOutputV1::empty();
        assert_eq!(call(&input(1, 0), &mut out), OK);
        assert!(calendar::date(
            out.numbers[0] as i32,
            out.numbers[1] as u32,
            out.numbers[2] as u32
        )
        .is_ok());
        release(&mut out);
        let mut request = input(2, 2);
        request.text[0] = KuCalendarSliceV1::view(b"-05:00");
        assert_eq!(call(&request, &mut out), OK);
        assert_eq!(&out.numbers[..3], &[1969, 12, 31]);
        release(&mut out);
        request = input(3, 3);
        request.numbers[..3].copy_from_slice(&[2000, 2, 29]);
        assert_eq!(call(&request, &mut out), OK);
        assert_eq!(&out.numbers[..3], &[2000, 2, 29]);
        release(&mut out);
        request = input(4, 7);
        request.numbers[..6].copy_from_slice(&[1970, 1, 1, 8, 0, 0]);
        request.text[0] = KuCalendarSliceV1::view(b"+08:00");
        assert_eq!(call(&request, &mut out), OK);
        assert_eq!(out.numbers[0], 0);
        release(&mut out);
        request = input(5, 3);
        request.numbers[0] = 123;
        request.text[0] = KuCalendarSliceV1::view(b"yyyy-MM-dd HH:mm:ss.SSS");
        request.text[1] = KuCalendarSliceV1::view(b"UTC");
        assert_eq!(call(&request, &mut out), OK);
        assert_eq!(string(out.text), "1970-01-01 00:00:00.123");
        release(&mut out);
        request = input(6, 3);
        request.text = [
            KuCalendarSliceV1::view(b"1970-01-01 00:00:00.123"),
            KuCalendarSliceV1::view(b"yyyy-MM-dd HH:mm:ss.SSS"),
            KuCalendarSliceV1::view(b"UTC"),
        ];
        assert_eq!(call(&request, &mut out), OK);
        assert_eq!(out.numbers[0], 123);
        release(&mut out);
        request = input(7, 2);
        request.numbers[0] = -1;
        request.text[0] = KuCalendarSliceV1::view(b"UTC");
        assert_eq!(call(&request, &mut out), OK);
        assert_eq!(&out.numbers[..8], &[1969, 12, 31, 23, 59, 59, 999, 3]);
        assert_eq!(string(out.text), "UTC");
        release(&mut out);
        request = input(8, 2);
        request.text[0] = KuCalendarSliceV1::view(b"UTC");
        assert_eq!(call(&request, &mut out), OK);
        assert_eq!(out.numbers[0], 4);
        release(&mut out);
        request = input(9, 2);
        request.numbers[..3].copy_from_slice(&[2000, 1, 1]);
        request.text[0] = KuCalendarSliceV1::view(b"ignored Date zone");
        assert_eq!(call(&request, &mut out), OK);
        assert_eq!(out.numbers[0], 6);
        release(&mut out);
    }

    #[test]
    fn output_owns_its_text_and_matched_release_is_idempotent() {
        let mut zone = String::from("+08:00");
        let mut request = input(7, 2);
        request.text[0] = KuCalendarSliceV1::view(zone.as_bytes());
        let mut out = KuCalendarOutputV1::empty();
        assert_eq!(call(&request, &mut out), OK);
        let owner = out.owner;
        assert!(!owner.is_null());
        zone.clear();
        drop(zone);
        assert_eq!(string(out.text), "+08:00");
        // A caller cannot accidentally overwrite and leak a live owner.
        assert_eq!(call(&input(1, 0), &mut out), INVALID_ABI);
        assert_eq!(out.owner, owner);
        assert_eq!(string(out.text), "+08:00");
        release(&mut out);
        assert!(out.owner.is_null());
        assert!(out.text.data.is_null());
        assert!(out.code.data.is_null());
        assert!(out.message.data.is_null());
        release(&mut out);
        assert_eq!(call(&input(1, 0), &mut out), OK);
        release(&mut out);
    }

    #[test]
    fn abi_shape_ranges_and_utf8_fail_without_creating_an_owner() {
        let mut out = KuCalendarOutputV1::empty();
        // SAFETY: The API rejects NULL before reading either object.
        assert_eq!(
            unsafe { __ku_calendar_call_v1(ptr::null(), &mut out) },
            INVALID_ABI
        );
        assert_eq!(
            unsafe { __ku_calendar_call_v1(&input(1, 0), ptr::null_mut()) },
            INVALID_ABI
        );
        for (op, argc) in [(0, 0), (10, 0), (1, 1), (5, 0), (5, 4), (9, 3)] {
            assert_eq!(call(&input(op, argc), &mut out), INVALID_ABI);
            assert!(out.owner.is_null());
            release(&mut out);
        }
        let mut request = input(1, 0);
        request.size -= 1;
        assert_eq!(call(&request, &mut out), INVALID_ABI);
        release(&mut out);
        request = input(1, 0);
        request.reserved = 1;
        assert_eq!(call(&request, &mut out), INVALID_ABI);
        release(&mut out);
        request = input(7, 2);
        request.text[0] = KuCalendarSliceV1 {
            data: ptr::null(),
            len: 1,
        };
        assert_eq!(call(&request, &mut out), INVALID_ABI);
        release(&mut out);
        request.text[0] = KuCalendarSliceV1 {
            data: (&out as *const KuCalendarOutputV1).cast(),
            len: 1,
        };
        assert_eq!(call(&request, &mut out), INVALID_ABI);
        release(&mut out);
        request.text[0] = KuCalendarSliceV1::view(&[0xff]);
        assert_eq!(call(&request, &mut out), ERROR);
        assert_eq!(string(out.code), "invalid_arguments");
        assert!(out.owner.is_null());
        release(&mut out);
    }

    #[test]
    fn error_views_and_embedded_nul_are_not_c_strings() {
        let mut request = input(5, 3);
        request.text[0] = KuCalendarSliceV1::view("前\0yyyy-MM-dd后".as_bytes());
        request.text[1] = KuCalendarSliceV1::view(b"UTC");
        let mut out = KuCalendarOutputV1::empty();
        assert_eq!(call(&request, &mut out), OK);
        assert_eq!(string(out.text), "前\01970-01-01后");
        release(&mut out);
        request.text[1] = KuCalendarSliceV1::view(b"UTC\0");
        assert_eq!(call(&request, &mut out), ERROR);
        assert_eq!(string(out.code), "invalid_zone");
        assert_eq!(string(out.message), "invalid zone: UTC\0");
        assert!(!out.owner.is_null());
        release(&mut out);
        request.text[1] = KuCalendarSliceV1::view(b"UTC");
        request.text[0] = KuCalendarSliceV1::view(b"%Q");
        assert_eq!(call(&request, &mut out), ERROR);
        assert_eq!(string(out.code), "invalid_format");
        release(&mut out);
    }

    #[test]
    fn owner_allocation_failure_is_a_releasable_error() {
        let mut out = KuCalendarOutputV1::empty();
        let value = Produced {
            numbers: [42; 10],
            text: String::from("owned output"),
        };
        assert_eq!(
            install_with_allocator(&mut out, Ok(value), |_| ptr::null_mut()),
            ERROR
        );
        assert_eq!(string(out.code), "out_of_memory");
        assert_eq!(out.numbers, [0; 10]);
        assert!(out.owner.is_null());
        release(&mut out);
        let error = CalendarError {
            code: "invalid_zone",
            message: Cow::Owned(String::from("dynamic detail")),
        };
        assert_eq!(
            install_with_allocator(&mut out, Err(error), |_| ptr::null_mut()),
            ERROR
        );
        assert_eq!(string(out.code), "out_of_memory");
        release(&mut out);
        // Numeric output does not call the owner allocator.
        assert_eq!(
            install_with_allocator(&mut out, Ok(Produced::numeric(123)), |_| panic!(
                "numeric output allocated"
            )),
            OK
        );
        assert_eq!(out.numbers[0], 123);
        release(&mut out);
    }

    #[test]
    fn concurrent_calls_keep_context_and_ownership_independent() {
        std::thread::scope(|scope| {
            for (zone, expected) in [
                ("UTC", "1970-01-01 00:00:00.123"),
                ("+08:00", "1970-01-01 08:00:00.123"),
                ("-05:00", "1969-12-31 19:00:00.123"),
            ] {
                scope.spawn(move || {
                    for _ in 0..64 {
                        let mut request = input(5, 3);
                        request.numbers[0] = 123;
                        request.text[0] = KuCalendarSliceV1::view(b"yyyy-MM-dd HH:mm:ss.SSS");
                        request.text[1] = KuCalendarSliceV1::view(zone.as_bytes());
                        let mut out = KuCalendarOutputV1::empty();
                        assert_eq!(call(&request, &mut out), OK);
                        assert_eq!(string(out.text), expected);
                        release(&mut out);
                    }
                });
            }
        });
    }
}
