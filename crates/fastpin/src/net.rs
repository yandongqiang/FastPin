//! Small WinHTTP helpers shared by translation and update checks.
//!
//! Both the response reader and the streaming downloader grow a `Vec` (or a
//! file) from `WinHttpQueryDataAvailable` with no ceiling unless a cap is
//! passed, so a broken or hostile endpoint could exhaust memory (fatal with
//! `panic = "abort"`). One capped reader and one capped streaming writer keep
//! every path identical.

use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use windows::Win32::Networking::WinHttp::{WinHttpQueryDataAvailable, WinHttpReadData};

use crate::error::{Error, Result};

/// Generous for a chat-completion or a GitHub release JSON, small enough that
/// a runaway endpoint cannot put the process into an unrecoverable allocation.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// 512 MiB ceiling for one streamed download (installer package).
pub const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Body is pulled in 64 KiB pieces so progress can be reported per chunk.
pub(crate) const DOWNLOAD_CHUNK_BYTES: u32 = 64 * 1024;

/// Streaming progress of one download. Re-exported by `update` for the settings
/// window's in-app installer progress display.
#[derive(Clone, Copy, Debug)]
pub struct DownloadProgress {
    pub received: u64,
    pub total: Option<u64>,
}

impl DownloadProgress {
    /// Whole percentage in `0..=100`, or `None` when the server did not send a
    /// usable `Content-Length`.
    pub fn percent(&self) -> Option<u32> {
        progress_percent(self.received, self.total)
    }
}

/// Reads the whole response body from a WinHTTP request handle, refusing to
/// allocate past `cap` bytes. Returns before the next read once the cap is hit.
///
/// Crate-private: the raw handle is only ever the live request owned by the
/// caller, so it must not be exposed as a safe public API.
pub(crate) fn read_response(request: *mut c_void, cap: usize) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    loop {
        let mut available = 0u32;
        unsafe {
            WinHttpQueryDataAvailable(request, &mut available).map_err(|e| winhttp_error(&e))?
        };
        if available == 0 {
            break;
        }
        let at = payload.len();
        let extra = available as usize;
        if extra > cap.saturating_sub(at) {
            return Err(Error::new(format!(
                "响应过大（超过 {}）",
                response_limit_label(cap)
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
            )
            .map_err(|e| winhttp_error(&e))?;
        }
        payload.truncate(at + read as usize);
        if read == 0 {
            break;
        }
    }
    Ok(payload)
}

/// 8 KiB 的校验文件上限不能显示成“超过 0 MiB”。
fn response_limit_label(cap: usize) -> String {
    if cap >= 1024 * 1024 {
        format!("{} MiB", cap / 1024 / 1024)
    } else {
        format!("{} KiB", cap / 1024)
    }
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

/// WinHTTP 的失败经 `error.rs` 只剩一个裸 HRESULT（例如 0x80072EE2），用户
/// 完全看不懂。把常见码翻成中文，其余保留原始码便于排查。
pub fn winhttp_error(e: &windows::core::Error) -> Error {
    // Win32 错误被包成 0x80070000 | code；低 16 位就是 WinHTTP 错误码。
    const BASE: u32 = 12000;
    const TIMEOUT: u32 = BASE + 2; // 12002
    const NAME_NOT_RESOLVED: u32 = BASE + 7; // 12007
    const CANNOT_CONNECT: u32 = BASE + 29; // 12029
    const CONNECTION_ERROR: u32 = BASE + 30; // 12030
    const CERT_DATE_INVALID: u32 = BASE + 37; // 12037
    const CERT_CN_INVALID: u32 = BASE + 38; // 12038
    const SECURE_FAILURE: u32 = BASE + 175; // 12175

    let code = e.code().0 as u32;
    let text = match code & 0x0000_FFFF {
        TIMEOUT => "连接超时，请检查网络或在设置里增大超时",
        NAME_NOT_RESOLVED => "无法解析服务器地址（DNS）",
        CANNOT_CONNECT => "无法连接到服务器",
        CONNECTION_ERROR => "连接中断或被重置",
        CERT_DATE_INVALID => "HTTPS 证书已过期或尚未生效",
        CERT_CN_INVALID => "HTTPS 证书域名不匹配",
        SECURE_FAILURE => "HTTPS 证书校验失败",
        _ => "网络请求失败",
    };
    Error::new(format!("{text}（0x{code:08X}）"))
}

/// API 返回的错误正文是服务端文本，偶尔会回显请求里的 Key 或带凭据的 URL。
/// 附加到用户可见的错误消息前先去敏，并截断到一屏以内。
pub fn redact_api_message(text: &str) -> String {
    /// 替换一个分词里的敏感串，但保留它前后的空白。
    fn replace(piece: &str, replacement: &str) -> String {
        let lead = piece.len() - piece.trim_start().len();
        let trail = piece.len() - piece.trim_end().len();
        format!(
            "{}{}{}",
            &piece[..lead],
            replacement,
            &piece[piece.len() - trail..]
        )
    }

    let mut out = String::new();
    let mut hide_next = false;
    for piece in text.split_inclusive(|c: char| c.is_whitespace()) {
        let core = piece.trim();
        if core.eq_ignore_ascii_case("Bearer") {
            hide_next = true;
            out.push_str(piece);
            continue;
        }
        if hide_next || core.starts_with("sk-") {
            out.push_str(&replace(piece, "***"));
            hide_next = false;
        } else if core.contains("://") && core.contains('@') {
            out.push_str(&replace(piece, &redact_url(core)));
        } else {
            out.push_str(piece);
        }
    }
    let out = out.trim().to_string();
    if out.chars().count() > 300 {
        let mut short: String = out.chars().take(300).collect();
        short.push('…');
        short
    } else {
        out
    }
}

/// 与 [`redact_api_message`] 相同，但先把本次请求实际使用的 Key 作为精确子串
/// 替换掉。网关可能在 error.message 里原样回显 `gsk_xxx` / `AIza...` /
/// `key=abcd` 之类的真实 Key——它们没有 Bearer/sk- 的外观，光靠词法规则
/// 挡不住，必须拿真 Key 比对。`secret` 为空时退化为纯词法去敏。
pub fn redact_api_message_with_secret(text: &str, secret: &str) -> String {
    let text = if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "***")
    };
    redact_api_message(&text)
}

/// `None` on u64 overflow or when adding `chunk` would pass `limit`.
pub(crate) fn checked_accumulate(received: u64, chunk: u64, limit: u64) -> Option<u64> {
    let next = received.checked_add(chunk)?;
    (next <= limit).then_some(next)
}

pub(crate) fn content_length_within_limit(total: Option<u64>, limit: u64) -> bool {
    total.is_none_or(|n| n <= limit)
}

pub(crate) fn progress_percent(received: u64, total: Option<u64>) -> Option<u32> {
    let total = total?;
    if total == 0 {
        return None;
    }
    Some((received.saturating_mul(100) / total).min(100) as u32)
}

/// 仅在服务端给出了 Content-Length 时校验完整性；分块传输（total 为 None）
/// 不校验，避免误判。调用方据此把截断的安装包当失败处理并删除临时文件。
fn ensure_complete(received: u64, total: Option<u64>) -> Result<()> {
    if let Some(expected) = total {
        if received != expected {
            return Err(Error::new(format!(
                "下载不完整：期望 {expected} 字节，实际 {received} 字节"
            )));
        }
    }
    Ok(())
}

/// Read the response body in chunks, writing straight to `path` and reporting
/// every chunk. Unlike [`read_response`] this never buffers the whole asset in
/// memory. Shared with `update::download_release_asset`.
pub(crate) fn stream_body_to_file(
    request: *mut c_void,
    path: &Path,
    total: Option<u64>,
    on_progress: &mut impl FnMut(DownloadProgress),
) -> Result<()> {
    let mut file = File::create(path)?;
    let mut buf = vec![0u8; DOWNLOAD_CHUNK_BYTES as usize];
    let mut received = 0u64;

    on_progress(DownloadProgress { received, total });

    loop {
        let mut available = 0u32;
        unsafe {
            WinHttpQueryDataAvailable(request, &mut available).map_err(|e| winhttp_error(&e))?
        };
        if available == 0 {
            break;
        }
        let to_read = available.min(DOWNLOAD_CHUNK_BYTES);
        let mut read = 0u32;
        unsafe {
            WinHttpReadData(request, buf.as_mut_ptr() as *mut c_void, to_read, &mut read)
                .map_err(|e| winhttp_error(&e))?;
        }
        if read == 0 {
            break;
        }
        received =
            checked_accumulate(received, read as u64, MAX_DOWNLOAD_BYTES).ok_or_else(|| {
                Error::new(format!(
                    "下载内容过大（超过 {} MiB）",
                    MAX_DOWNLOAD_BYTES / 1024 / 1024
                ))
            })?;
        file.write_all(&buf[..read as usize])?;
        on_progress(DownloadProgress { received, total });
    }
    file.flush()?;
    // 有 Content-Length 却提前结束（连接被截断）时不能报成功，否则
    // 设置页会把半截安装包当成下载完成交付给用户。
    ensure_complete(received, total)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_limit_label_uses_kib_for_small_caps() {
        assert_eq!(response_limit_label(8 * 1024), "8 KiB");
        assert_eq!(response_limit_label(8 * 1024 * 1024), "8 MiB");
    }

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

    #[test]
    fn winhttp_errors_become_readable_chinese() {
        // 12002 / 12029 / 12175 是超时、连不上、证书失败的常见码。
        let timeout = windows::core::Error::from_hresult(windows::core::HRESULT(
            (0x8007_0000u32 | 12002) as i32,
        ));
        assert!(winhttp_error(&timeout).to_string().contains("超时"));
        let connect = windows::core::Error::from_hresult(windows::core::HRESULT(
            (0x8007_0000u32 | 12029) as i32,
        ));
        assert!(winhttp_error(&connect).to_string().contains("无法连接"));
        let cert = windows::core::Error::from_hresult(windows::core::HRESULT(
            (0x8007_0000u32 | 12175) as i32,
        ));
        assert!(winhttp_error(&cert).to_string().contains("证书"));
        // 未知码仍保留原始 HRESULT，便于排查。
        let other = windows::core::Error::from_hresult(windows::core::HRESULT(-2147467259));
        assert!(winhttp_error(&other).to_string().contains("0x"));
    }

    #[test]
    fn api_error_text_is_redacted_and_truncated() {
        let msg = "bad key sk-abcdefghijklmnopqrstuvwxyz Bearer tok-1234567890 @ https://u:p@api.example/v1";
        let redacted = redact_api_message(msg);
        assert!(!redacted.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(!redacted.contains("tok-1234567890"));
        assert!(!redacted.contains("u:p@"));
        assert!(redacted.contains("***"));
        // 超长正文被截断到一屏以内。
        let long = "x".repeat(1000);
        let short = redact_api_message(&long);
        assert!(short.chars().count() <= 301);
        assert!(short.ends_with('…'));
    }

    #[test]
    fn echoed_secret_is_redacted_exactly() {
        let msg = "Invalid API key: gsk_secret123 (key=gsk_secret123)";
        let redacted = redact_api_message_with_secret(msg, "gsk_secret123");
        assert!(!redacted.contains("gsk_secret123"));
        assert!(redacted.contains("***"));
        // 空 secret 保持原来的词法去敏行为。
        assert_eq!(
            redact_api_message_with_secret("sk-abcdefghijklmnop", ""),
            "***",
            "existing sk- rule still applies"
        );
    }

    #[test]
    fn download_completeness_only_checked_when_total_known() {
        assert!(ensure_complete(100, Some(100)).is_ok());
        // 分块传输没有 Content-Length：不管收到多少都不误判。
        assert!(ensure_complete(100, None).is_ok());
        assert!(ensure_complete(0, None).is_ok());

        let short = ensure_complete(90, Some(100)).unwrap_err();
        let msg = short.to_string();
        assert!(msg.contains("下载不完整"), "{msg}");
        assert!(msg.contains("100"), "{msg}");
        assert!(msg.contains("90"), "{msg}");

        // 收到比声明的更多同样视为异常。
        assert!(ensure_complete(110, Some(100))
            .unwrap_err()
            .to_string()
            .contains("下载不完整"));
    }
}
