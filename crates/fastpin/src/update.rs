//! Update check against GitHub Releases.
//!
//! Same WinHTTP choice as `translate`: one GET does not justify an HTTP crate.
//! The release source is the compile-time `GITHUB_REPO` constant below.

use std::ffi::c_void;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_CANCELLED;
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_INTERNET_SCHEME_HTTPS, WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::error::{Error, Result};
// 下载进度与流式写入由 `net` 统一实现，更新包下载复用这套上限与写入器。
pub use crate::net::DownloadProgress;
use crate::net::{content_length_within_limit, stream_body_to_file, MAX_DOWNLOAD_BYTES};

/// `owner/repo` of the GitHub project that publishes releases.
const GITHUB_REPO: &str = "yandongqiang/FastPin";

/// Generous bound for one tiny JSON document on a flaky connection.
const TIMEOUT_SECONDS: u32 = 15;

/// Release-side checksum manifest. `release.yml` publishes it next to the
/// installer; the downloader refuses to hand an unverified exe to the user.
const CHECKSUMS_FILE_NAME: &str = "SHA256SUMS.txt";
/// The manifest is a few lines of ASCII, not a download-sized blob.
const MAX_CHECKSUM_BYTES: u64 = 8 * 1024;

pub struct Release {
    pub version: String,
    pub url: String,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// True when `latest` sorts after `current`. SemVer 风格比较：先逐段比较
/// 数字主版本（支持三/四段，缺失段按 0 补齐），主版本相同则正式版新于
/// 预发布版，两边都是预发布时按 `.` 分段、数字段小于字符串段逐段比较。
pub fn is_newer(latest: &str, current: &str) -> bool {
    compare_versions(latest, current) == std::cmp::Ordering::Greater
}

struct ParsedVersion {
    core: Vec<u64>,
    pre: Option<String>,
}

impl ParsedVersion {
    fn parse(v: &str) -> Self {
        let v = v.trim().trim_start_matches(['v', 'V']);
        // `+` 之后是构建元数据，semver 中不参与优先级比较。
        let core_and_pre = v.split_once('+').map_or(v, |(left, _)| left);
        // `-` 之后是预发布标识；没有 `-` 就是正式版。
        let (core, pre) = match core_and_pre.split_once('-') {
            Some((core, pre)) => (core, Some(pre.to_string())),
            None => (core_and_pre, None),
        };
        let core = core
            .split('.')
            .map(|piece| piece.parse::<u64>().unwrap_or(0))
            .collect();
        Self { core, pre }
    }
}

fn compare_versions(latest: &str, current: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering as Cmp;
    let latest = ParsedVersion::parse(latest);
    let current = ParsedVersion::parse(current);

    // 逐段比较；段数不同时短的补 0，所以 0.3.0.1 > 0.3.0。
    let segments = latest.core.len().max(current.core.len());
    for i in 0..segments {
        let a = latest.core.get(i).copied().unwrap_or(0);
        let b = current.core.get(i).copied().unwrap_or(0);
        if a != b {
            return a.cmp(&b);
        }
    }

    // 主版本相同：正式版 > 预发布版（0.3.0 > 0.3.0-beta.2）。
    match (&latest.pre, &current.pre) {
        (None, None) => Cmp::Equal,
        (None, Some(_)) => Cmp::Greater,
        (Some(_), None) => Cmp::Less,
        (Some(a), Some(b)) => compare_prerelease(a, b),
    }
}

/// 预发布比较：按 `.` 分段，数字段按数值比且小于非数字段，
/// 非数字段按字典序比；前缀都相同则段数多者更新。
fn compare_prerelease(latest: &str, current: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering as Cmp;
    let mut a = latest.split('.');
    let mut b = current.split('.');
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Cmp::Equal,
            (None, Some(_)) => return Cmp::Less,
            (Some(_), None) => return Cmp::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Cmp::Less,
                    (Err(_), Ok(_)) => Cmp::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if ord != Cmp::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Blocking. Call it off the UI thread.
pub fn check_latest() -> Result<Release> {
    let body = get(&format!(
        "https://api.github.com/repos/{GITHUB_REPO}/releases/latest"
    ))?;
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| Error::new("发布信息不是有效的 JSON"))?;
    let tag = json["tag_name"].as_str().unwrap_or_default();
    let url = json["html_url"].as_str().unwrap_or_default();
    if tag.is_empty() || url.is_empty() {
        return Err(Error::new("发布信息缺少 tag_name 或 html_url"));
    }
    // The URL came from the network; never hand an unvalidated scheme/host to
    // ShellExecuteW (file:// and local paths would launch arbitrary programs).
    if !trusted_release_url(url) {
        return Err(Error::new("发布页地址不可信，已拒绝打开"));
    }
    Ok(Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        url: url.to_string(),
    })
}

/// Only `https://github.com/...` release pages may be opened. Rejects
/// userinfo tricks (`https://github.com@evil.com/`) as well as other schemes.
fn trusted_release_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(authority_and_path) = lower.strip_prefix("https://") else {
        return false;
    };
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = authority
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(authority);
    host == "github.com"
}

pub fn open_release_page(url: &str) -> Result<()> {
    if !trusted_release_url(url) {
        return Err(Error::new("发布页地址不可信，已拒绝打开"));
    }
    let verb: Vec<u16> = "open".encode_utf16().chain([0]).collect();
    let target: Vec<u16> = url.encode_utf16().chain([0]).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a fake HINSTANCE; <= 32 is the error range.
    if result.0 as usize <= 32 {
        return Err(Error::new("无法打开发布页面"));
    }
    Ok(())
}

/// GET the whole body, status-checked. Mirrors `translate::post_json` minus
/// the request body and auth header; GitHub only insists on a User-Agent,
/// which `WinHttpOpen`'s agent string already supplies.
fn get(url: &str) -> Result<String> {
    let wide: Vec<u16> = url.encode_utf16().chain([0]).collect();
    let mut parts = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        // u32::MAX asks WinHttp for pointers into `wide` instead of copies.
        dwSchemeLength: u32::MAX,
        dwHostNameLength: u32::MAX,
        dwUrlPathLength: u32::MAX,
        dwExtraInfoLength: u32::MAX,
        ..Default::default()
    };
    unsafe { WinHttpCrackUrl(&wide, 0, &mut parts) }
        .map_err(|_| Error::new(format!("更新地址无效：{}", crate::net::redact_url(url))))?;

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
            // 本地网络栈初始化失败，不是服务端问题：给出可操作的方向。
            return Err(Error::new("网络初始化失败，请检查网络或代理设置后重试"));
        }
        let ms = (TIMEOUT_SECONDS * 1000) as i32;
        WinHttpSetTimeouts(session.0, ms, ms, ms, ms).map_err(|e| crate::net::winhttp_error(&e))?;

        let connect = Handle(WinHttpConnect(
            session.0,
            PCWSTR(nul_terminated_from(&host).as_ptr()),
            parts.nPort,
            0,
        ));
        if connect.0.is_null() {
            // 连不上更新服务器，多为代理或 DNS 问题，提示稍后重试。
            return Err(Error::new("无法连接到更新服务器，请稍后重试"));
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
            // 会话与连接都有效却建不出请求，按瞬时故障提示重试。
            return Err(Error::new("无法创建更新请求，请稍后重试"));
        }

        WinHttpSendRequest(request.0, None, None, 0, 0, 0)
            .map_err(|e| crate::net::winhttp_error(&e))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|e| crate::net::winhttp_error(&e))?;

        let mut status = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|e| crate::net::winhttp_error(&e))?;

        let payload = crate::net::read_response(request.0, crate::net::MAX_RESPONSE_BYTES)?;

        if !(200..300).contains(&status) {
            return Err(Error::new(format!("检查更新失败（HTTP {status}）")));
        }
        String::from_utf8(payload).map_err(|_| Error::new("发布信息不是有效的 UTF-8"))
    }
}

/// GitHub asset URLs redirect to a CDN; allow a few hops, then give up.
const MAX_DOWNLOAD_REDIRECTS: usize = 5;

/// Strip surrounding whitespace and one leading `v`/`V` from a release tag.
fn normalize_version(version: &str) -> String {
    version.trim().trim_start_matches(['v', 'V']).to_string()
}

/// Tags are embedded both in a URL path and in a file name, so reject anything
/// that is not a plain ASCII version before it reaches either.
fn valid_version(version: &str) -> bool {
    let v = normalize_version(version);
    !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

/// Upload name from `.github/workflows/release.yml`: the installer is
/// `FastPin-v<tag>-x64-setup.exe`, where `<tag>` is e.g. `v0.3.0`.
fn asset_file_name(version: &str) -> String {
    format!("FastPin-v{}-x64-setup.exe", normalize_version(version))
}

/// Every download -- installer and checksum manifest alike -- starts from the
/// same trusted `github.com` release path; redirects are validated separately.
fn release_asset_url(version: &str, file_name: &str) -> String {
    let v = normalize_version(version);
    format!("https://github.com/{GITHUB_REPO}/releases/download/v{v}/{file_name}")
}

/// 下载完成后的安装包名：`seq == 1` 用发布资产原名，撞名则追加 `-2`、`-3`…。
/// 不覆盖旧文件：上一次下载的安装包可能正在运行，覆盖会失败或破坏它。
fn installer_candidate_name(version: &str, seq: u32) -> String {
    let file = asset_file_name(version);
    if seq == 1 {
        return file;
    }
    let stem = file.strip_suffix(".exe").unwrap_or(&file);
    format!("{stem}-{seq}.exe")
}

/// 把 `.part` 原子改名为最终安装包名。Windows 上目标已存在时 `rename` 会失败，
/// 所以逐个候选尝试；只有确认是「目标已存在」才换下一个名字。
fn finalize_download(part: &Path, dir: &Path, version: &str) -> Result<PathBuf> {
    for seq in 1..=999u32 {
        let candidate = dir.join(installer_candidate_name(version, seq));
        // 有些平台上 rename 会直接覆盖目标，所以先主动跳过已存在的名字。
        if candidate.exists() {
            continue;
        }
        match fs::rename(part, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(_) if candidate.exists() => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::new("安装包命名冲突过多，无法保存"))
}

/// Resolve the GitHub asset URL through the same host-checked redirect chain as
/// the installer download. Status is left to the caller so each asset can give
/// its own actionable error (a missing checksum file is not a missing exe).
fn open_release_asset(version: &str, file_name: &str) -> Result<(DownloadSession, ResponseHead)> {
    let mut current = release_asset_url(version, file_name);
    let (mut session, mut head) = unsafe { begin_download(&current)? };
    let mut hops = 0usize;
    while is_redirect(head.status) {
        let location = head
            .location
            .take()
            .ok_or_else(|| Error::new("更新包重定向缺少地址"))?;
        current = resolve_redirect(&current, &location)
            .ok_or_else(|| Error::new("更新包重定向地址不可信"))?;
        hops += 1;
        if hops > MAX_DOWNLOAD_REDIRECTS {
            return Err(Error::new("更新包重定向次数过多"));
        }
        let (next_session, next_head) = unsafe { begin_download(&current)? };
        session = next_session;
        head = next_head;
    }
    Ok((session, head))
}

/// Parse one `<sha256>  <filename>` entry from a SHA256SUMS file. Both GNU
/// separators (`  ` for binary, ` *` for text) are accepted; comments, blank
/// lines and unrelated assets are ignored. A conflicting duplicate is fatal —
/// an ambiguous checksum file must never turn into a successful verification.
fn parse_sha256sums(contents: &str, expected_file: &str) -> Result<[u8; 32]> {
    let mut found: Option<[u8; 32]> = None;
    for line in contents.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((hex, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim_start_matches([' ', '\t', '*']);
        if name != expected_file || hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let mut bytes = [0u8; 32];
        for (slot, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let pair = std::str::from_utf8(pair).map_err(|_| Error::new("校验文件包含无效字符"))?;
            bytes[slot] = u8::from_str_radix(pair, 16)
                .map_err(|_| Error::new("校验文件包含无效 SHA-256 值"))?;
        }
        match found {
            Some(previous) if previous != bytes => {
                return Err(Error::new("SHA256SUMS.txt 中安装包校验值冲突"));
            }
            _ => found = Some(bytes),
        }
    }
    found.ok_or_else(|| Error::new(format!("SHA256SUMS.txt 缺少 {expected_file} 的校验值")))
}

/// Download and parse the release-side checksum manifest. A 404 is a hard
/// failure: an unsigned, unverified exe must never reach ShellExecuteW just
/// because an older release forgot to publish its hashes.
fn fetch_expected_hash(version: &str) -> Result<[u8; 32]> {
    let (session, head) = open_release_asset(version, CHECKSUMS_FILE_NAME)?;
    if head.status == 404 {
        return Err(Error::new(
            "发布页缺少 SHA256SUMS.txt，出于安全考虑拒绝安装；请到发布页手动下载",
        ));
    }
    if !(200..300).contains(&head.status) {
        return Err(Error::new(format!(
            "下载校验文件失败（HTTP {}）",
            head.status
        )));
    }
    if !content_length_within_limit(head.total, MAX_CHECKSUM_BYTES) {
        return Err(Error::new("校验文件过大，已拒绝"));
    }
    let body = crate::net::read_response(session.request.0, MAX_CHECKSUM_BYTES as usize)?;
    let text = String::from_utf8(body).map_err(|_| Error::new("校验文件不是有效的 UTF-8"))?;
    parse_sha256sums(&text, &asset_file_name(version))
}

/// Streaming SHA-256: the installer can be hundreds of MiB, so never read it
/// into memory just to hash it.
fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finalize().into())
}

fn verify_hash(path: &Path, expected: &[u8; 32]) -> Result<()> {
    let actual = hash_file(path)?;
    if actual != *expected {
        return Err(Error::new(
            "安装包 SHA-256 校验失败：磁盘内容与发布文件不一致",
        ));
    }
    Ok(())
}

/// 安装前用下载时缓存的哈希复核磁盘文件，防止落盘到点击安装之间被替换。
pub fn verify_installer_hash(path: &Path, expected: &[u8; 32]) -> Result<()> {
    if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("exe") {
        return Err(Error::new("安装程序不存在或已失效，请重新下载"));
    }
    verify_hash(path, expected)
}

/// 安装范围由用户选择，而不是由启动器擅自提权。安装器用
/// `PrivilegesRequiredOverridesAllowed` 接受 `/CURRENTUSER` 与 `/ALLUSERS`，
/// 命令行参数会抑制安装器自己的范围询问对话框。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallScope {
    /// 每用户安装：不弹 UAC，与安装器的 `PrivilegesRequired=lowest` 一致。
    CurrentUser,
    /// 全机器安装：必须用 `runas` 提权，UAC 拒绝时明确报错。
    AllUsers,
}

/// 用 ShellExecuteW 拉起下载好的安装包。verb 与安装范围一一对应，不再有
/// "runas 失败偷偷退回 open" 造成的权限模型错乱。
pub fn launch_installer(path: &Path, scope: InstallScope) -> Result<()> {
    if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("exe") {
        return Err(Error::new("安装程序不存在或已失效，请重新下载"));
    }
    let (verb, params) = match scope {
        InstallScope::CurrentUser => ("open", "/CURRENTUSER"),
        InstallScope::AllUsers => ("runas", "/ALLUSERS"),
    };
    let wide_verb = nul_terminated(verb);
    let target = nul_terminated(&path.to_string_lossy());
    let wide_params = nul_terminated(params);
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(wide_verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR(wide_params.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 把失败码伪装成 HINSTANCE；UAC 被拒时返回 1223（>32），
    // 不能只看 <= 32，否则用户点了“否”也会被当成启动成功、毫无反馈。
    let code = result.0 as usize;
    if code == ERROR_CANCELLED.0 as usize {
        return Err(Error::new("已取消安装：请在用户账户控制提示中选择“是”"));
    }
    if code <= 32 {
        return Err(Error::new("无法启动安装程序，请手动到下载目录运行"));
    }
    Ok(())
}

fn is_redirect(status: u32) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Resolve a `Location` header. Only HTTPS is allowed, and the host must be
/// GitHub or its asset CDN, so a redirect cannot downgrade or exfiltrate.
fn resolve_redirect(base: &str, location: &str) -> Option<String> {
    let loc = location.trim();
    if loc.is_empty() {
        return None;
    }
    if let Some(rest) = loc.strip_prefix("https://") {
        return trusted_asset_authority(rest).then(|| loc.to_string());
    }
    if loc.starts_with("http://") {
        return None;
    }
    if let Some(path) = loc.strip_prefix('/') {
        let host = base
            .strip_prefix("https://")
            .and_then(|rest| rest.split(['/', '?', '#']).next())
            .filter(|host| !host.is_empty() && !host.contains('@'));
        return host.map(|host| format!("https://{host}/{path}"));
    }
    None
}

fn trusted_asset_authority(authority_and_path: &str) -> bool {
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

struct ResponseHead {
    status: u32,
    total: Option<u64>,
    location: Option<String>,
}

/// Keeps the session and connect handles alive for as long as the request is
/// being read. Closing a parent early is usually harmless, but there is no
/// reason to risk it here.
struct DownloadSession {
    _session: Handle,
    _connect: Handle,
    request: Handle,
}

struct UrlParts {
    host: Vec<u16>,
    path: Vec<u16>,
    port: u16,
    secure: bool,
}

fn crack_download_url(url: &str) -> Result<UrlParts> {
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
        .map_err(|_| Error::new(format!("更新地址无效：{}", crate::net::redact_url(url))))?;
    let host = unsafe { slice_of(parts.lpszHostName, parts.dwHostNameLength) };
    let mut path = unsafe { slice_of(parts.lpszUrlPath, parts.dwUrlPathLength) };
    path.extend_from_slice(&unsafe { slice_of(parts.lpszExtraInfo, parts.dwExtraInfoLength) });
    Ok(UrlParts {
        host,
        path,
        port: parts.nPort,
        secure: parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS,
    })
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
    // A fixed buffer avoids the ERROR_INSUFFICIENT_BUFFER probe; 4096 wide
    // chars (8 KiB) is far more than a Location header needs.
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

unsafe fn begin_download(url: &str) -> Result<(DownloadSession, ResponseHead)> {
    let parts = crack_download_url(url)?;
    unsafe {
        let session = Handle(WinHttpOpen(
            PCWSTR(nul_terminated("FastPin").as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            // 与检查更新同一条网络初始化路径，文案保持一致。
            return Err(Error::new("网络初始化失败，请检查网络或代理设置后重试"));
        }
        let ms = (TIMEOUT_SECONDS * 1000) as i32;
        WinHttpSetTimeouts(session.0, ms, ms, ms, ms).map_err(|e| crate::net::winhttp_error(&e))?;

        let connect = Handle(WinHttpConnect(
            session.0,
            PCWSTR(nul_terminated_from(&parts.host).as_ptr()),
            parts.port,
            0,
        ));
        if connect.0.is_null() {
            // 下载同样连不上更新服务器，提示稍后重试。
            return Err(Error::new("无法连接到更新服务器，请稍后重试"));
        }

        let request = Handle(WinHttpOpenRequest(
            connect.0,
            PCWSTR(nul_terminated("GET").as_ptr()),
            PCWSTR(nul_terminated_from(&parts.path).as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            if parts.secure {
                WINHTTP_FLAG_SECURE
            } else {
                Default::default()
            },
        ));
        if request.0.is_null() {
            // 会话与连接都有效却建不出请求，按瞬时故障提示重试。
            return Err(Error::new("无法创建更新请求，请稍后重试"));
        }

        WinHttpSendRequest(request.0, None, None, 0, 0, 0)
            .map_err(|e| crate::net::winhttp_error(&e))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|e| crate::net::winhttp_error(&e))?;

        let status = query_u32_header(request.0, WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        let total = query_u32_header(request.0, WINHTTP_QUERY_CONTENT_LENGTH).map(u64::from);
        let location = query_string_header(request.0, WINHTTP_QUERY_LOCATION);

        Ok((
            DownloadSession {
                _session: session,
                _connect: connect,
                request,
            },
            ResponseHead {
                status,
                total,
                location,
            },
        ))
    }
}

fn download_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|root| PathBuf::from(root).join("FastPin").join("downloads"))
        .unwrap_or_else(std::env::temp_dir)
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_temp_path(dir: &Path, version: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(
        "FastPin-v{}-{}-{nanos}-{seq}.part",
        normalize_version(version),
        std::process::id()
    ))
}

/// 下载临时文件与已下载的安装包超过这个时长就视为遗留，可以安全删除。
/// 24 小时门槛给「下载完但还没点安装」的用户留足时间，避免误删刚下好的包。
const DOWNLOAD_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// 本程序下载的成品安装包名：`FastPin-v<版本>-x64-setup.exe`；撞名时为
/// `...-setup-2.exe` 等。只认这套命名，绝不删除下载目录里的其它 exe。
fn is_downloaded_installer(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".exe") else {
        return false;
    };
    let Some(rest) = stem.strip_prefix("FastPin-v") else {
        return false;
    };
    let version = if let Some(v) = rest.strip_suffix("-setup") {
        v
    } else if let Some((v, seq)) = rest.rsplit_once("-setup-") {
        if seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        v
    } else {
        return false;
    };
    // 版本号必须以数字开头，避免把 `FastPin-v-x64-setup.exe` 这类名字算进来。
    valid_version(version) && version.bytes().next().is_some_and(|b| b.is_ascii_digit())
}

/// 删除 `dir` 中过期的下载文件，返回删除的个数。
///
/// 两类都清：`.part` 临时文件（崩溃/中断遗留）和改名后的成品安装包（用户
/// 下载完却一直没点安装，否则会长期堆积）。只按修改时间删旧文件：新下载
/// 总是用 `unique_temp_path` 生成的唯一文件名，且 `File::create` 会立即写入，
/// 所以按 24 小时门槛清理不会误删正在进行中的下载或刚下好的包。目录不存在
/// 或不可读时静默返回 0 —— 清理是尽力而为的，不能影响主流程。
fn cleanup_download_files(dir: &Path, max_age: Duration) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_part = entry.path().extension().and_then(|ext| ext.to_str()) == Some("part")
            && name.starts_with("FastPin-v");
        if !is_part && !is_downloaded_installer(&name) {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if stale && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 清理 downloads 目录中超过 24 小时的 `.part` 临时文件与成品安装包，供
/// 上层启动时和每次下载前调用。
///
/// 不返回错误也不打扰用户：磁盘清理失败不应阻止应用启动。
pub fn cleanup_downloads() {
    cleanup_download_files(&download_dir(), DOWNLOAD_MAX_AGE);
}

/// Download the installer for `version` and return the path to the final
/// installer file (streamed into a `.part`, SHA-256 verified against the
/// release's SHA256SUMS.txt, then renamed atomically). A failed verification
/// deletes the partial file and returns an error: no unverified exe is ever
/// handed back for execution. No shell execution, extraction or installation
/// happens here; the caller decides what to do with the returned path.
///
/// Blocking. Call it off the UI thread. `on_progress` is called once before
/// the first read (with `received == 0` and the parsed `Content-Length`) and
/// once after every chunk.
pub fn download_release_asset(
    version: &str,
    on_progress: impl FnMut(DownloadProgress),
) -> Result<PathBuf> {
    download_release_asset_with_hash(version, on_progress).map(|(path, _)| path)
}

/// Same as [`download_release_asset`], but also returns the verified hash so
/// the settings view can re-check the file immediately before launching it.
pub fn download_release_asset_with_hash(
    version: &str,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<(PathBuf, [u8; 32])> {
    if !valid_version(version) {
        return Err(Error::new("版本号无效"));
    }

    // 新一次下载开始前，先清理同一下载目录里遗留的过期下载：崩溃/中断的
    // .part 与一直没点安装的成品包从此都有上限，不会无限堆积。新文件用
    // 唯一名字创建，不会与（万一存在的）并发下载冲突。
    let dir = download_dir();
    cleanup_download_files(&dir, DOWNLOAD_MAX_AGE);

    let (session, head) = open_release_asset(version, &asset_file_name(version))?;

    if head.status == 404 {
        return Err(Error::new(format!(
            "更新包不存在（HTTP 404）：发布页可能尚未上传 {}",
            asset_file_name(version)
        )));
    }
    if !(200..300).contains(&head.status) {
        return Err(Error::new(format!(
            "下载更新包失败（HTTP {}）",
            head.status
        )));
    }

    let total = head.total;
    if !content_length_within_limit(total, MAX_DOWNLOAD_BYTES) {
        return Err(Error::new(format!(
            "更新包过大（超过 {} MiB）",
            MAX_DOWNLOAD_BYTES / 1024 / 1024
        )));
    }

    fs::create_dir_all(&dir)?;
    let path = unique_temp_path(&dir, version);

    let expected = fetch_expected_hash(version)?;
    match stream_body_to_file(session.request.0, &path, total, &mut on_progress)
        .and_then(|()| verify_hash(&path, &expected))
    {
        // 哈希校验通过后立刻改名：`.part` 会被 24 小时清理任务删除，而且没有
        // 安装器会用它。改名在同目录内完成，是原子操作，撞名时换序号而非覆盖。
        Ok(()) => finalize_download(&path, &dir, version).map(|final_path| (final_path, expected)),
        Err(e) => {
            // 截断、超限或哈希不符的文件一律不留：下次启动的清理任务不该是
            // 唯一拦住未验证安装包的防线。
            let _ = fs::remove_file(&path);
            Err(e)
        }
    }
}

/// Closes on drop, so every early return above still releases the handle.
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

/// `lpszX`/`dwXLength` pairs point into the caller's cracked URL; copy out.
unsafe fn slice_of(p: windows::core::PWSTR, len: u32) -> Vec<u16> {
    if p.is_null() || len == 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(p.0, len as usize) }.to_vec()
}

fn nul_terminated(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn nul_terminated_from(s: &[u16]) -> Vec<u16> {
    s.iter().copied().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{checked_accumulate, progress_percent};

    #[test]
    fn version_compare_ignores_prefix_and_padding() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.2", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1"));
        assert!(!is_newer("0.1.0-beta", "0.1.0"));
    }

    #[test]
    fn version_compare_handles_prerelease_and_fourth_segment() {
        // 常规升级必须正确提示。
        assert!(is_newer("0.3.1", "0.3.0"));
        assert!(is_newer("v0.3.1", "0.3.0"));
        // 预发布版旧于对应正式版，不会反复提示更新。
        assert!(!is_newer("0.3.0-beta.2", "0.3.0"));
        assert!(!is_newer("0.3.0-alpha", "0.3.0-beta.2"));
        // 同为预发布时按数字标识比较。
        assert!(is_newer("0.3.0-beta.2", "0.3.0-beta.1"));
        // 第四段参与比较，缺失段按 0 补齐。
        assert!(is_newer("0.3.0.1", "0.3.0"));
        assert!(!is_newer("0.3.0", "0.3.0.1"));
        // 构建元数据不影响版本先后。
        assert!(!is_newer("0.3.0+build.5", "0.3.0"));
    }

    #[test]
    fn only_github_https_release_pages_are_trusted() {
        assert!(trusted_release_url(
            "https://github.com/yandongqiang/FastPin/releases/tag/v0.3.0"
        ));
        assert!(trusted_release_url(
            "HTTPS://GitHub.com/owner/repo/releases"
        ));
        assert!(!trusted_release_url("http://github.com/owner/repo"));
        assert!(!trusted_release_url("https://github.com@evil.example/x"));
        assert!(!trusted_release_url("file:///C:/Windows/System32/cmd.exe"));
        assert!(!trusted_release_url("https://evil.example/github.com/x"));
    }

    #[test]
    fn open_releases_page_refuses_untrusted_urls() {
        assert!(open_release_page("file:///C:/Windows/System32/cmd.exe").is_err());
        assert!(open_release_page("https://github.com@evil.example/x").is_err());
    }

    #[test]
    fn version_normalization_trims_space_and_v() {
        assert_eq!(normalize_version(" v0.3.0 "), "0.3.0");
        assert_eq!(normalize_version("V0.3.0"), "0.3.0");
        assert_eq!(normalize_version("0.3.0"), "0.3.0");
    }

    #[test]
    fn asset_url_points_at_setup_installer() {
        let url = |version: &str| release_asset_url(version, &asset_file_name(version));
        assert_eq!(
            url("0.3.0"),
            "https://github.com/yandongqiang/FastPin/releases/download/v0.3.0/FastPin-v0.3.0-x64-setup.exe"
        );
        // A tag that already carries `v` produces exactly the same URL.
        assert_eq!(url("v0.3.0"), url("0.3.0"));
        assert!(url("0.3.0").ends_with(asset_file_name("0.3.0").as_str()));
        assert!(release_asset_url("0.3.0", CHECKSUMS_FILE_NAME).ends_with(CHECKSUMS_FILE_NAME));
    }

    #[test]
    fn installer_name_keeps_asset_name_and_adds_sequence_on_collision() {
        assert_eq!(
            installer_candidate_name("0.3.0", 1),
            "FastPin-v0.3.0-x64-setup.exe",
            "首次下载沿用发布资产的文件名"
        );
        assert_eq!(
            installer_candidate_name("0.3.0", 2),
            "FastPin-v0.3.0-x64-setup-2.exe",
            "撞名时追加序号，不覆盖旧安装包"
        );
        assert_eq!(
            installer_candidate_name("v0.3.0-beta", 3),
            "FastPin-v0.3.0-beta-x64-setup-3.exe",
            "版本号前缀与序号同时生效"
        );
    }

    #[test]
    fn downloaded_installer_names_are_recognized_for_cleanup() {
        assert!(
            is_downloaded_installer(&installer_candidate_name("0.3.0", 1)),
            "首次下载的成品包要能清理"
        );
        assert!(
            is_downloaded_installer(&installer_candidate_name("0.3.0", 2)),
            "撞名追加序号的成品包也要清理"
        );
        assert!(!is_downloaded_installer("setup.exe"), "无关 exe 不能误删");
        assert!(
            !is_downloaded_installer("FastPin-v0.3.0-x64-setup.exe.bak"),
            "备份文件不能被当成安装包"
        );
        assert!(
            !is_downloaded_installer("FastPin-v-x64-setup.exe"),
            "空版本号不认，避免误删同名前缀的文件"
        );
    }

    #[test]
    fn launcher_refuses_missing_or_non_executable_paths() {
        for scope in [InstallScope::CurrentUser, InstallScope::AllUsers] {
            assert!(
                launch_installer(Path::new("C:/definitely/missing/FastPin.exe"), scope).is_err()
            );
            assert!(
                launch_installer(Path::new("C:/definitely/missing/FastPin.part"), scope).is_err()
            );
        }
    }

    #[test]
    fn checksum_manifest_matches_only_the_named_asset() {
        let name = asset_file_name("0.3.0");
        let sum = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let manifest = format!(
            "# release checksums\n0000000000000000000000000000000000000000000000000000000000000000  unrelated.exe\n{sum}  {name}\n"
        );
        assert_eq!(parse_sha256sums(&manifest, &name).unwrap()[0], 0xba);
        // GNU text mode uses ` *` instead of two spaces.
        let text_mode = format!("{sum} *{name}\r\n");
        assert_eq!(parse_sha256sums(&text_mode, &name).unwrap()[31], 0xad);
        assert!(parse_sha256sums(&manifest, "other.exe").is_err());
        assert!(parse_sha256sums("not a manifest\n", &name).is_err());
        let other = format!("{:064}", 1);
        let conflict = format!("{sum}  {name}\n{other}  {name}\n");
        assert!(parse_sha256sums(&conflict, &name).is_err());
    }

    #[test]
    fn streaming_hash_matches_a_known_digest() {
        // 用工作目录而不是系统 temp：某些受限环境下 temp_dir 不可写。
        let path = std::env::current_dir().unwrap().join(format!(
            "fastpin-hash-test-{}-{}.tmp",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, b"abc").unwrap();
        let got = hash_file(&path).unwrap();
        let _ = fs::remove_file(&path);
        let expected = parse_sha256sums(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  a.txt\n",
            "a.txt",
        )
        .unwrap();
        assert_eq!(got, expected);
    }

    #[test]
    fn invalid_versions_are_rejected_before_url_or_filename() {
        assert!(valid_version("0.3.0"));
        assert!(valid_version("v1.2.3-beta"));
        assert!(!valid_version(""));
        assert!(!valid_version("v"));
        assert!(!valid_version("0.3.0/../../evil"));
        assert!(!valid_version("0.3.0?x=1"));
        // Rejected before any WinHTTP call, so this never touches the network.
        assert!(download_release_asset("../evil", |_| {}).is_err());
    }

    #[test]
    fn size_limit_and_accumulation_are_checked() {
        assert!(content_length_within_limit(None, MAX_DOWNLOAD_BYTES));
        assert!(content_length_within_limit(
            Some(MAX_DOWNLOAD_BYTES),
            MAX_DOWNLOAD_BYTES
        ));
        assert!(!content_length_within_limit(
            Some(MAX_DOWNLOAD_BYTES + 1),
            MAX_DOWNLOAD_BYTES
        ));
        assert_eq!(checked_accumulate(10, 5, 100), Some(15));
        assert_eq!(checked_accumulate(98, 5, 100), None);
        assert_eq!(checked_accumulate(u64::MAX, 1, u64::MAX), None);
    }

    #[test]
    fn progress_percentage_is_bounded_and_optional() {
        assert_eq!(progress_percent(25, Some(100)), Some(25));
        assert_eq!(progress_percent(250, Some(100)), Some(100));
        assert_eq!(progress_percent(1, None), None);
        assert_eq!(progress_percent(1, Some(0)), None);
        assert_eq!(
            DownloadProgress {
                received: 1,
                total: Some(4)
            }
            .percent(),
            Some(25)
        );
    }

    #[test]
    fn redirects_are_limited_to_https_github_hosts() {
        let base = release_asset_url("0.3.0", &asset_file_name("0.3.0"));
        assert_eq!(
            resolve_redirect(&base, "https://objects.githubusercontent.com/x?y=1"),
            Some("https://objects.githubusercontent.com/x?y=1".to_string())
        );
        assert_eq!(
            resolve_redirect(&base, "/owner/repo/releases"),
            Some("https://github.com/owner/repo/releases".to_string())
        );
        assert_eq!(resolve_redirect(&base, "http://evil.example/x"), None);
        assert_eq!(resolve_redirect(&base, "https://evil.example/x"), None);
        assert_eq!(
            resolve_redirect(&base, "https://github.com@evil.example/x"),
            None
        );
        assert_eq!(resolve_redirect(&base, "  "), None);
    }

    #[test]
    fn redirect_statuses_are_classified() {
        assert!(is_redirect(301));
        assert!(is_redirect(302));
        assert!(is_redirect(307));
        assert!(is_redirect(308));
        assert!(!is_redirect(200));
        assert!(!is_redirect(404));
    }
}
