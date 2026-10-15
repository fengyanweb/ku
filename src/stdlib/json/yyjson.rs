//! Private ownership wrappers. yyjson memory is always released by yyjson/C,
//! while returned Ku strings and containers are independent Rust allocations.
use std::{
    ffi::{c_char, c_void, CStr},
    marker::PhantomData,
    ptr::NonNull,
};

#[derive(Default)]
#[repr(C)]
struct Detail {
    code: u32,
    position: usize,
    message: *const c_char,
}

extern "C" {
    fn ku_yyjson_read(text: *const u8, length: usize, error: *mut Detail) -> *mut c_void;
    fn ku_yyjson_doc_free(doc: *mut c_void);
    fn ku_yyjson_root(doc: *const c_void) -> *mut c_void;
    fn ku_yyjson_kind(value: *const c_void) -> u32;
    fn ku_yyjson_bool(value: *const c_void) -> bool;
    fn ku_yyjson_int(value: *const c_void) -> i64;
    fn ku_yyjson_uint(value: *const c_void) -> u64;
    fn ku_yyjson_real(value: *const c_void) -> f64;
    fn ku_yyjson_length(value: *const c_void) -> usize;
    fn ku_yyjson_string(value: *const c_void) -> *const u8;
    fn ku_yyjson_raw(value: *const c_void) -> *const u8;
    fn ku_yyjson_first(value: *const c_void) -> *mut c_void;
    fn ku_yyjson_next(value: *const c_void) -> *mut c_void;
    fn ku_yyjson_builder_new() -> *mut c_void;
    fn ku_yyjson_builder_free(doc: *mut c_void);
    fn ku_yyjson_null(doc: *mut c_void) -> *mut c_void;
    fn ku_yyjson_make_bool(doc: *mut c_void, value: bool) -> *mut c_void;
    fn ku_yyjson_make_int(doc: *mut c_void, value: i64) -> *mut c_void;
    fn ku_yyjson_make_string(doc: *mut c_void, value: *const u8, length: usize) -> *mut c_void;
    fn ku_yyjson_make_number(doc: *mut c_void, value: *const u8, length: usize) -> *mut c_void;
    fn ku_yyjson_array(doc: *mut c_void) -> *mut c_void;
    fn ku_yyjson_object(doc: *mut c_void) -> *mut c_void;
    fn ku_yyjson_append(array: *mut c_void, value: *mut c_void) -> bool;
    fn ku_yyjson_add(object: *mut c_void, key: *mut c_void, value: *mut c_void) -> bool;
    fn ku_yyjson_write(
        doc: *mut c_void,
        root: *mut c_void,
        length: *mut usize,
        error: *mut Detail,
    ) -> *mut u8;
    fn ku_yyjson_text_free(text: *mut c_void);
}

#[derive(Debug)]
pub(super) struct Failure {
    pub code: u32,
    pub position: usize,
    pub message: String,
}
impl From<Detail> for Failure {
    fn from(detail: Detail) -> Self {
        let message = if detail.message.is_null() {
            "JSON allocation failed".into()
        } else {
            // yyjson error descriptions have static, NUL-terminated storage.
            unsafe { CStr::from_ptr(detail.message) }
                .to_string_lossy()
                .into_owned()
        };
        Self {
            code: detail.code,
            position: detail.position,
            message,
        }
    }
}

pub(super) struct Document(NonNull<c_void>);
impl Document {
    pub fn read(text: &str) -> Result<Self, Failure> {
        let mut detail = Detail::default();
        // No INSITU flag: yyjson copies input and never writes the Ku string.
        let doc = unsafe { ku_yyjson_read(text.as_ptr(), text.len(), &mut detail) };
        NonNull::new(doc).map(Self).ok_or_else(|| detail.into())
    }
    pub fn root(&self) -> Node<'_> {
        // A successfully read document owns a non-null root and all descendants.
        Node {
            raw: unsafe { ku_yyjson_root(self.0.as_ptr()) },
            owner: PhantomData,
        }
    }
}
impl Drop for Document {
    fn drop(&mut self) {
        unsafe { ku_yyjson_doc_free(self.0.as_ptr()) }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Node<'a> {
    raw: *mut c_void,
    owner: PhantomData<&'a Document>,
}
impl<'a> Node<'a> {
    pub fn kind(self) -> u32 {
        unsafe { ku_yyjson_kind(self.raw) }
    }
    pub fn boolean(self) -> bool {
        unsafe { ku_yyjson_bool(self.raw) }
    }
    pub fn integer(self) -> i64 {
        unsafe { ku_yyjson_int(self.raw) }
    }
    pub fn unsigned(self) -> u64 {
        unsafe { ku_yyjson_uint(self.raw) }
    }
    pub fn real(self) -> f64 {
        unsafe { ku_yyjson_real(self.raw) }
    }
    pub fn len(self) -> usize {
        unsafe { ku_yyjson_length(self.raw) }
    }
    pub fn text(self) -> &'a str {
        assert_eq!(self.kind(), 5, "string accessor requires a string node");
        // Only validated string nodes are passed here; yyjson validated UTF-8.
        let bytes = unsafe { std::slice::from_raw_parts(ku_yyjson_string(self.raw), self.len()) };
        std::str::from_utf8(bytes).expect("yyjson validated UTF-8")
    }
    pub fn raw_number(self) -> &'a str {
        assert_eq!(self.kind(), 8, "raw accessor requires a raw number node");
        let bytes = unsafe { std::slice::from_raw_parts(ku_yyjson_raw(self.raw), self.len()) };
        std::str::from_utf8(bytes).expect("yyjson validated numeric ASCII")
    }
    pub fn array_items(self) -> ArrayItems<'a> {
        assert_eq!(self.kind(), 6, "array iterator requires an array node");
        ArrayItems {
            current: self.first(),
            remaining: self.len(),
        }
    }
    pub fn object_fields(self) -> ObjectFields<'a> {
        assert_eq!(self.kind(), 7, "object iterator requires an object node");
        ObjectFields {
            current: self.first(),
            remaining: self.len(),
        }
    }
    fn first(self) -> Self {
        // For an empty container use the valid container pointer as a sentinel;
        // iterators never read it. Never expose an unchecked one-past node.
        Self {
            raw: if self.len() == 0 {
                self.raw
            } else {
                unsafe { ku_yyjson_first(self.raw) }
            },
            owner: PhantomData,
        }
    }
    fn following(self) -> Self {
        Self {
            raw: unsafe { ku_yyjson_next(self.raw) },
            owner: PhantomData,
        }
    }
}

pub(super) struct ArrayItems<'a> {
    current: Node<'a>,
    remaining: usize,
}
impl<'a> Iterator for ArrayItems<'a> {
    type Item = Node<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let value = self.current;
        self.remaining -= 1;
        if self.remaining != 0 {
            self.current = value.following();
        }
        Some(value)
    }
}
pub(super) struct ObjectFields<'a> {
    current: Node<'a>,
    remaining: usize,
}
impl<'a> Iterator for ObjectFields<'a> {
    type Item = (Node<'a>, Node<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let key = self.current;
        // Each yyjson object entry owns one validated key and one value node.
        let value = key.following();
        self.remaining -= 1;
        if self.remaining != 0 {
            self.current = value.following();
        }
        Some((key, value))
    }
}

pub(super) struct Builder(NonNull<c_void>);
// A detached DOM node is single-use: attachment consumes it, preventing cycles
// and reuse of yyjson's intrusive list links. The document outlives every node.
pub(super) struct Mutable<'a> {
    raw: NonNull<c_void>,
    document: NonNull<c_void>,
    owner: PhantomData<&'a Builder>,
}
impl Builder {
    pub fn new() -> Option<Self> {
        NonNull::new(unsafe { ku_yyjson_builder_new() }).map(Self)
    }
    fn node(&self, raw: *mut c_void) -> Option<Mutable<'_>> {
        NonNull::new(raw).map(|raw| Mutable {
            raw,
            document: self.0,
            owner: PhantomData,
        })
    }
    pub fn null(&self) -> Option<Mutable<'_>> {
        self.node(unsafe { ku_yyjson_null(self.0.as_ptr()) })
    }
    pub fn boolean(&self, value: bool) -> Option<Mutable<'_>> {
        self.node(unsafe { ku_yyjson_make_bool(self.0.as_ptr(), value) })
    }
    pub fn integer(&self, value: i64) -> Option<Mutable<'_>> {
        self.node(unsafe { ku_yyjson_make_int(self.0.as_ptr(), value) })
    }
    pub fn string(&self, value: &str) -> Option<Mutable<'_>> {
        self.node(unsafe { ku_yyjson_make_string(self.0.as_ptr(), value.as_ptr(), value.len()) })
    }
    pub fn number(&self, value: &str) -> Option<Mutable<'_>> {
        self.node(unsafe { ku_yyjson_make_number(self.0.as_ptr(), value.as_ptr(), value.len()) })
    }
    pub fn array(&self) -> Option<Mutable<'_>> {
        self.node(unsafe { ku_yyjson_array(self.0.as_ptr()) })
    }
    pub fn object(&self) -> Option<Mutable<'_>> {
        self.node(unsafe { ku_yyjson_object(self.0.as_ptr()) })
    }
    pub fn append(&self, array: &Mutable<'_>, value: Mutable<'_>) -> bool {
        array.document == self.0
            && value.document == self.0
            && unsafe { ku_yyjson_append(array.raw.as_ptr(), value.raw.as_ptr()) }
    }
    pub fn add(&self, object: &Mutable<'_>, key: Mutable<'_>, value: Mutable<'_>) -> bool {
        object.document == self.0
            && key.document == self.0
            && value.document == self.0
            && unsafe { ku_yyjson_add(object.raw.as_ptr(), key.raw.as_ptr(), value.raw.as_ptr()) }
    }
    pub fn write(&self, root: Mutable<'_>) -> Result<Text, Failure> {
        if root.document != self.0 {
            return Err(Failure {
                code: 1,
                position: 0,
                message: "JSON node belongs to another document".into(),
            });
        }
        let mut detail = Detail::default();
        let mut length = 0;
        let raw = unsafe {
            ku_yyjson_write(self.0.as_ptr(), root.raw.as_ptr(), &mut length, &mut detail)
        };
        NonNull::new(raw)
            .map(|raw| Text { raw, length })
            .ok_or_else(|| detail.into())
    }
}
impl Drop for Builder {
    fn drop(&mut self) {
        unsafe { ku_yyjson_builder_free(self.0.as_ptr()) }
    }
}
pub(super) struct Text {
    raw: NonNull<u8>,
    length: usize,
}
impl Text {
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.raw.as_ptr(), self.length) }
    }
}
impl Drop for Text {
    fn drop(&mut self) {
        unsafe { ku_yyjson_text_free(self.raw.as_ptr().cast()) }
    }
}
