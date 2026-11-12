use windows::core::PCWSTR;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::Foundation::ERROR_ALREADY_EXISTS;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
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
