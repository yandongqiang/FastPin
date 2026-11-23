//! OpenAI-compatible translation through the user's own API.
//!
//! WinHTTP rather than an HTTP crate: this binary's size and cold start are the
//! point of the Rust port, and a full async stack buys nothing for one POST.

use std::ffi::c_void;
use std::path::PathBuf;

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

/// 本地模型缺失 / 推理管线未实现时的固定文案。设置页用同一句话提示用户。
const LOCAL_NOT_INSTALLED: &str = "本地翻译模型未安装：请将模型放入 models/translate 后使用";
const LOCAL_NOT_IMPLEMENTED: &str = "本地翻译推理管线尚未实现";

/// 本地服务的模型目录，按与 `ocr::resolve_models_dir` 相同的优先级解析：
/// 配置覆盖 → exe 旁的 `models/translate` → 仓库副本（`cargo run` 场景）。
/// 只读解析，不创建目录；下载时才由调用方建目录。
pub fn local_model_dir(service_id: &str) -> Result<PathBuf> {
    if service_id.trim().is_empty()
        || service_id.contains('/')
        || service_id.contains('\\')
        || service_id.split(['/', '\\']).any(|p| p == "..")
    {
        return Err(Error::new("本地服务 id 无效"));
    }
    if let Some(dir) = config::get().translation.local_models_dir.clone() {
        return Ok(dir.join("translate").join(service_id));
    }
    let exe = std::env::current_exe()?;
    let base = exe
        .parent()
        .ok_or_else(|| Error::new("executable has no parent directory"))?;
    let shipped = base.join("models").join("translate").join(service_id);
    if shipped.is_dir() {
        return Ok(shipped);
    }
    // 与 OCR 的仓库回退同理：`target\` 里没有打包目录，开发时指向仓库副本。
    Ok(PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../models/translate"
    ))
    .join(service_id))
}

/// 本地翻译入口（骨架）。
///
/// 仓库里没有任何本地翻译模型，也没有 tokenizer / seq2seq 推理管线，所以
/// 这里**从不返回译文**：模型不存在时报「未安装」，文件存在时报「管线尚未
/// 实现」。`translate()` 捕获错误后回退到现有 API 路径，绝不假装本地可用。
pub fn infer_local(_text: &str, service_id: &str) -> Result<String> {
    let model = local_model_dir(service_id)?.join("model.onnx");
    if !model.is_file() {
        return Err(Error::new(LOCAL_NOT_INSTALLED));
    }
    Err(Error::new(LOCAL_NOT_IMPLEMENTED))
}

/// Blocking. Call it off the UI thread.
pub fn translate(text: &str) -> Result<String> {
    let cfg = config::get().translation;
    // 本地优先：只在实际产出译文时才返回；失败写日志后落到下面的 API 路径。
    // 配置 / base_url 校验 / 凭据 / HTTP 闸门 / WinHTTP 逻辑全部保持不变。
    if cfg.default_service != "api" {
        match infer_local(text, &cfg.default_service) {
            Ok(translated) => return Ok(translated),
            Err(e) => crate::perf::log_app(&format!(
                "本地翻译失败，回退 API：{}（服务 {}）",
                e, cfg.default_service
            )),
        }
    }
    if cfg.base_url.trim().is_empty() {
        let path = config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        return Err(Error::new(format!(
            "翻译未配置：在 {path} 里填 translation.baseUrl"
        )));
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
        .ok_or_else(|| Error::new("Translation API returned an empty result."))
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

/// Closes on every path, including the error returns below.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
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
        headers.push_str(&format!("Authorization: Bearer {}\r\n", api_key.trim()));
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
        // One budget for the whole exchange, matching the C# provider, which
        // times the request rather than each socket phase.
        let ms = (timeout_seconds * 1000) as i32;
        WinHttpSetTimeouts(session.0, ms, ms, ms, ms)?;

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
        )?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())?;

        let mut status = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut size,
            std::ptr::null_mut(),
        )?;

        let payload = crate::net::read_response(request.0, crate::net::MAX_RESPONSE_BYTES)?;

        if !(200..300).contains(&status) {
            // The body often names the real cause (bad key, unknown model), but
            // it is the provider's text — keep it short and never log the key.
            return Err(Error::new(format!("Translation API failed ({status}).")));
        }
        String::from_utf8(payload)
            .map_err(|_| Error::new("Translation API returned invalid UTF-8."))
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

    #[test]
    fn infer_local_reports_missing_model_honestly() {
        // 这个 id 在任何候选目录里都不存在，因此必然命中「未安装」分支。
        let err = infer_local("你好", "__fastpin_test_missing_model__")
            .expect_err("骨架不能假装翻译成功");
        assert_eq!(err.to_string(), LOCAL_NOT_INSTALLED);
    }

    #[test]
    fn infer_local_rejects_path_traversal_ids() {
        for id in ["", "..", "a/b", "a\\b", "a/../b"] {
            assert!(infer_local("你好", id).is_err(), "{id:?}");
            assert!(local_model_dir(id).is_err(), "{id:?}");
        }
    }
}
