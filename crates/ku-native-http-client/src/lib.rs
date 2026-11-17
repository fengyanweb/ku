//! Synchronous native HTTP client; no Ku Value, interpreter or scheduler.
use std::{
    alloc::{alloc, dealloc, Layout},
    collections::HashMap,
    ffi::c_void,
    mem::{align_of, size_of},
    panic::{catch_unwind, AssertUnwindSafe},
    ptr, slice,
};
mod asynchronous;
#[path = "../../../src/stdlib/http_client_engine.rs"]
pub mod engine;
#[cfg(not(panic = "unwind"))]
compile_error!("ku-native-http-client requires panic=unwind");
const BUILD_ID: &[u8] = b"ku-native-http-client/0.1.0;abi=1;ureq=2.12.1;url=2.5.8;engine=shared";
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Slice {
    pub data: *const u8,
    pub len: usize,
}
impl Slice {
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
pub struct Header {
    pub name: Slice,
    pub value: Slice,
}
#[repr(C)]
pub struct Input {
    pub size: u32,
    pub flags: u32,
    pub method: Slice,
    pub url: Slice,
    pub body: Slice,
    pub headers: *const Header,
    pub header_count: usize,
    pub timeout_ms: u64,
    pub caller_budget_ms: u64,
    pub max_body_bytes: usize,
}
#[repr(C)]
pub struct Output {
    pub size: u32,
    pub status: u32,
    pub http_status: i64,
    pub body: Slice,
    pub headers: *const Header,
    pub header_count: usize,
    pub code: Slice,
    pub message: Slice,
    pub owner: *mut c_void,
}
impl Output {
    fn empty() -> Self {
        Self {
            size: size_of::<Self>() as u32,
            status: 0,
            http_status: 0,
            body: Slice::EMPTY,
            headers: ptr::null(),
            header_count: 0,
            code: Slice::EMPTY,
            message: Slice::EMPTY,
            owner: ptr::null_mut(),
        }
    }
}
struct Owner {
    response: Option<engine::HttpResponse>,
    headers: Vec<Header>,
    code: String,
    message: String,
}
fn copy(text: &str) -> Result<String, u32> {
    let mut s = String::new();
    s.try_reserve_exact(text.len()).map_err(|_| 1u32)?;
    s.push_str(text);
    Ok(s)
}
unsafe fn text<'a>(s: Slice) -> Result<&'a str, u32> {
    if s.len > isize::MAX as usize
        || s.len != 0 && s.data.is_null()
        || (s.data as usize).checked_add(s.len).is_none()
    {
        return Err(2u32);
    }
    let bytes = if s.len == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(s.data, s.len) }
    };
    std::str::from_utf8(bytes).map_err(|_| 2u32)
}
fn overlaps(a: usize, n: usize, b: usize, m: usize) -> bool {
    match (a.checked_add(n), b.checked_add(m)) {
        (Some(ae), Some(be)) => a < be && b < ae,
        _ => true,
    }
}
unsafe fn decode_request(input: &Input) -> Result<engine::HttpRequest, u32> {
    let mut headers = HashMap::new();
    headers.try_reserve(input.header_count).map_err(|_| 1u32)?;
    let raw = if input.header_count == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(input.headers, input.header_count) }
    };
    for h in raw {
        let n = copy(unsafe { text(h.name) }?)?;
        let v = copy(unsafe { text(h.value) }?)?;
        if headers.insert(n, v).is_some() {
            return Err(2u32);
        }
    }
    let request = engine::HttpRequest {
        method: copy(unsafe { text(input.method) }?)?,
        url: copy(unsafe { text(input.url) }?)?,
        headers,
        body: if input.flags & 1 != 0 {
            Some(copy(unsafe { text(input.body) }?)?)
        } else {
            None
        },
        timeout_ms: input.timeout_ms,
        max_body_bytes: input.max_body_bytes,
    };

    Ok(request)
}
unsafe fn execute(input: &Input, deadline: Option<std::time::Instant>) -> Result<Owner, u32> {
    let request = unsafe { decode_request(input) }?;
    match engine::http_request_with_deadline(request, deadline) {
        Ok(response) => {
            let mut headers = Vec::new();
            headers
                .try_reserve_exact(response.headers.len())
                .map_err(|_| 1u32)?;
            for (name, value) in &response.headers {
                headers.push(Header {
                    name: Slice::view(name.as_bytes()),
                    value: Slice::view(value.as_bytes()),
                })
            }
            Ok(Owner {
                response: Some(response),
                headers,
                code: String::new(),
                message: String::new(),
            })
        }
        Err(error) => Ok(Owner {
            response: None,
            headers: Vec::new(),
            code: error.code,
            message: error.message,
        }),
    }
}
#[no_mangle]
pub extern "C" fn __ku_http_client_abi_version() -> u32 {
    1
}
/// # Safety
/// Non-null output pointers name writable, suitably aligned storage.
#[no_mangle]
pub unsafe extern "C" fn __ku_http_client_v1_build_id(
    data: *mut *const u8,
    len: *mut usize,
) -> u32 {
    if data.is_null()
        || len.is_null()
        || !(data as usize).is_multiple_of(align_of::<*const u8>())
        || !(len as usize).is_multiple_of(align_of::<usize>())
        || overlaps(
            data as usize,
            size_of::<*const u8>(),
            len as usize,
            size_of::<usize>(),
        )
    {
        return 2;
    }
    unsafe {
        *data = BUILD_ID.as_ptr();
        *len = BUILD_ID.len()
    };
    0
}
/// # Safety
/// Input descriptors/ranges are valid for the call. Output is distinct writable
/// initialized storage. Only a live owner created by this library may be released.
#[no_mangle]
pub unsafe extern "C" fn __ku_http_client_call_v1(input: *const Input, output: *mut Output) -> u32 {
    if input.is_null()
        || output.is_null()
        || !(input as usize).is_multiple_of(align_of::<Input>())
        || !(output as usize).is_multiple_of(align_of::<Output>())
        || overlaps(
            input as usize,
            size_of::<Input>(),
            output as usize,
            size_of::<Output>(),
        )
    {
        return 2;
    }
    let (i, o) = unsafe { (&*input, &mut *output) };
    if o.size != size_of::<Output>() as u32 || !o.owner.is_null() {
        return 2;
    }
    if i.size != size_of::<Input>() as u32
        || i.flags > 1
        || i.timeout_ms == 0
        || i.timeout_ms > engine::MAX_TIMEOUT_MS
        || i.max_body_bytes == 0
        || i.max_body_bytes > engine::MAX_BODY_BYTES
    {
        return 2;
    }
    let Some(header_bytes) = i.header_count.checked_mul(size_of::<Header>()) else {
        return 2;
    };
    if header_bytes > isize::MAX as usize
        || header_bytes != 0
            && (i.headers.is_null() || !(i.headers as usize).is_multiple_of(align_of::<Header>()))
        || overlaps(
            i.headers as usize,
            header_bytes,
            output as usize,
            size_of::<Output>(),
        )
    {
        return 2;
    }
    for s in [i.method, i.url, i.body] {
        if overlaps(s.data as usize, s.len, output as usize, size_of::<Output>()) {
            return 2;
        }
    }
    let raw = if i.header_count == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(i.headers, i.header_count) }
    };
    for h in raw {
        for s in [h.name, h.value] {
            if overlaps(s.data as usize, s.len, output as usize, size_of::<Output>()) {
                return 2;
            }
        }
    }
    *o = Output::empty();
    let deadline = if i.caller_budget_ms == 0 {
        None
    } else {
        std::time::Instant::now().checked_add(std::time::Duration::from_millis(i.caller_budget_ms))
    };
    let produced = catch_unwind(AssertUnwindSafe(|| unsafe { execute(i, deadline) }));
    let owned = match produced {
        Ok(Ok(owner)) => owner,
        Ok(Err(2)) => {
            o.status = 2;
            return 2;
        }
        Ok(Err(_)) => {
            o.status = 1;
            o.code = Slice::view(b"out_of_memory");
            o.message = Slice::view(b"HTTP input or output allocation failed");
            return 1;
        }
        Err(_) => {
            o.status = 255;
            o.code = Slice::view(b"internal_panic");
            o.message = Slice::view(b"HTTP client failed internally");
            return 255;
        }
    };
    unsafe { publish_owner(owned, o) }
}
unsafe fn publish_owner(owned: Owner, o: &mut Output) -> u32 {
    let memory = unsafe { alloc(Layout::new::<Owner>()) } as *mut Owner;
    if memory.is_null() {
        o.status = 1;
        o.code = Slice::view(b"out_of_memory");
        o.message = Slice::view(b"HTTP owner allocation failed");
        return 1;
    }
    unsafe { ptr::write(memory, owned) };
    let owner = unsafe { &*memory };
    o.owner = memory.cast();
    if let Some(response) = &owner.response {
        o.http_status = response.status;
        o.body = Slice::view(response.body.as_bytes());
        o.headers = owner.headers.as_ptr();
        o.header_count = owner.headers.len();
        0
    } else {
        o.status = 1;
        o.code = Slice::view(owner.code.as_bytes());
        o.message = Slice::view(owner.message.as_bytes());
        1
    }
}
/// # Safety
/// Output is initialized writable storage and its live owner, if any, was
/// returned by this library and has not been released elsewhere.
#[no_mangle]
pub unsafe extern "C" fn __ku_http_client_release_v1(output: *mut Output) -> u32 {
    if output.is_null() || !(output as usize).is_multiple_of(align_of::<Output>()) {
        return 2;
    }
    let o = unsafe { &mut *output };
    if o.size != size_of::<Output>() as u32 {
        return 2;
    }
    if !o.owner.is_null() {
        unsafe {
            let p = o.owner.cast::<Owner>();
            ptr::drop_in_place(p);
            dealloc(p.cast(), Layout::new::<Owner>())
        }
    }
    *o = Output::empty();
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };

    fn input(method: &str, url: &str, body: Option<&str>) -> Input {
        Input {
            size: size_of::<Input>() as u32,
            flags: u32::from(body.is_some()),
            method: Slice::view(method.as_bytes()),
            url: Slice::view(url.as_bytes()),
            body: Slice::view(body.unwrap_or("").as_bytes()),
            headers: ptr::null(),
            header_count: 0,
            timeout_ms: 500,
            caller_budget_ms: 0,
            max_body_bytes: 4096,
        }
    }
    fn peer(response: &'static [u8]) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let thread = thread::spawn(move || {
            let stop = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < stop =>
                    {
                        thread::sleep(Duration::from_millis(1))
                    }
                    other => panic!("bounded peer accept failed: {other:?}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).unwrap();
                assert!(count != 0 && bytes.len() + count <= 16384);
                bytes.extend_from_slice(&chunk[..count]);
            }
            stream.write_all(response).unwrap();
            bytes
        });
        (url, thread)
    }
    #[test]
    fn abi_layout_invalid_descriptors_and_idempotent_release() {
        assert_eq!(size_of::<Slice>(), 16);
        assert_eq!(size_of::<Header>(), 32);
        assert_eq!(size_of::<Input>(), 96);
        assert_eq!(size_of::<Output>(), 88);
        let mut data = ptr::null();
        let mut len = 0;
        assert_eq!(
            unsafe { __ku_http_client_v1_build_id(&mut data, &mut len) },
            0
        );
        assert_eq!(unsafe { slice::from_raw_parts(data, len) }, BUILD_ID);
        let mut shared_word = 0usize;
        let data_alias = ptr::addr_of_mut!(shared_word).cast::<*const u8>();
        let len_alias = ptr::addr_of_mut!(shared_word);
        assert_eq!(
            unsafe { __ku_http_client_v1_build_id(data_alias, len_alias) },
            2
        );
        assert_eq!(shared_word, 0, "rejected alias must not be written");
        let mut out = Output::empty();
        assert_eq!(
            unsafe { __ku_http_client_call_v1(ptr::null(), &mut out) },
            2
        );
        let mut i = input("GET", "http://127.0.0.1/", None);
        i.header_count = 1;
        assert_eq!(unsafe { __ku_http_client_call_v1(&i, &mut out) }, 2);
        assert!(out.owner.is_null());
        assert_eq!(unsafe { __ku_http_client_release_v1(&mut out) }, 0);
        assert_eq!(unsafe { __ku_http_client_release_v1(&mut out) }, 0);
    }
    #[test]
    fn owned_non_success_response_and_nul_body_survive_peer_exit() {
        let (url,peer)=peer(b"HTTP/1.1 404 Not Found\r\nX-Ku: yes\r\nContent-Length: 3\r\nConnection: close\r\n\r\na\0b");
        let i = input("GET", &url, None);
        let mut out = Output::empty();
        assert_eq!(unsafe { __ku_http_client_call_v1(&i, &mut out) }, 0);
        assert!(peer.join().unwrap().starts_with(b"GET / HTTP/1.1\r\n"));
        assert_eq!(out.http_status, 404);
        assert!(!out.owner.is_null());
        assert_eq!(unsafe { text(out.body) }.unwrap().as_bytes(), b"a\0b");
        let headers = unsafe { slice::from_raw_parts(out.headers, out.header_count) };
        assert!(headers
            .iter()
            .any(|h| unsafe { text(h.name) }.unwrap() == "x-ku"
                && unsafe { text(h.value) }.unwrap() == "yes"));
        assert_eq!(unsafe { __ku_http_client_release_v1(&mut out) }, 0);
        assert!(out.owner.is_null() && out.body.data.is_null() && out.header_count == 0);
    }
    #[test]
    fn bounded_body_utf8_and_operational_errors_release() {
        for (response, limit, code) in [
            (
                &b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n1234"[..],
                3,
                "body_too_large",
            ),
            (
                &b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n\xff"[..],
                4096,
                "invalid_utf8",
            ),
        ] {
            let (url, peer) = peer(response);
            let mut i = input("GET", &url, None);
            i.max_body_bytes = limit;
            let mut out = Output::empty();
            assert_eq!(unsafe { __ku_http_client_call_v1(&i, &mut out) }, 1);
            peer.join().unwrap();
            assert_eq!(unsafe { text(out.code) }.unwrap(), code);
            assert_eq!(unsafe { __ku_http_client_release_v1(&mut out) }, 0);
            assert!(out.owner.is_null());
        }
        for (method, url, code) in [
            ("DELETE", "http://127.0.0.1/", "invalid_method"),
            ("GET", "file:///tmp/test", "invalid_url"),
        ] {
            let i = input(method, url, None);
            let mut out = Output::empty();
            assert_eq!(unsafe { __ku_http_client_call_v1(&i, &mut out) }, 1);
            assert_eq!(unsafe { text(out.code) }.unwrap(), code);
            assert_eq!(unsafe { __ku_http_client_release_v1(&mut out) }, 0);
        }
    }
    #[test]
    fn elapsed_caller_deadline_is_not_restarted_before_io() {
        let request = engine::HttpRequest {
            method: "GET".into(),
            url: "http://127.0.0.1:1/".into(),
            headers: HashMap::new(),
            body: None,
            timeout_ms: 500,
            max_body_bytes: 4096,
        };
        let error = engine::http_request_with_deadline(
            request,
            Some(Instant::now() - Duration::from_millis(1)),
        )
        .unwrap_err();
        assert_eq!(error.code, "transport");
        assert!(error.message.contains("before I/O"));
    }
}
