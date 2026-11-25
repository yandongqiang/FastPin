//! OpenAI-compatible translation through the user's own API.
//!
//! WinHTTP rather than an HTTP crate: this binary's size and cold start are the
//! point of the Rust port, and a full async stack buys nothing for one POST.

use std::ffi::c_void;

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Security::Credentials::{PasswordCredential, PasswordVault};
#[cfg(test)]
use windows::Win32::Networking::WinHttp::WINHTTP_INTERNET_SCHEME_HTTP;
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_INTERNET_SCHEME_HTTPS, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

use crate::config;
use crate::error::{Error, Result};

/// Vault entry name. Renamed with the product; `migrate_legacy_vault` moves an
/// existing key over so users do not have to paste it again.
const VAULT_RESOURCE: &str = "FastPin.Translation";
const LEGACY_VAULT_RESOURCE: &str = "Zshot.Translation";
const VAULT_ACCOUNT: &str = "apiKey";

const DEFAULT_SYSTEM_PROMPT: &str = "You are a translator. Output only the translation. Keep paragraph structure. Do not explain. Do not translate URLs, file paths, or code. Keep numbers and units.";

/// Blocking. Call it off the UI thread.
pub fn translate(text: &str) -> Result<String> {
    let cfg = config::get().translation;
    if cfg.base_url.trim().is_empty() {
        return Err(Error::new(
            "翻译未配置：请在 设置 → 翻译 中填写「接口地址」。",
        ));
    }
    let body = serde_json::json!({
        "model": cfg.model,
        "messages": [
            { "role": "system", "content": system_prompt(&cfg.prompt, &cfg.target_language) },
            { "role": "user", "content": text },
        ],
        "temperature": 0.2,
    })
    .to_string();

    let key = api_key()?;
    if !key.is_empty() && is_insecure_http(&cfg.base_url) && !cfg.allow_insecure_http {
        return Err(Error::new(
            "API Key 将通过明文 HTTP 发送。请先在 设置 → 翻译 中确认此风险。",
        ));
    }
    let response = post_json(
        &endpoint(&cfg.base_url),
        &key,
        &body,
        cfg.timeout_seconds.clamp(5, 300),
    )?;
    message_content(&response)
        .ok_or_else(|| Error::new("翻译接口返回空结果，请检查模型名称与提示词配置。"))
}

/// Whether the actual request would be plaintext — anything that is not HTTPS.
/// Used by both the runtime guard and the settings warning banner so the two
/// cannot disagree.
///
/// 判定必须与 `post_json` 的 `WINHTTP_FLAG_SECURE` 判定同源：后者只在
/// `parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS` 时加安全标志，其余 scheme
/// （`http`、`ftp` 等）都会把 `Authorization: Bearer <key>` 明文发出。
/// 因此这里问的不是"字面是不是 http"，而是"非 HTTPS 即明文"。解析失败也
/// 按不安全处理——此时 `post_json` 同样会失败，不会真的发出请求。
/// 函数名中的 `http` 是历史遗留，保持原名以免改动调用点。
pub fn is_insecure_http(base_url: &str) -> bool {
    let wide: Vec<u16> = base_url.trim().encode_utf16().chain([0]).collect();
    let mut parts = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        // u32::MAX asks WinHttp for pointers into `wide` instead of copies.
        dwSchemeLength: u32::MAX,
        dwHostNameLength: u32::MAX,
        dwUrlPathLength: u32::MAX,
        dwExtraInfoLength: u32::MAX,
        ..Default::default()
    };
    if unsafe { WinHttpCrackUrl(&wide, 0, &mut parts) }.is_ok() {
        return parts.nScheme != WINHTTP_INTERNET_SCHEME_HTTPS;
    }
    // 解析失败时 post_json 同样会失败，不会真的发出请求；这里保守兜底为
    // true，宁可多提示一次也不能把可疑地址当成安全的 HTTPS 放行。
    true
}

/// The vault entry is optional: local gateways commonly take no key.
pub fn api_key() -> Result<String> {
    let vault = PasswordVault::new()?;
    let Ok(cred) = vault.Retrieve(
        &HSTRING::from(VAULT_RESOURCE),
        &HSTRING::from(VAULT_ACCOUNT),
    ) else {
        return Ok(String::new());
    };
    cred.RetrievePassword()?;
    Ok(cred.Password()?.to_string())
}

/// Replaces the stored key. An empty value removes the entry rather than
/// storing a blank one, so `api_key` keeps reporting "no key" the same way it
/// does before the user ever opens the settings window.
pub fn set_api_key(key: &str) -> Result<()> {
    let vault = PasswordVault::new()?;
    let resource = HSTRING::from(VAULT_RESOURCE);
    let account = HSTRING::from(VAULT_ACCOUNT);
    let key = key.trim();

    // 先备份旧凭据再删除：Add/CreatePasswordCredential 任一失败都要把旧
    // Key 放回去，否则设置页只显示“保存失败”，原 Key 却已经没了。
    // RetrievePassword 把密码读进对象，之后可以直接把同一个对象 Add 回去。
    let old = match vault.Retrieve(&resource, &account) {
        Ok(cred) => {
            cred.RetrievePassword()?;
            vault.Remove(&cred)?;
            Some(cred)
        }
        Err(_) => None,
    };

    if key.is_empty() {
        return Ok(());
    }

    let result = (|| -> Result<()> {
        let cred =
            PasswordCredential::CreatePasswordCredential(&resource, &account, &HSTRING::from(key))?;
        vault.Add(&cred)?;
        Ok(())
    })();
    if result.is_err() {
        if let Some(cred) = old.as_ref() {
            let _ = vault.Add(cred);
        }
    }
    result
}

/// 读取 vault 条目密码的三种结果。`Retrieve` 成功只证明条目存在，密码
/// 读取的瞬时错误必须与"确认是空密码"分开，否则会把有效 key 误判成可删除。
#[derive(Debug, Clone, PartialEq, Eq)]
enum VaultPassword {
    Valid(String),
    Empty,
    Unreadable,
}

impl VaultPassword {
    /// 从条目读取密码并分类。任何读取失败都归入 Unreadable，绝不降级成 Empty。
    fn read(cred: &PasswordCredential) -> Self {
        if cred.RetrievePassword().is_err() {
            return Self::Unreadable;
        }
        match cred.Password() {
            Ok(password) if !password.is_empty() => Self::Valid(password.to_string()),
            Ok(_) => Self::Empty,
            Err(_) => Self::Unreadable,
        }
    }
}

/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`。Credential Manager 对"没有该条目"
/// 返回这个码；这是全新安装、需要从 legacy 迁移的正常场景，而不是读失败。
const HRESULT_NOT_FOUND: i32 = 0x8007_0490u32 as i32;

/// 区分"条目不存在"与"读取条目/密码失败"：
/// - 条目不存在：等同空条目，允许从 legacy 补写（正常迁移路径）；
/// - 其它 Retrieve 错误 / 密码读取失败：视为瞬时不可读，跳过迁移，绝不删 key；
/// - 条目存在：只有密码读取成功且确认为空时才允许替换。
fn classify_current(
    existing: Option<&PasswordCredential>,
    retrieve_error: Option<i32>,
) -> VaultPassword {
    if let Some(cred) = existing {
        return VaultPassword::read(cred);
    }
    match retrieve_error {
        Some(code) if code != HRESULT_NOT_FOUND => VaultPassword::Unreadable,
        // 没有条目（None）或未找到（HRESULT_NOT_FOUND）都按空条目处理。
        _ => VaultPassword::Empty,
    }
}

/// 迁移决策。current 只有在确认密码为空时才可能被替换；读取失败或已有效
/// 时一律保留。legacy 无可用值时同样保留 current，绝不留下"两边都删空"。
#[derive(Debug, Clone, PartialEq, Eq)]
enum MigrationDecision {
    AlreadyMigrated,
    SkipUnreadableCurrent,
    ReplaceEmptyCurrent(String),
    NothingToMigrate,
}

/// 纯函数：把 current/legacy 的读取结果映射成迁移动作，便于单测覆盖
/// Credential Manager 难以在测试中模拟的瞬时失败分支。
fn migration_decision(current: VaultPassword, legacy: VaultPassword) -> MigrationDecision {
    match current {
        VaultPassword::Valid(_) => MigrationDecision::AlreadyMigrated,
        VaultPassword::Unreadable => MigrationDecision::SkipUnreadableCurrent,
        VaultPassword::Empty => match legacy {
            VaultPassword::Valid(key) => MigrationDecision::ReplaceEmptyCurrent(key),
            VaultPassword::Empty | VaultPassword::Unreadable => MigrationDecision::NothingToMigrate,
        },
    }
}

/// Copies a key stored under the legacy credential resource to
/// `FastPin.Translation`, then drops the old entry. A missing legacy entry is
/// the normal case (fresh install), not an error. Best-effort: every failure is
/// reported as a note for the caller to log and never blocks startup.
/// A current entry whose password reads as empty is replaced with the legacy
/// value; a failed `Retrieve` of the current entry (including "not found") and
/// an unreadable password count as transient failures and leave both entries
/// untouched so a failed read can never delete a valid key.
pub(crate) fn migrate_legacy_vault() -> Vec<String> {
    let vault = match PasswordVault::new() {
        Ok(v) => v,
        Err(e) => return vec![format!("vault: unavailable ({e})")],
    };
    let account = HSTRING::from(VAULT_ACCOUNT);
    let legacy = HSTRING::from(LEGACY_VAULT_RESOURCE);
    let current = HSTRING::from(VAULT_RESOURCE);
    let Ok(old) = vault.Retrieve(&legacy, &account) else {
        return Vec::new();
    };
    let legacy_password = VaultPassword::read(&old);
    if matches!(&legacy_password, VaultPassword::Unreadable) {
        return vec!["vault: legacy translation key unreadable".into()];
    }

    // Retrieve 成功只说明条目存在，不代表 key 可用；读不到密码就更不能删。
    // Retrieve 失败（含条目不存在）也不能当成"确认空条目"：没读到就没有
    // 替换权限，按 Unreadable 跳过，只有确认空才走替换路径。
    // Retrieve 失败要区分"没有条目"（正常迁移）与"读失败"（瞬时错误）。
    // 后者绝不能当成空条目去替换，否则杀软/凭据管理器瞬时抖动就可能删掉有效 key。
    let mut existing = None;
    let mut retrieve_error = None;
    match vault.Retrieve(&current, &account) {
        Ok(cred) => existing = Some(cred),
        Err(e) => retrieve_error = Some(e.code().0),
    }
    let current_password = classify_current(existing.as_ref(), retrieve_error);

    match migration_decision(current_password, legacy_password) {
        // 幂等：current 已有有效 key，绝不动它，只清掉重复的 legacy 条目。
        MigrationDecision::AlreadyMigrated => match vault.Remove(&old) {
            Ok(()) => vec!["vault: duplicate legacy translation key removed".into()],
            Err(_) => vec!["vault: FastPin key already present, legacy entry not removed".into()],
        },
        // Credential Manager 瞬时错误：立即返回，current 与 legacy 都保留。
        MigrationDecision::SkipUnreadableCurrent => {
            vec!["vault: cannot read current translation key, skip migration".into()]
        }
        // legacy 无可用值：不删 current（即使它是空条目），也不动 legacy。
        MigrationDecision::NothingToMigrate => {
            vec!["vault: no translation key to migrate".into()]
        }
        MigrationDecision::ReplaceEmptyCurrent(key) => {
            // current 已确认为空，Add 不接受同名条目：先删坏条目再补写。
            if let Some(cred) = existing {
                if let Err(e) = vault.Remove(&cred) {
                    return vec![format!("vault: could not remove empty FastPin entry ({e})")];
                }
            }
            let cred = match PasswordCredential::CreatePasswordCredential(
                &current,
                &account,
                &HSTRING::from(key.as_str()),
            ) {
                Ok(c) => c,
                Err(e) => return vec![format!("vault: could not create FastPin entry ({e})")],
            };
            if let Err(e) = vault.Add(&cred) {
                return vec![format!("vault: could not save FastPin entry ({e})")];
            }
            match vault.Remove(&old) {
                Ok(()) => vec!["vault: moved translation key to FastPin.Translation".into()],
                Err(_) => vec!["vault: FastPin key written, legacy entry not removed".into()],
            }
        }
    }
}

/// Accept a bare host, a `/v1` root, or the full path — users paste all three.
fn endpoint(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        return base.to_string();
    }
    if base.ends_with("/v1") {
        return format!("{base}/chat/completions");
    }
    format!("{base}/v1/chat/completions")
}

fn system_prompt(custom: &str, target_language: &str) -> String {
    let base = if custom.trim().is_empty() {
        DEFAULT_SYSTEM_PROMPT
    } else {
        custom.trim()
    };
    format!("{base}\nTarget language: {target_language}")
}

/// `choices[0].message.content`, with the streaming and Responses-API shapes as
/// fallbacks. Gateways return all three.
fn message_content(json: &str) -> Option<String> {
    // An HTML error page from a proxy is not worth handing to the JSON parser.
    if json.trim_start().starts_with('<') {
        return None;
    }
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let first = root.get("choices").and_then(|c| c.get(0));
    let text = first
        .and_then(|c| c.pointer("/message/content"))
        .or_else(|| first.and_then(|c| c.pointer("/delta/content")))
        .or_else(|| root.get("output_text"))
        .and_then(|v| v.as_str())?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// `{"error":{"message":"..."}}` —— 网关失败时真正的原因在这里（401 只显示
/// 状态码会把"API Key 无效"这类信息丢掉）。与 `message_content` 一样容错：
/// 非 JSON / HTML 错误页一律忽略，文本经去敏（含本次请求实际使用的 Key）、
/// 截断后才给用户看。
fn error_message(json: &str, key: &str) -> Option<String> {
    if json.trim_start().starts_with('<') {
        return None;
    }
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let msg = root
        .get("error")
        .and_then(|e| e.get("message"))
        .or_else(|| root.get("error"))
        .and_then(|v| v.as_str())?;
    let msg = crate::net::redact_api_message_with_secret(msg.trim(), key);
    (!msg.is_empty()).then_some(msg)
}

/// Closes on every path, including the error returns below.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

/// Removes CR/LF from an API Key before it is written into a WinHTTP header.
/// A pasted key containing `\r\n` could otherwise terminate the Authorization
/// line and inject extra headers into the request.
fn header_safe_key(key: &str) -> String {
    key.chars().filter(|c| *c != '\r' && *c != '\n').collect()
}

/// Per-phase timeout handed to `WinHttpSetTimeouts`, in milliseconds.
///
/// WinHTTP applies its four timeouts (resolve / connect / send / receive) to
/// *each phase separately* — they are not one budget for the whole exchange.
/// Passing the configured `timeout_seconds * 1000` to all four therefore lets
/// a slow gateway occupy the background executor for roughly 4x the configured
/// time (300 s -> ~20 minutes). Split the budget between the phases instead,
/// with a 2 s floor so a slow DNS lookup still has a chance to finish.
///
/// Two limits remain by design: receive timeout is re-armed for every chunk,
/// so a slow trickle can still outlast the total budget, and there is no
/// cancellation — the request runs on a background thread where closing the
/// WinHTTP handle from another thread is unsafe. Short per-phase limits are
/// the whole mitigation.
fn phase_timeout_ms(timeout_seconds: u32) -> i32 {
    const PHASES: u32 = 4;
    const MIN_MS: u64 = 2_000;
    let total_ms = u64::from(timeout_seconds.clamp(5, 300)).saturating_mul(1000);
    let per_phase = (total_ms / u64::from(PHASES)).max(MIN_MS);
    per_phase.min(i32::MAX as u64) as i32
}

fn post_json(url: &str, api_key: &str, body: &str, timeout_seconds: u32) -> Result<String> {
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
    // 与设置页的「接口地址」保持同一术语；地址里的用户名密码须脱敏后再回显。
    unsafe { WinHttpCrackUrl(&wide, 0, &mut parts) }.map_err(|_| {
        Error::new(format!(
            "接口地址无效，请在 设置 → 翻译 中检查地址：{}",
            crate::net::redact_url(url)
        ))
    })?;

    let host = unsafe { slice_of(parts.lpszHostName, parts.dwHostNameLength) };
    let mut path = unsafe { slice_of(parts.lpszUrlPath, parts.dwUrlPathLength) };
    path.extend_from_slice(&unsafe { slice_of(parts.lpszExtraInfo, parts.dwExtraInfoLength) });
    let secure = parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS;

    let mut headers = String::from("Content-Type: application/json\r\n");
    if !api_key.trim().is_empty() {
        // Key 来自用户粘贴的文本框：其中的 CR/LF 会提前结束 Authorization
        // 头并注入任意请求头，必须在拼接头之前剥掉。
        let key = header_safe_key(api_key.trim());
        if !key.is_empty() {
            headers.push_str(&format!("Authorization: Bearer {key}\r\n"));
        }
    }

    unsafe {
        let session = Handle(WinHttpOpen(
            PCWSTR(nul_terminated("FastPin").as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            // 本地网络栈初始化失败，给出可操作的方向。
            return Err(Error::new("网络初始化失败，请检查网络或代理设置后重试"));
        }
        // 配置值是整个请求的总预算：四段各拿约四分之一（并有 2 s 下限），
        // 墙钟因此接近配置值；接收超时按块重计时，慢速流仍可能超出。
        // 设置页的"请求超时"与此语义一致，详见 phase_timeout_ms。
        let ms = phase_timeout_ms(timeout_seconds);
        WinHttpSetTimeouts(session.0, ms, ms, ms, ms).map_err(|e| crate::net::winhttp_error(&e))?;

        let host_w = nul_terminated_from(&host);
        let connect = Handle(WinHttpConnect(
            session.0,
            PCWSTR(host_w.as_ptr()),
            parts.nPort,
            0,
        ));
        if connect.0.is_null() {
            // 连不上翻译接口，多为代理或 DNS 问题，提示稍后重试。
            return Err(Error::new("无法连接到翻译服务器，请稍后重试"));
        }

        let verb = nul_terminated("POST");
        let path_w = nul_terminated_from(&path);
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path_w.as_ptr()),
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
            return Err(Error::new("无法创建翻译请求，请稍后重试"));
        }

        let header_w: Vec<u16> = headers.encode_utf16().collect();
        WinHttpSendRequest(
            request.0,
            Some(&header_w),
            Some(body.as_ptr() as *const c_void),
            body.len() as u32,
            body.len() as u32,
            0,
        )
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
            // 401 只报状态码会丢掉"API Key 无效"这类真正原因。附上服务端
            // error.message（已去敏、截断），HTML 错误页等无法解析的情况则
            // 退回原来的简短状态码提示。
            let detail = String::from_utf8_lossy(&payload);
            let detail = error_message(&detail, api_key)
                .map(|m| format!("：{m}"))
                .unwrap_or_default();
            return Err(Error::new(format!("翻译失败（HTTP {status}）{detail}")));
        }
        String::from_utf8(payload)
            .map_err(|_| Error::new("翻译接口返回的内容不是有效的 UTF-8 文本。"))
    }
}

unsafe fn slice_of(p: PWSTR, len: u32) -> Vec<u16> {
    unsafe {
        if p.is_null() || len == 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(p.0, len as usize).to_vec()
    }
}

fn nul_terminated(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn nul_terminated_from(s: &[u16]) -> Vec<u16> {
    s.iter().copied().chain([0]).collect()
}

/// Test-only probe: the scheme WinHTTP actually resolved, or `None` when it
/// refuses the URL. Keeps the backslash-URL test tied to real parser behavior
/// instead of an assumption about it.
#[cfg(test)]
fn cracked_scheme(url: &str) -> Option<u16> {
    let wide: Vec<u16> = url.trim().encode_utf16().chain([0]).collect();
    let mut parts = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        dwSchemeLength: u32::MAX,
        dwHostNameLength: u32::MAX,
        dwUrlPathLength: u32::MAX,
        dwExtraInfoLength: u32::MAX,
        ..Default::default()
    };
    if unsafe { WinHttpCrackUrl(&wide, 0, &mut parts) }.is_ok() {
        Some(parts.nScheme.0 as u16)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_keeps_a_valid_current_key_in_every_legacy_state() {
        // current 已有效：无论 legacy 是什么，都不进入删除/补写路径。
        let decision = migration_decision(
            VaultPassword::Valid("sk-current".into()),
            VaultPassword::Valid("sk-legacy".into()),
        );
        assert_eq!(
            decision,
            MigrationDecision::AlreadyMigrated,
            "a valid current key is already migrated"
        );
        let decision = migration_decision(
            VaultPassword::Valid("sk-current".into()),
            VaultPassword::Empty,
        );
        assert_eq!(
            decision,
            MigrationDecision::AlreadyMigrated,
            "an empty legacy entry must not remove a valid current key"
        );
        let decision = migration_decision(
            VaultPassword::Valid("sk-current".into()),
            VaultPassword::Unreadable,
        );
        assert_eq!(
            decision,
            MigrationDecision::AlreadyMigrated,
            "an unreadable legacy entry must not remove a valid current key"
        );
    }

    #[test]
    fn migration_never_deletes_an_unreadable_current_key() {
        // Credential Manager 瞬时错误必须立即停下，不能当成空密码删条目。
        let decision = migration_decision(
            VaultPassword::Unreadable,
            VaultPassword::Valid("sk-legacy".into()),
        );
        assert_eq!(
            decision,
            MigrationDecision::SkipUnreadableCurrent,
            "an unreadable current key must be preserved, not replaced"
        );
        let decision = migration_decision(VaultPassword::Unreadable, VaultPassword::Empty);
        assert_eq!(
            decision,
            MigrationDecision::SkipUnreadableCurrent,
            "an empty legacy must not turn a failed current read into a deletion"
        );
        let decision = migration_decision(VaultPassword::Unreadable, VaultPassword::Unreadable);
        assert_eq!(
            decision,
            MigrationDecision::SkipUnreadableCurrent,
            "both unreadable still preserves the current entry"
        );
    }

    #[test]
    fn a_transient_current_retrieve_failure_never_counts_as_confirmed_empty() {
        // 非 NotFound 的 Retrieve 失败可能只是瞬时错误，绝不当空条目补写。
        let transient_error = HRESULT_NOT_FOUND.wrapping_add(1);
        let decision = migration_decision(
            classify_current(None, Some(transient_error)),
            VaultPassword::Valid("sk-legacy".into()),
        );
        assert_eq!(
            decision,
            MigrationDecision::SkipUnreadableCurrent,
            "a failed retrieve must skip migration, not replace the entry"
        );
    }

    #[test]
    fn a_missing_current_entry_still_migrates_the_legacy_key() {
        // 条目不存在是正常迁移场景（全新安装），不能被当成读取失败跳过。
        let expected = MigrationDecision::ReplaceEmptyCurrent("sk-legacy".into());
        let decision = migration_decision(
            classify_current(None, None),
            VaultPassword::Valid("sk-legacy".into()),
        );
        assert_eq!(decision, expected.clone(), "no entry at all must migrate");
        let decision = migration_decision(
            classify_current(None, Some(HRESULT_NOT_FOUND)),
            VaultPassword::Valid("sk-legacy".into()),
        );
        assert_eq!(decision, expected, "ERROR_NOT_FOUND must migrate");
    }

    #[test]
    fn migration_only_replaces_a_confirmed_empty_current_with_a_legacy_key() {
        let decision = migration_decision(
            VaultPassword::Empty,
            VaultPassword::Valid("sk-legacy".into()),
        );
        assert_eq!(
            decision,
            MigrationDecision::ReplaceEmptyCurrent("sk-legacy".into()),
            "a confirmed empty current is replaced with the legacy key"
        );
        let decision = migration_decision(VaultPassword::Empty, VaultPassword::Empty);
        assert_eq!(
            decision,
            MigrationDecision::NothingToMigrate,
            "no valid legacy key means the current entry stays untouched"
        );
        let decision = migration_decision(VaultPassword::Empty, VaultPassword::Unreadable);
        assert_eq!(
            decision,
            MigrationDecision::NothingToMigrate,
            "an unreadable legacy key is not proof of an empty key"
        );
    }

    #[test]
    fn api_key_cannot_inject_extra_http_headers() {
        assert_eq!(header_safe_key("sk-valid"), "sk-valid");
        assert_eq!(
            header_safe_key("sk-a\r\nX-Evil: 1\nX-Other: 2"),
            "sk-aX-Evil: 1X-Other: 2"
        );
        assert_eq!(header_safe_key("\r\n"), "");
    }

    #[test]
    fn timeout_budget_is_split_across_winhttp_phases() {
        // 四段之和应与配置的预算同量级，而不是每段都拿到整份预算。
        assert_eq!(phase_timeout_ms(30), 7_500, "30 s budget / 4");
        assert_eq!(phase_timeout_ms(300), 75_000, "worst case stays ~5 min");
        // 小预算也要保住慢 DNS：2 s 下限会让墙钟略超配置，但远好于原来的 4x。
        assert_eq!(phase_timeout_ms(5), 2_000, "2 s floor for short budgets");
        // 异常配置同样收敛到安全区间，避免 u32 溢出或无限等待。
        assert_eq!(phase_timeout_ms(0), 2_000, "clamped up to 5 s");
        assert_eq!(phase_timeout_ms(u32::MAX), 75_000, "clamped down to 300 s");
    }

    #[test]
    fn detects_plaintext_http_endpoints() {
        assert!(is_insecure_http("http://localhost:11434"));
        assert!(is_insecure_http("  HTTP://gateway.local/v1 "));
        assert!(!is_insecure_http("https://api.openai.com/v1"));
        // 非 HTTPS 一律按明文处理：post_json 只在 HTTPS 时加 WINHTTP_FLAG_SECURE。
        assert!(is_insecure_http("ftp://gateway.local/v1"));
        // 解析失败（含空串）时 post_json 同样失败，保守兜底为 true。
        assert!(is_insecure_http(""));
        // 反斜杠形式：本机实测 WinHttpCrackUrl 返回 12005（解析失败），
        // 因此不构成绕过；其他版本若真解析为 HTTP，scheme 必须是 HTTP。
        if let Some(scheme) = cracked_scheme("http:\\gateway.local\\v1") {
            assert_eq!(
                scheme, WINHTTP_INTERNET_SCHEME_HTTP.0 as u16,
                "WinHTTP 若解析该 URL，必须按明文 HTTP 判为不安全"
            );
        }
        // None 时 post_json 不会发请求；两种情形守卫都必须返回 true。
        assert!(is_insecure_http("http:\\gateway.local\\v1"));
    }

    #[test]
    fn endpoint_accepts_the_three_shapes_users_paste() {
        let full = "https://api.openai.com/v1/chat/completions";
        assert_eq!(endpoint("https://api.openai.com"), full);
        assert_eq!(endpoint("https://api.openai.com/"), full);
        assert_eq!(endpoint("https://api.openai.com/v1"), full);
        assert_eq!(endpoint("https://api.openai.com/v1/"), full);
        assert_eq!(
            endpoint("  https://api.openai.com/v1/chat/completions  "),
            full
        );
    }

    #[test]
    fn a_custom_prompt_replaces_the_default_but_keeps_the_target_line() {
        assert_eq!(
            system_prompt("", "zh-CN"),
            format!("{DEFAULT_SYSTEM_PROMPT}\nTarget language: zh-CN")
        );
        assert_eq!(
            system_prompt("  只译不解释  ", "en"),
            "只译不解释\nTarget language: en"
        );
    }

    #[test]
    fn reads_the_standard_chat_completion_shape() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"  你好世界  "}}]}"#;
        assert_eq!(message_content(json).as_deref(), Some("你好世界"));
    }

    #[test]
    fn falls_back_to_delta_and_output_text() {
        assert_eq!(
            message_content(r#"{"choices":[{"delta":{"content":"partial"}}]}"#).as_deref(),
            Some("partial")
        );
        assert_eq!(
            message_content(r#"{"output_text":"responses api"}"#).as_deref(),
            Some("responses api")
        );
    }

    #[test]
    fn error_message_extracts_and_redacts_the_providers_reason() {
        let json = r#"{"error":{"message":"Incorrect API key provided: sk-abcdefghijklmnop"}}"#;
        let msg = error_message(json, "").expect("error.message is present");
        assert!(msg.contains("Incorrect API key"));
        assert!(!msg.contains("sk-abcdefghijklmnop"));
        assert!(msg.contains("***"));
        // 直接是字符串、HTML 错误页、缺字段都要容错。
        assert_eq!(
            error_message(r#"{"error":"model not found"}"#, "").as_deref(),
            Some("model not found")
        );
        assert!(error_message("<html>502 Bad Gateway</html>", "").is_none());
        assert!(error_message(r#"{"error":{}}"#, "").is_none());
        assert!(error_message("{", "").is_none());
    }

    #[test]
    fn error_message_redacts_the_key_echoed_by_the_gateway() {
        // gsk_/AIza/key= 这类真实 Key 没有 Bearer/sk- 外观，只有拿本次请求
        // 实际带上的 Key 做精确替换才能去掉。
        let json = r#"{"error":{"message":"invalid api key gsk_secret123"}}"#;
        let msg = error_message(json, "gsk_secret123").expect("error.message is present");
        assert!(!msg.contains("gsk_secret123"));
        assert!(msg.contains("***"));
    }

    #[test]
    fn malformed_bodies_yield_none_rather_than_panicking() {
        for json in [
            "",
            "<html><body>502 Bad Gateway</body></html>",
            "{",
            r#"{"choices":[]}"#,
            r#"{"choices":[{"message":{}}]}"#,
            r#"{"choices":[{"message":{"content":"   "}}]}"#,
            r#"{"error":{"message":"bad key"}}"#,
        ] {
            assert!(message_content(json).is_none(), "{json:?}");
        }
    }
}
