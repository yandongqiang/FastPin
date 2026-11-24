//! Small WinHTTP helpers shared by translation and update checks.
//!
//! Both the response reader and the streaming downloader grow a `Vec` (or a
//! file) from `WinHttpQueryDataAvailable` with no ceiling unless a cap is
//! passed, so a broken or hostile endpoint could exhaust memory (fatal with
//! `panic = "abort"`). One capped reader and one capped streaming writer keep
//! every path identical.

use std::ffi::c_void;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts, URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_FLAG_SECURE, WINHTTP_INTERNET_SCHEME_HTTPS, WINHTTP_QUERY_CONTENT_LENGTH,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE,
};

use crate::error::{Error, Result};

/// Generous for a chat-completion or a GitHub release JSON, small enough that
/// a runaway endpoint cannot put the process into an unrecoverable allocation.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// 512 MiB ceiling for one streamed download (installer package).
pub const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Body is pulled in 64 KiB pieces so progress can be reported per chunk.
pub(crate) const DOWNLOAD_CHUNK_BYTES: u32 = 64 * 1024;

/// 一次下载的整体超时（连接 / 发送 / 接收 / 读取共用一个预算）。
const DOWNLOAD_TIMEOUT_SECONDS: u32 = 20;

/// 跟随下载托管站常见的 CDN 跳转；超过就放弃，避免无限循环。
const MAX_DOWNLOAD_REDIRECTS: usize = 5;

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

/// File name for a downloaded asset: the URL's last path segment, with
/// characters Windows refuses in a file name replaced. Pure so callers can use
/// it without touching the network.
pub fn download_file_name(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    // Strip the scheme and host: a bare `https://host/` has no path segment at
    // all and must fall back, while `https://host/a/b/` should still yield "b".
    let path = without_query
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, p)| p))
        .unwrap_or(without_query);
    let last = path
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("download.bin");
    let cleaned: String = last
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "download.bin".into()
    } else {
        trimmed.to_string()
    }
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
    Ok(())
}

/// Closes on drop, so every early return still releases the handle.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// Keeps the session and connect handles alive while the request is read.
struct GetHandles {
    _session: Handle,
    _connect: Handle,
    request: Handle,
    status: u32,
    total: Option<u64>,
    location: Option<String>,
}

fn nul_terminated(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn nul_terminated_from(s: &[u16]) -> Vec<u16> {
    s.iter().copied().chain([0]).collect()
}

unsafe fn slice_of(p: PWSTR, len: u32) -> Vec<u16> {
    if p.is_null() || len == 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(p.0, len as usize) }.to_vec()
}

unsafe fn query_u32_header(request: *mut c_void, info: u32) -> Option<u32> {
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            request,
            info | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut value as *mut u32 as *mut c_void),
            &mut size,
            std::ptr::null_mut(),
        )
    };
    ok.ok().map(|()| value)
}

unsafe fn query_string_header(request: *mut c_void, info: u32) -> Option<String> {
    let mut buf = vec![0u16; 4096];
    let mut size = (buf.len() * 2) as u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            request,
            info,
            PCWSTR::null(),
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut size,
            std::ptr::null_mut(),
        )
    };
    if ok.is_err() {
        return None;
    }
    let len = (size as usize / 2).min(buf.len());
    let end = buf[..len].iter().position(|&c| c == 0).unwrap_or(len);
    Some(String::from_utf16_lossy(&buf[..end]))
}

fn begin_get(url: &str) -> Result<GetHandles> {
    let wide = nul_terminated(url);
    let mut parts = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        dwSchemeLength: u32::MAX,
        dwHostNameLength: u32::MAX,
        dwUrlPathLength: u32::MAX,
        dwExtraInfoLength: u32::MAX,
        ..Default::default()
    };
    unsafe { WinHttpCrackUrl(&wide, 0, &mut parts) }
        .map_err(|_| Error::new(format!("下载地址无效：{}", redact_url(url))))?;
    let host = unsafe { slice_of(parts.lpszHostName, parts.dwHostNameLength) };
    let mut path = unsafe { slice_of(parts.lpszUrlPath, parts.dwUrlPathLength) };
    path.extend_from_slice(&unsafe { slice_of(parts.lpszExtraInfo, parts.dwExtraInfoLength) });
    let secure = parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS;

    unsafe {
        let session = Handle(WinHttpOpen(
            PCWSTR(nul_terminated("FastPin").as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(Error::new("WinHttpOpen failed"));
        }
        let ms = (DOWNLOAD_TIMEOUT_SECONDS * 1000) as i32;
        WinHttpSetTimeouts(session.0, ms, ms, ms, ms).map_err(|e| winhttp_error(&e))?;

        let connect = Handle(WinHttpConnect(
            session.0,
            PCWSTR(nul_terminated_from(&host).as_ptr()),
            parts.nPort,
            0,
        ));
        if connect.0.is_null() {
            return Err(Error::new("WinHttpConnect failed"));
        }
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            PCWSTR(nul_terminated("GET").as_ptr()),
            PCWSTR(nul_terminated_from(&path).as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            if secure {
                WINHTTP_FLAG_SECURE
            } else {
                Default::default()
            },
        ));
        if request.0.is_null() {
            return Err(Error::new("WinHttpOpenRequest failed"));
        }

        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|e| winhttp_error(&e))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| winhttp_error(&e))?;

        let status = query_u32_header(request.0, WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        let total = query_u32_header(request.0, WINHTTP_QUERY_CONTENT_LENGTH).map(u64::from);
        let location = query_string_header(request.0, WINHTTP_QUERY_LOCATION);
        Ok(GetHandles {
            _session: session,
            _connect: connect,
            request,
            status,
            total,
            location,
        })
    }
}

fn is_redirect(status: u32) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Download hosts redirect to a CDN. 与 `update.rs` 的发布包策略保持一致：
/// 重定向只允许 HTTPS，且目标主机必须在 GitHub / GitHub 资源 CDN 白名单内；
/// 根相对路径沿用当前 URL 的主机。拒绝 `http://` 降级、`file://` 和本地路径。
///
/// 注意：`download_url_to` 目前没有业务调用方；若将来接入新的下载源，必须
/// 先把它的主机加进下面的白名单，不能靠放宽这里的校验来「修」下载失败。
fn resolve_download_redirect(base: &str, location: &str) -> Result<String> {
    let loc = location.trim();
    if loc.is_empty() {
        return Err(Error::new("下载重定向地址为空"));
    }
    let lower = loc.to_ascii_lowercase();
    if lower.starts_with("http://") {
        return Err(Error::new("下载重定向只允许 HTTPS"));
    }
    if let Some(stripped) = lower.strip_prefix("https://") {
        if !trusted_download_authority(stripped) {
            return Err(Error::new("下载重定向主机不在白名单内"));
        }
        return Ok(loc.to_string());
    }
    if let Some(path) = loc.strip_prefix('/') {
        let (scheme, rest) = base
            .split_once("://")
            .ok_or_else(|| Error::new("下载地址无效"))?;
        if scheme != "https" {
            return Err(Error::new("下载重定向只允许 HTTPS"));
        }
        let authority = rest
            .split(['/', '?', '#'])
            .next()
            .filter(|host| !host.is_empty() && !host.contains('@'))
            .ok_or_else(|| Error::new("下载重定向地址不可用"))?;
        if !trusted_download_authority(authority) {
            return Err(Error::new("下载重定向主机不在白名单内"));
        }
        return Ok(format!("https://{authority}/{path}"));
    }
    Err(Error::new("下载重定向地址不可用"))
}

/// GitHub 及其资源 CDN 之外的主机一律不接受，规则与 `update.rs` 相同：
/// 拒绝 userinfo 技巧（`github.com@evil.example/`），并忽略可选的端口。
fn trusted_download_authority(authority_and_path: &str) -> bool {
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let host = authority
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(authority);
    host == "github.com" || host.ends_with(".githubusercontent.com")
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// `.part` sits beside the destination so the final `rename` is atomic on the
/// same volume. Unique name keeps concurrent downloads from clobbering each
/// other.
fn part_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download.bin");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    dest.with_file_name(format!(".{name}.{}-{stamp}-{seq}.part", std::process::id()))
}

/// Stream one http(s) URL into `dest` (a file path chosen by the caller), with a
/// 512 MiB ceiling and per-chunk progress. Bytes land in a sibling `.part` file
/// first and are renamed into place only on success, so an interrupted download
/// never leaves a half-written file at the destination.
///
/// Blocking. Call it off the UI thread. `on_progress` fires once before the
/// first read and once after every chunk.
pub fn download_url_to(
    url: &str,
    dest: &Path,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<PathBuf> {
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err(Error::new("下载地址无效：仅支持 http/https"));
    }

    let mut current = url.trim().to_string();
    let mut head = begin_get(&current)?;
    let mut hops = 0usize;
    while is_redirect(head.status) {
        let location = head
            .location
            .take()
            .ok_or_else(|| Error::new("下载重定向缺少地址"))?;
        current = resolve_download_redirect(&current, &location)?;
        hops += 1;
        if hops > MAX_DOWNLOAD_REDIRECTS {
            return Err(Error::new("下载重定向次数过多"));
        }
        head = begin_get(&current)?;
    }

    if head.status == 404 {
        return Err(Error::new("下载文件不存在（HTTP 404）"));
    }
    if !(200..300).contains(&head.status) {
        return Err(Error::new(format!("下载失败（HTTP {}）", head.status)));
    }
    if !content_length_within_limit(head.total, MAX_DOWNLOAD_BYTES) {
        return Err(Error::new(format!(
            "下载内容过大（超过 {} MiB）",
            MAX_DOWNLOAD_BYTES / 1024 / 1024
        )));
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let part = part_path(dest);
    match stream_body_to_file(head.request.0, &part, head.total, &mut on_progress) {
        Ok(()) => {
            if let Err(e) = fs::rename(&part, dest) {
                let _ = fs::remove_file(&part);
                return Err(e.into());
            }
            Ok(dest.to_path_buf())
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
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

    #[test]
    fn download_file_name_uses_the_url_leaf_and_is_windows_safe() {
        assert_eq!(
            download_file_name("https://example.com/models/haidass.onnx?token=1"),
            "haidass.onnx"
        );
        assert_eq!(download_file_name("https://example.com/a/b/"), "b");
        assert_eq!(download_file_name("https://example.com/a/..\\b"), ".._b");
        assert_eq!(download_file_name("https://example.com/"), "download.bin");
        assert_eq!(download_file_name("not a url"), "not a url");
    }

    #[test]
    fn redirects_accept_only_https_and_whitelisted_hosts() {
        let base = "https://github.com/owner/repo/releases/a.bin";
        assert_eq!(
            resolve_download_redirect(base, "https://objects.githubusercontent.com/x.bin").unwrap(),
            "https://objects.githubusercontent.com/x.bin"
        );
        assert_eq!(
            resolve_download_redirect(base, "/other/x.bin").unwrap(),
            "https://github.com/other/x.bin"
        );
        // 不允许明文降级，也不允许跳到白名单外的主机（含 userinfo 伪装）。
        assert!(resolve_download_redirect(base, "http://github.com/x.bin").is_err());
        assert!(resolve_download_redirect(base, "https://evil.example/x.bin").is_err());
        assert!(resolve_download_redirect(base, "https://github.com@evil.example/x").is_err());
        assert!(resolve_download_redirect(base, "file:///C:/Windows/System32/cmd.exe").is_err());
        assert!(resolve_download_redirect(base, "  ").is_err());
        // 无网络调用：无效 URL 在 WinHTTP 之前就被拒绝。
        assert!(download_url_to("file:///C:/x", Path::new("x"), |_| {}).is_err());
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
}
