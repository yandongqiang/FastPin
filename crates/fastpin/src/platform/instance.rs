use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Foundation::{GetLastError, SetLastError};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, WIN32_ERROR};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex};

use crate::error::{Error, Result};

/// Rebranded with the product: no migration for the mutex name. The only
/// window it opens is between an old and a new build launched together, and
/// the consequence is two tray icons, not lost data.
const NAME: &str = "Local\\FastPin.SingleInstance\0";

pub struct SingleInstance {
    handle: HANDLE,
}

impl SingleInstance {
    pub fn acquire() -> Result<Self> {
        unsafe {
            let mut wide: Vec<u16> = NAME.encode_utf16().collect();
            // CreateMutexW 只在「已存在」时把 last-error 置为
            // ERROR_ALREADY_EXISTS；调用前必须先清零，否则此前任意 Win32
            // 调用遗留的该错误码会让首个实例误判为「已在运行」。
            SetLastError(WIN32_ERROR(0));
            let handle = CreateMutexW(None, true, PCWSTR(wide.as_mut_ptr()))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                return Err(Error::new("already running"));
            }
            Ok(Self { handle })
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}
