use std::{
    collections::HashMap,
    ffi::{c_char, c_void, CStr},
    path::{Component, Path, PathBuf},
    ptr,
    sync::{Mutex, OnceLock},
};

use crate::{error::KuResult, span::Span, stdlib::errors, value::Value};

use super::{ReturnKind, MAX_ARGUMENTS, MAX_RESULT_BYTES, RELATIVE_RUNTIME_ROOT};

#[derive(Clone, Copy)]
#[repr(C)]
struct BridgeValue {
    tag: u32,
    reserved: u32,
    integer: i64,
    data: *const u8,
    length: usize,
}

impl BridgeValue {
    const fn empty() -> Self {
        Self {
            tag: 0,
            reserved: 0,
            integer: 0,
            data: ptr::null(),
            length: 0,
        }
    }

    fn from_value(value: &Value) -> Option<Self> {
        let mut result = Self::empty();
        match value {
            Value::Null => {}
            Value::Int(value) => {
                result.tag = 1;
                result.integer = *value;
            }
            Value::String(value) => {
                result.tag = 2;
                result.data = value.as_ptr();
                result.length = value.len();
            }
            Value::Bool(value) => {
                result.tag = 3;
                result.integer = i64::from(*value);
            }
            _ => return None,
        }
        Some(result)
    }

    fn text(&self) -> Result<&str, &'static str> {
        if self.length > MAX_RESULT_BYTES {
            return Err("interop string exceeds the result limit");
        }
        if self.length == 0 {
            return Ok("");
        }
        if self.data.is_null() {
            return Err("interop result has no string storage");
        }
        // SAFETY: A loaded bridge declares this ABI. Its result storage is
        // live until the corresponding bridge release function is called.
        let bytes = unsafe { std::slice::from_raw_parts(self.data, self.length) };
        std::str::from_utf8(bytes).map_err(|_| "interop bridge returned invalid UTF-8")
    }

    fn convert_result(&self, kind: ReturnKind) -> Result<Value, &'static str> {
        if self.tag != kind.tag()
            || self.reserved != 0
            || (kind != ReturnKind::Str && (!self.data.is_null() || self.length != 0))
        {
            return Err("interop bridge returned an unexpected type");
        }
        Ok(match kind {
            ReturnKind::Null => Value::Null,
            ReturnKind::Int => Value::Int(self.integer),
            ReturnKind::Str => {
                let value = self.text()?;
                let mut owned = String::new();
                owned
                    .try_reserve_exact(value.len())
                    .map_err(|_| "interop result allocation failed")?;
                owned.push_str(value);
                Value::String(owned)
            }
            ReturnKind::Bool if matches!(self.integer, 0 | 1) => Value::Bool(self.integer != 0),
            ReturnKind::Bool => return Err("interop boolean is not 0 or 1"),
        })
    }
}

type Call = unsafe extern "C" fn(*const c_char, *const BridgeValue, usize, *mut BridgeValue) -> i32;
type Release = unsafe extern "C" fn(*mut BridgeValue);

#[derive(Clone, Copy)]
struct Bridge {
    call: Call,
    release: Release,
}

// Loading is lazy and a successfully admitted bridge stays loaded for the
// process lifetime. Foreign owners and callbacks may outlive a single call.
static BRIDGES: OnceLock<Mutex<HashMap<String, Bridge>>> = OnceLock::new();

struct Output {
    value: BridgeValue,
    release: Release,
}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: The output was initialized for this exact plugin ABI.
        unsafe { (self.release)(&mut self.value) };
    }
}

pub(crate) fn invoke(
    kind: ReturnKind,
    library: &str,
    operation: &str,
    values: &[Value],
    span: Span,
) -> KuResult<Value> {
    let mut operation_buffer = [0; 201];
    if operation.len() >= operation_buffer.len() || operation.contains('\0') {
        return Ok(errors::err(
            "interop",
            "contract_error",
            "invalid interop operation",
        ));
    }
    operation_buffer[..operation.len()].copy_from_slice(operation.as_bytes());
    let operation = CStr::from_bytes_with_nul(&operation_buffer[..=operation.len()])
        .expect("operation has one trailing NUL");
    let bridge = match load(library) {
        Ok(bridge) => bridge,
        Err(message) => return Ok(errors::err("interop", "load_error", message)),
    };
    let mut arguments = [BridgeValue::empty(); MAX_ARGUMENTS];
    if values.len() > arguments.len() {
        return Ok(errors::err(
            "interop",
            "contract_error",
            "too many interop arguments",
        ));
    }
    with_arguments(values, &mut arguments, 0, span, &|args| {
        call_bridge(kind, bridge, operation, args)
    })
}

fn with_arguments(
    values: &[Value],
    arguments: &mut [BridgeValue],
    index: usize,
    span: Span,
    call: &impl Fn(&[BridgeValue]) -> Value,
) -> KuResult<Value> {
    let Some(value) = values.get(index) else {
        return Ok(call(&arguments[..index]));
    };
    value.with_read(span, |value| {
        let Some(argument) = BridgeValue::from_value(value) else {
            return Ok(errors::err(
                "interop",
                "contract_error",
                "unsupported interop argument type",
            ));
        };
        arguments[index] = argument;
        with_arguments(values, arguments, index + 1, span, call)
    })
}

fn call_bridge(
    kind: ReturnKind,
    bridge: Bridge,
    operation: &std::ffi::CStr,
    arguments: &[BridgeValue],
) -> Value {
    let mut output = Output {
        value: BridgeValue::empty(),
        release: bridge.release,
    };
    // SAFETY: Both functions were obtained from the same admitted library and
    // all borrowed byte slices and the operation remain live for this call.
    let status = unsafe {
        (bridge.call)(
            operation.as_ptr(),
            arguments.as_ptr(),
            arguments.len(),
            &mut output.value,
        )
    };
    if status != 0 {
        return match (output.value.tag, output.value.reserved, output.value.text()) {
            (2, 0, Ok(message)) => errors::err("interop", "call_error", message),
            _ => errors::err(
                "interop",
                "contract_error",
                "interop bridge returned an invalid error",
            ),
        };
    }
    match output.value.convert_result(kind) {
        Ok(value) => errors::ok(value),
        Err(message) => errors::err("interop", "contract_error", message),
    }
}

fn library_path(library: &str) -> Result<PathBuf, String> {
    let path = Path::new(library);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        if !path.starts_with(RELATIVE_RUNTIME_ROOT)
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("interop bridge is not bound to an installed package".into());
        }
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|parent| parent.join(path)))
            .ok_or_else(|| "could not locate the application interop directory".to_owned())?
    };
    path.canonicalize()
        .map_err(|error| format!("could not find interop bridge {}: {error}", path.display()))
}

fn load(library: &str) -> Result<Bridge, String> {
    let mut bridges = BRIDGES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| "interop library table is unavailable".to_owned())?;
    if let Some(bridge) = bridges.get(library) {
        return Ok(*bridge);
    }
    let path = library_path(library)?;
    let bridge = platform::load(&path)?;
    bridges.insert(library.to_owned(), bridge);
    Ok(bridge)
}

fn resolve_exports(
    path: &Path,
    resolve: impl Fn(&std::ffi::CStr) -> *mut c_void,
) -> Result<Bridge, String> {
    let call = resolve(c"ku_interop_call_v1");
    let release = resolve(c"ku_interop_free_v1");
    if call.is_null() || release.is_null() {
        return Err(format!(
            "{} does not export the Ku interop v1 ABI",
            path.display()
        ));
    }
    // SAFETY: The two non-null symbols are functions at the stable C ABI
    // names. This is equivalent to linking against an explicitly installed SDK.
    Ok(unsafe {
        Bridge {
            call: std::mem::transmute::<*mut c_void, Call>(call),
            release: std::mem::transmute::<*mut c_void, Release>(release),
        }
    })
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    pub(super) fn load(path: &Path) -> Result<Bridge, String> {
        let filename = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| "interop bridge path contains NUL".to_owned())?;
        // SAFETY: The filename is an absolute NUL-terminated path from a
        // resolved package and is live throughout dlopen.
        let handle = unsafe { libc::dlopen(filename.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!("could not load interop bridge {}", path.display()));
        }
        // SAFETY: handle came from successful dlopen, and symbol names are CStr.
        let result = resolve_exports(path, |symbol| unsafe {
            libc::dlsym(handle, symbol.as_ptr())
        });
        if result.is_err() {
            // SAFETY: No function pointers escaped an unsuccessful admission.
            unsafe { libc::dlclose(handle) };
        }
        result
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::FreeLibrary,
        System::LibraryLoader::{
            GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        },
    };

    pub(super) fn load(path: &Path) -> Result<Bridge, String> {
        let filename: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: Absolute UTF-16 filename with terminating NUL. The DLL
        // directory and secure system/application directories are searched.
        let handle = unsafe {
            LoadLibraryExW(
                filename.as_ptr(),
                ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if handle.is_null() {
            return Err(format!(
                "could not load interop bridge {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        let result = resolve_exports(path, |symbol| {
            // SAFETY: handle and NUL-terminated symbol are valid for this lookup.
            unsafe { GetProcAddress(handle, symbol.as_ptr().cast()) }
                .map_or(ptr::null_mut(), |address| address as *mut c_void)
        });
        if result.is_err() {
            // SAFETY: No function pointers escaped an unsuccessful admission.
            unsafe { FreeLibrary(handle) };
        }
        result
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;

    pub(super) fn load(_path: &Path) -> Result<Bridge, String> {
        Err("native interop is unavailable on this target".into())
    }
}
