//! The embedded WinDivert runtime: bytes, unpacking, and raw FFI.
//!
//! Both WinDivert files ship *inside* this executable and are unpacked to a
//! per-user directory on first run, so the release is a single downloadable
//! file with nothing for the user to install alongside it.
//!
//! That is only possible because the library is loaded at *runtime*. The
//! usual import-table linkage (`#[link(name = "WinDivert")]`, which is what
//! the `windivert-sys` crate emits) is resolved by the Windows loader before
//! `main` is entered — the process would fail to start long before reaching
//! the code that unpacks the DLL. Loading it by path with `LoadLibrary`
//! after unpacking is the whole trick, and it is why this module hand-declares
//! the four entry points the capture loop needs instead of taking the crate.

use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

use libloading::Library;
use windows::Win32::Foundation::{BOOL, HANDLE};

use crate::error::{CaptureError, DriverRejection, ERROR_FILE_NOT_FOUND};
use crate::install::{ensure_file, runtime_dir};

/// Version of the bundled WinDivert distribution. Also the name of the
/// directory it unpacks into, so bumping it is enough to avoid reusing an
/// older build's files.
pub const WINDIVERT_VERSION: &str = "2.2.2";

/// The official signed x64 release binaries, verbatim (see
/// `vendor/windivert/LICENSE`). The driver cannot be built from source here:
/// Windows only loads kernel drivers carrying a signature it trusts.
const WINDIVERT_DLL: &[u8] = include_bytes!("../vendor/windivert/WinDivert.dll");
const WINDIVERT_SYS: &[u8] = include_bytes!("../vendor/windivert/WinDivert64.sys");

const DLL_NAME: &str = "WinDivert.dll";
const SYS_NAME: &str = "WinDivert64.sys";

/// `WINDIVERT_LAYER_NETWORK`.
pub const LAYER_NETWORK: u32 = 0;
/// `WINDIVERT_FLAG_SNIFF` — observe packets without diverting them.
pub const FLAG_SNIFF: u64 = 0x0001;
/// `WINDIVERT_SHUTDOWN_RECV`.
pub const SHUTDOWN_RECV: u32 = 0x1;

/// `WINDIVERT_ADDRESS` from `windivert.h`: an 8-byte timestamp, 8 bytes of
/// bitfields and reserved words, then a 64-byte per-layer union.
///
/// Declared as opaque storage rather than field-by-field because the capture
/// loop never reads it — only the size and alignment have to be right, so the
/// driver's write stays in bounds.
#[repr(C, align(8))]
pub struct WinDivertAddress([u8; 80]);

// Undersizing this would let the driver write past the end of the caller's
// `MaybeUninit`: silent memory corruption rather than a crash, and not
// something a test could reliably catch. Pin it at compile time instead.
const _: () = {
    assert!(size_of::<WinDivertAddress>() == 80);
    assert!(align_of::<WinDivertAddress>() == 8);
};

type OpenFn = unsafe extern "system" fn(*const c_char, u32, i16, u64) -> HANDLE;
type RecvFn =
    unsafe extern "system" fn(HANDLE, *mut c_void, u32, *mut u32, *mut WinDivertAddress) -> BOOL;
type ShutdownFn = unsafe extern "system" fn(HANDLE, u32) -> BOOL;
type CloseFn = unsafe extern "system" fn(HANDLE) -> BOOL;

/// The unpacked, loaded WinDivert library and the entry points used here.
pub struct Api {
    open: OpenFn,
    recv: RecvFn,
    shutdown: ShutdownFn,
    close: CloseFn,
    /// Where the runtime was unpacked. Kept for the retry path and for
    /// diagnostics in the log.
    dir: PathBuf,
    /// The loaded library. Every function pointer above points into it, so it
    /// is held for as long as the `Api` lives — and the only `Api` lives in a
    /// process-lifetime static, so they can never dangle.
    _lib: Library,
}

/// Process-wide handle to the loaded runtime.
///
/// A single load is both sufficient and necessary: `LoadLibrary` refcounts,
/// but the driver handles handed out by `WinDivertOpen` outlive individual
/// calls here, so the library must never be unloaded while one is open.
static API: OnceLock<Api> = OnceLock::new();

/// Unpacks (if needed) and loads the bundled runtime, reusing the already
/// loaded one on subsequent calls.
pub fn api() -> Result<&'static Api, CaptureError> {
    if let Some(api) = API.get() {
        return Ok(api);
    }
    let loaded = Api::load()?;
    // A concurrent caller may have won the race; theirs is just as good, and
    // dropping ours only decrements the loader's refcount.
    Ok(API.get_or_init(|| loaded))
}

impl Api {
    fn load() -> Result<Self, CaptureError> {
        let dir = runtime_dir(&unpack_base(), WINDIVERT_VERSION);
        let dll_path = dir.join(DLL_NAME);
        let sys_path = dir.join(SYS_NAME);

        for (path, bytes) in [(&dll_path, WINDIVERT_DLL), (&sys_path, WINDIVERT_SYS)] {
            let wrote = ensure_file(path, bytes)
                .map_err(|err| CaptureError::RuntimeUnpack(format!("{}: {err}", path.display())))?;
            if wrote {
                log::info!("unpacked {}", path.display());
            }
        }

        // SAFETY: the path names a file this process just verified byte-for-byte
        // against the vendored WinDivert release, and loading it runs only that
        // library's initialisers.
        let lib = unsafe { Library::new(&dll_path) }
            .map_err(|err| CaptureError::RuntimeLoad(format!("{}: {err}", dll_path.display())))?;

        // SAFETY: each symbol exists in WinDivert.dll with the signature
        // declared above (checked against `windivert.h` for the vendored
        // version); the returned pointers are copied out and stay valid as
        // long as `lib`, which this struct owns.
        let (open, recv, shutdown, close) = unsafe {
            (
                *symbol::<OpenFn>(&lib, b"WinDivertOpen\0")?,
                *symbol::<RecvFn>(&lib, b"WinDivertRecv\0")?,
                *symbol::<ShutdownFn>(&lib, b"WinDivertShutdown\0")?,
                *symbol::<CloseFn>(&lib, b"WinDivertClose\0")?,
            )
        };

        Ok(Api {
            open,
            recv,
            shutdown,
            close,
            dir,
            _lib: lib,
        })
    }

    /// Opens a sniff-mode handle on `filter`.
    ///
    /// `WinDivertOpen` installs the kernel driver on demand, resolving
    /// `WinDivert64.sys` relative to the loaded `WinDivert.dll` — which is why
    /// both files are unpacked into the same directory. Should a build resolve
    /// it relative to the *executable* instead, the driver simply is not found;
    /// that one case is worth a second copy and a retry rather than a dead end.
    pub fn open_sniff(&self, filter: &CStr) -> Result<HANDLE, CaptureError> {
        match self.try_open(filter) {
            Ok(handle) => Ok(handle),
            Err(err) if err.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {
                log::warn!(
                    "WinDivert did not find {SYS_NAME} in {}; retrying beside the executable",
                    self.dir.display()
                );
                match self.install_sys_beside_exe() {
                    Ok(path) => {
                        log::info!("unpacked {}", path.display());
                        self.try_open(filter)
                            .map_err(|err| CaptureError::from_open_error(&err))
                    }
                    Err(err) => {
                        log::error!("could not unpack {SYS_NAME} beside the executable: {err}");
                        Err(CaptureError::DriverRejected(DriverRejection::SysNotFound))
                    }
                }
            }
            Err(err) => Err(CaptureError::from_open_error(&err)),
        }
    }

    fn try_open(&self, filter: &CStr) -> Result<HANDLE, std::io::Error> {
        // SAFETY: `filter` is a valid NUL-terminated C string that outlives
        // the call; the remaining arguments are plain values.
        let handle = unsafe { (self.open)(filter.as_ptr(), LAYER_NETWORK, 0, FLAG_SNIFF) };
        if handle.is_invalid() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(handle)
    }

    /// Blocking single-packet receive; see [`crate::win`] for the contract.
    ///
    /// # Safety
    /// `handle` must be a live handle from [`Self::open_sniff`].
    pub unsafe fn recv(
        &self,
        handle: HANDLE,
        buffer: &mut [u8],
        addr: *mut WinDivertAddress,
    ) -> Result<usize, std::io::Error> {
        let mut packet_len: u32 = 0;
        // SAFETY: delegated to the caller for `handle`; `buffer` is a valid
        // writable slice of `buffer.len()` bytes and `addr` a valid
        // out-parameter, both borrowed for the duration of the call.
        let ok = unsafe {
            (self.recv)(
                handle,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
                &mut packet_len,
                addr,
            )
        };
        if !ok.as_bool() {
            return Err(std::io::Error::last_os_error());
        }
        Ok((packet_len as usize).min(buffer.len()))
    }

    /// # Safety
    /// `handle` must be a live handle from [`Self::open_sniff`].
    pub unsafe fn shutdown_recv(&self, handle: HANDLE) {
        // SAFETY: delegated to the caller; `WinDivertShutdown` is documented
        // as callable concurrently with a blocked recv on the same handle.
        unsafe {
            (self.shutdown)(handle, SHUTDOWN_RECV);
        }
    }

    /// # Safety
    /// `handle` must be a live handle from [`Self::open_sniff`], unused by any
    /// other thread from here on.
    pub unsafe fn close(&self, handle: HANDLE) {
        // SAFETY: delegated to the caller.
        unsafe {
            (self.close)(handle);
        }
    }

    /// Writes the driver next to the running executable, for the case where
    /// WinDivert looks for it there. Returns where it landed.
    fn install_sys_beside_exe(&self) -> std::io::Result<PathBuf> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "executable has no parent directory",
            )
        })?;
        let path = dir.join(SYS_NAME);
        ensure_file(&path, WINDIVERT_SYS)?;
        Ok(path)
    }
}

/// Copies a function pointer out of `lib`, mapping a missing export to a
/// load failure rather than a panic.
///
/// # Safety
/// `T` must be the symbol's actual signature.
unsafe fn symbol<'lib, T>(
    lib: &'lib Library,
    name: &[u8],
) -> Result<libloading::Symbol<'lib, T>, CaptureError> {
    // SAFETY: delegated to the caller.
    unsafe { lib.get(name) }.map_err(|err| {
        CaptureError::RuntimeLoad(format!(
            "{}: {err}",
            String::from_utf8_lossy(&name[..name.len() - 1])
        ))
    })
}

/// Root the runtime is unpacked under: the user's local app data, falling
/// back to the temp directory on the odd system where `LOCALAPPDATA` is unset.
fn unpack_base() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A truncated or line-ending-mangled payload would produce a driver
    /// Windows refuses to load, with no signal until runtime.
    #[test]
    fn bundled_binaries_are_the_vendored_release() {
        assert_eq!(WINDIVERT_DLL.len(), 47616);
        assert_eq!(WINDIVERT_SYS.len(), 94144);
        assert_eq!(&WINDIVERT_DLL[..2], b"MZ");
        assert_eq!(&WINDIVERT_SYS[..2], b"MZ");
    }
}
