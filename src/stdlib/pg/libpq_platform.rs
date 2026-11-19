use std::ffi::{c_int, c_void, CStr, OsStr};
use std::io;
use std::path::Path;
use std::ptr::NonNull;

pub(super) struct Library(NonNull<c_void>);

// The OS loader serializes access to the handle. The immutable symbol table
// keeps the library alive for every connection that can call those symbols.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

impl Library {
    pub(super) fn find() -> Option<Self> {
        let configured = std::env::var_os("KU_PG_RUNTIME_DIR");
        #[cfg(unix)]
        let configured = configured.or_else(|| std::env::var_os("KU_PG_LIB"));
        if let Some(path) = configured.as_deref() {
            return Self::from_directory(Path::new(path));
        }

        default_library_paths()
            .iter()
            .find_map(|path| Self::open(OsStr::new(path)))
    }

    fn from_directory(dir: &Path) -> Option<Self> {
        if !dir.is_absolute() {
            return None;
        }
        library_names()
            .iter()
            .map(|name| dir.join(name))
            .filter(|file| file.is_file())
            .find_map(|file| Self::open(file.as_os_str()))
    }

    #[cfg(unix)]
    fn open(file: &OsStr) -> Option<Self> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let file = CString::new(file.as_bytes()).ok()?;
        // `file` remains a live C string; the loader makes its own reference.
        NonNull::new(unsafe { libc::dlopen(file.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) })
            .map(Self)
    }

    #[cfg(windows)]
    fn open(file: &OsStr) -> Option<Self> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::LibraryLoader::{
            LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        };

        let mut path: Vec<u16> = file.encode_wide().collect();
        if path.contains(&0) {
            return None;
        }
        path.push(0);
        let mut flags = LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;
        if Path::new(file).is_absolute() {
            flags |= LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR;
        }
        // `path` is terminated and remains live for the call. Library
        // dependencies are resolved by the configured loader search flags.
        NonNull::new(unsafe { LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), flags) })
            .map(Self)
    }

    #[cfg(unix)]
    pub(super) fn symbol(&self, name: &CStr) -> Option<NonNull<c_void>> {
        // Both the owning loader reference and the C symbol name are live.
        NonNull::new(unsafe { libc::dlsym(self.0.as_ptr(), name.as_ptr()) })
    }

    #[cfg(windows)]
    pub(super) fn symbol(&self, name: &CStr) -> Option<NonNull<c_void>> {
        use windows_sys::Win32::System::LibraryLoader::GetProcAddress;

        // Both the owning loader reference and the C symbol name are live.
        let symbol = unsafe { GetProcAddress(self.0.as_ptr(), name.as_ptr().cast()) }?;
        NonNull::new(symbol as *mut c_void)
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // This is the one reference acquired when loading the library.
        #[cfg(unix)]
        unsafe {
            libc::dlclose(self.0.as_ptr())
        };
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::FreeLibrary(self.0.as_ptr())
        };
    }
}

fn library_names() -> &'static [&'static str] {
    #[cfg(windows)]
    return &["libpq.dll"];
    #[cfg(target_os = "macos")]
    return &["libpq.5.dylib", "libpq.dylib"];
    #[cfg(all(unix, not(target_os = "macos")))]
    return &["libpq.so.5", "libpq.so"];
}

fn default_library_paths() -> &'static [&'static str] {
    // A bare dylib name can resolve relative to the current directory on
    // macOS. Use absolute system locations or the explicitly configured dir.
    #[cfg(target_os = "macos")]
    return &[
        "/usr/lib/libpq.5.dylib",
        "/opt/homebrew/lib/libpq.5.dylib",
        "/usr/local/lib/libpq.5.dylib",
    ];
    #[cfg(not(target_os = "macos"))]
    return library_names();
}

#[cfg(unix)]
pub(super) use libc::{POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT};
#[cfg(windows)]
pub(super) use windows_sys::Win32::Networking::WinSock::{
    POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT,
};

#[cfg(unix)]
pub(super) fn poll(socket: c_int, events: i16, timeout_ms: c_int) -> io::Result<i16> {
    let mut fd = libc::pollfd {
        fd: socket,
        events,
        revents: 0,
    };
    // libpq retains the socket throughout this call; only one fd is supplied.
    if unsafe { libc::poll(&mut fd, 1, timeout_ms) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd.revents)
}

#[cfg(windows)]
pub(super) fn poll(socket: c_int, events: i16, timeout_ms: c_int) -> io::Result<i16> {
    use windows_sys::Win32::Networking::WinSock::{WSAGetLastError, WSAPoll, WSAEINTR, WSAPOLLFD};

    // libpq's public PQsocket API returns int, even with WinSock's SOCKET type.
    let mut fd = WSAPOLLFD {
        fd: socket as u32 as usize,
        events,
        revents: 0,
    };
    // libpq retains the socket throughout this call; only one fd is supplied.
    if unsafe { WSAPoll(&mut fd, 1, timeout_ms) } < 0 {
        let code = unsafe { WSAGetLastError() };
        return Err(if code == WSAEINTR {
            io::ErrorKind::Interrupted.into()
        } else {
            io::Error::from_raw_os_error(code)
        });
    }
    Ok(fd.revents)
}
