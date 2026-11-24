use std::fmt;

#[derive(Debug)]
pub struct Error(pub String);

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        // 保留 HRESULT：诊断时据此区分设备丢失 / E_INVALIDARG / 权限不足等。
        // 结构是 tuple，不能在 source() 里挂原始错误，因此把代码并入消息。
        // windows-rs 的 Display 通常已经带上了 "0x…"（HRESULT），重复追加会
        // 让文案变成 "… (0x80070057) (HRESULT 0x80070057)"，这里跳过。
        let msg = e.to_string();
        if msg.contains("0x") || msg.contains("HRESULT") {
            Self(msg)
        } else {
            let code = e.code();
            Self(format!("{} (HRESULT 0x{:08X})", msg, code.0 as u32))
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        let msg = e.to_string();
        match e.raw_os_error() {
            // 标准库的 Display 通常已经带上了 "(os error N)"，不要重复追加。
            Some(code) if !msg.contains(&format!("(os error {code})")) => {
                Self(format!("{msg} (os error {code})"))
            }
            _ => Self(msg),
        }
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_message_keeps_errno() {
        // EACCES = 5；即使包装成 crate::Error 也要能在日志里看到 errno。
        let err = Error::from(std::io::Error::from_raw_os_error(5));
        assert!(err.0.contains("os error 5"), "{}", err.0);
    }

    #[test]
    fn custom_io_error_message_is_unchanged() {
        let err = Error::from(std::io::Error::other("自定义错误"));
        assert_eq!(err.0, "自定义错误");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_error_message_keeps_hresult() {
        use windows::Win32::Foundation::E_INVALIDARG;
        let err = Error::from(windows::core::Error::from(E_INVALIDARG));
        assert!(err.0.contains("0x80070057"), "{}", err.0);
    }
}
