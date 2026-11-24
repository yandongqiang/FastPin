//! OpenAI-compatible translation through the user's own API.
//!
//! WinHTTP rather than an HTTP crate: this binary's size and cold start are the
//! point of the Rust port, and a full async stack buys nothing for one POST.

use std::ffi::c_void;

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Security::Credentials::{PasswordCredential, PasswordVault};
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

/// Whether the configured Base URL uses plaintext HTTP. Used by both the
/// runtime guard and the settings warning banner so the two cannot disagree.
pub fn is_insecure_http(base_url: &str) -> bool {
    base_url.trim().to_ascii_lowercase().starts_with("http://")
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
    if let Ok(existing) = vault.Retrieve(&resource, &account) {
        vault.Remove(&existing)?;
    }
    let key = key.trim();
    if key.is_empty() {
        return Ok(());
    }
    let cred =
        PasswordCredential::CreatePasswordCredential(&resource, &account, &HSTRING::from(key))?;
    vault.Add(&cred)?;
    Ok(())
}

/// Copies a key stored under the legacy credential resource to
/// `FastPin.Translation`, then drops the old entry. A missing legacy entry is
/// the normal case (fresh install), not an error. Best-effort: every failure is
/// reported as a note for the caller to log and never blocks startup.
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
    if old.RetrievePassword().is_err() {
        return vec!["vault: legacy translation key unreadable".into()];
    }
    let Ok(password) = old.Password() else {
        return vec!["vault: legacy translation key unreadable".into()];
    };
    // Never overwrite a key the new build already saved.
    let already = vault.Retrieve(&current, &account).is_ok();
    if !already {
        let cred = match PasswordCredential::CreatePasswordCredential(&current, &account, &password)
        {
            Ok(c) => c,
            Err(e) => return vec![format!("vault: could not create FastPin entry ({e})")],
        };
        if let Err(e) = vault.Add(&cred) {
            return vec![format!("vault: could not save FastPin entry ({e})")];
        }
    }
    match vault.Remove(&old) {
        Ok(()) if already => vec!["vault: duplicate legacy translation key removed".into()],
        Ok(()) => vec!["vault: moved translation key to FastPin.Translation".into()],
        Err(_) => vec!["vault: FastPin key written, legacy entry not removed".into()],
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
/// 非 JSON / HTML 错误页一律忽略，文本经去敏、截断后才给用户看。
fn error_message(json: &str) -> Option<String> {
    if json.trim_start().starts_with('<') {
        return None;
    }
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let msg = root
        .get("error")
        .and_then(|e| e.get("message"))
        .or_else(|| root.get("error"))
        .and_then(|v| v.as_str())?;
    let msg = crate::net::redact_api_message(msg.trim());
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
    unsafe { WinHttpCrackUrl(&wide, 0, &mut parts) }.map_err(|_| {
        Error::new(format!(
            "Translation Base URL is not a valid URL: {}",
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
            return Err(Error::new("WinHttpOpen failed"));
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
            return Err(Error::new("WinHttpConnect failed"));
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
            return Err(Error::new("WinHttpOpenRequest failed"));
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
            let detail = error_message(&detail)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!is_insecure_http(""));
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
        let msg = error_message(json).expect("error.message is present");
        assert!(msg.contains("Incorrect API key"));
        assert!(!msg.contains("sk-abcdefghijklmnop"));
        assert!(msg.contains("***"));
        // 直接是字符串、HTML 错误页、缺字段都要容错。
        assert_eq!(
            error_message(r#"{"error":"model not found"}"#).as_deref(),
            Some("model not found")
        );
        assert!(error_message("<html>502 Bad Gateway</html>").is_none());
        assert!(error_message(r#"{"error":{}}"#).is_none());
        assert!(error_message("{").is_none());
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
