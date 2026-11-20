//! Small WinHTTP helpers shared by translation and update checks.
//!
//! Both paths used to grow a `Vec` from `WinHttpQueryDataAvailable` with no
//! ceiling, so a broken or hostile endpoint could exhaust memory (fatal with
//! `panic = "abort"`). One capped reader keeps the two paths identical.

use std::ffi::c_void;

use windows::Win32::Networking::WinHttp::{WinHttpQueryDataAvailable, WinHttpReadData};

use crate::error::{Error, Result};

/// Generous for a chat-completion or a GitHub release JSON, small enough that
/// a runaway endpoint cannot put the process into an unrecoverable allocation.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Reads the whole response body from a WinHTTP request handle, refusing to
/// allocate past `cap` bytes. Returns before the next read once the cap is hit.
///
/// Crate-private: the raw handle is only ever the live request owned by the
/// caller, so it must not be exposed as a safe public API.
pub(crate) fn read_response(request: *mut c_void, cap: usize) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    loop {
        let mut available = 0u32;
        unsafe { WinHttpQueryDataAvailable(request, &mut available)? };
        if available == 0 {
            break;
        }
        let at = payload.len();
        let extra = available as usize;
        if extra > cap.saturating_sub(at) {
            return Err(Error::new(format!(
                "响应过大（超过 {} MiB）",
                cap / (1024 * 1024)
            )));
        }
        payload.resize(at + extra, 0u8);
        let mut read = 0u32;
        unsafe {
            WinHttpReadData(
                request,
                payload.as_mut_ptr().add(at) as *mut c_void,
                available,
                &mut read,
            )?;
        }
        payload.truncate(at + read as usize);
        if read == 0 {
            break;
        }
    }
    Ok(payload)
}

/// Removes `user:password@` from a URL before it is echoed in an error message.
/// User-supplied Base URLs legitimately contain credentials in some gateways;
/// never let those leak into a log line or a status label.
pub fn redact_url(url: &str) -> String {
    let Some(pos) = url.find("://") else {
        return url.to_string();
    };
    let (scheme, rest) = url.split_at(pos + 3);
    let Some(at) = rest.find('@') else {
        return url.to_string();
    };
    // Only treat it as userinfo if an authority follows it.
    let authority = &rest[at + 1..];
    if authority.is_empty() || authority.starts_with('/') {
        return url.to_string();
    }
    format!("{scheme}***@{authority}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_userinfo_but_keeps_the_host() {
        assert_eq!(
            redact_url("https://user:secret@api.example.com/v1"),
            "https://***@api.example.com/v1"
        );
        assert_eq!(
            redact_url("https://api.example.com/v1"),
            "https://api.example.com/v1"
        );
        assert_eq!(redact_url("not a url"), "not a url");
    }
}
