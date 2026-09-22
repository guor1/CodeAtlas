//! Anthropic Messages API client.
//!
//! Deliberately minimal and dependency-free over the system `curl`: the tool
//! makes a few dozen large requests per run, so connection reuse buys nothing
//! and avoiding a TLS stack keeps the build fast and portable.

use crate::store::Store;
use crate::util;
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    /// Give up on a single request after this many seconds.
    pub timeout_secs: u64,
    /// Total tokens this run may spend; 0 means unlimited.
    pub budget_tokens: usize,
}

impl Config {
    /// Read configuration from the environment, matching what Claude Code uses.
    pub fn from_env(model: Option<&str>) -> Result<Self> {
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
        let api_key = std::env::var("ANTHROPIC_AUTH_TOKEN")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .context(
                "未找到 API 凭证：请设置 ANTHROPIC_AUTH_TOKEN 或 ANTHROPIC_API_KEY",
            )?;
        let model = model
            .map(str::to_string)
            .or_else(|| std::env::var("ANTHROPIC_DEFAULT_SONNET_MODEL").ok())
            .unwrap_or_else(|| "claude-sonnet-5".to_string());
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            // A domain dossier for a large legacy area runs long: the state
            // table, rules and landmines together overflowed 8192 in practice,
            // and a truncated response is unparseable rather than merely short.
            max_tokens: 16384,
            model,
            timeout_secs: 300,
            budget_tokens: 0,
        })
    }
}

#[derive(Debug, Serialize)]
struct Request<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<Message<'a>>,
}

#[derive(Debug, Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    content: Vec<Block>,
    #[serde(default)]
    usage: Usage,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize, Default)]
struct Block {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: usize,
    #[serde(default)]
    pub output_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
    #[serde(rename = "type", default)]
    kind: String,
}

pub struct Completion {
    pub text: String,
    pub usage: Usage,
    /// True when the response was served from the local cache.
    pub cached: bool,
    /// Cache key, so a caller can persist the response once it has validated it.
    prompt_hash: String,
    request_json: String,
    model: String,
}

/// Send one prompt, consulting the cache first.
///
/// Caching is keyed on the prompt, model and token limit, so re-running `deepen`
/// after a crash costs nothing for the tasks that did not change. This matters: a
/// full deepen over a large project is expensive enough that an operator will not
/// retry it if failures are not free.
///
/// The response is **not** cached here. Callers must validate it first and then
/// call [`Completion::persist`]. Caching unconditionally would pin a truncated or
/// malformed response forever, so every later retry would replay the same failure
/// without ever reaching the API again.
pub fn complete(store: &Store, cfg: &Config, system: &str, user: &str) -> Result<Completion> {
    complete_keyed(store, cfg, system, user, system)
}

/// As [`complete`], but cached under `cache_system` instead of `system`.
///
/// Retries reword the system prompt to push the model back into the required
/// format. Keying on the reworded prompt would file the eventual good response
/// where no later run looks for it, so a retry stores its result under the
/// original prompt's key — the response answers the same question either way.
pub fn complete_keyed(
    store: &Store,
    cfg: &Config,
    system: &str,
    user: &str,
    cache_system: &str,
) -> Result<Completion> {
    // `max_tokens` is part of the key: raising the limit must invalidate the
    // truncated responses produced under the old one.
    let prompt_hash = util::digest_parts([
        cache_system,
        user,
        cfg.model.as_str(),
        &cfg.max_tokens.to_string(),
    ]);

    if let Some((text, input, output)) = cached(store, &prompt_hash, &cfg.model)? {
        return Ok(Completion {
            text,
            usage: Usage { input_tokens: input, output_tokens: output },
            cached: true,
            prompt_hash,
            request_json: String::new(),
            model: cfg.model.clone(),
        });
    }

    let body = serde_json::to_string(&Request {
        model: &cfg.model,
        max_tokens: cfg.max_tokens,
        system,
        messages: vec![Message { role: "user", content: user }],
    })?;

    let raw = post(cfg, &body)?;
    let parsed: Response = serde_json::from_str(&raw)
        .with_context(|| format!("解析响应失败：{}", util::truncate_chars(&raw, 400)))?;

    if let Some(e) = parsed.error {
        anyhow::bail!("API 返回错误 [{}]：{}", e.kind, e.message);
    }
    let text = parsed
        .content
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    anyhow::ensure!(
        !text.trim().is_empty(),
        "API 返回空内容（stop_reason={:?}）",
        parsed.stop_reason
    );
    // A truncated response yields invalid JSON downstream; fail here where the
    // cause is still obvious.
    if parsed.stop_reason.as_deref() == Some("max_tokens") {
        anyhow::bail!(
            "响应在 max_tokens={} 处被截断，请减小任务粒度或提高 max_tokens",
            cfg.max_tokens
        );
    }

    Ok(Completion {
        text,
        usage: parsed.usage,
        cached: false,
        prompt_hash,
        request_json: body,
        model: cfg.model.clone(),
    })
}

impl Completion {
    /// Store this response for reuse. Call only after the response has been
    /// validated, so a malformed one is never replayed from cache.
    pub fn persist(&self, store: &Store) -> Result<()> {
        if self.cached {
            return Ok(());
        }
        store.conn.execute(
            "INSERT OR REPLACE INTO llm_cache(prompt_hash, model, request_json, response_json,
                                              input_tokens, output_tokens, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                self.prompt_hash,
                self.model,
                self.request_json,
                self.text,
                self.usage.input_tokens as i64,
                self.usage.output_tokens as i64,
                util::now_iso(),
            ],
        )?;
        Ok(())
    }
}

fn cached(store: &Store, hash: &str, model: &str) -> Result<Option<(String, usize, usize)>> {
    let row = store
        .conn
        .query_row(
            "SELECT response_json, input_tokens, output_tokens FROM llm_cache
             WHERE prompt_hash = ?1 AND model = ?2",
            params![hash, model],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0) as usize,
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0) as usize,
                ))
            },
        )
        .optional()?;
    Ok(row)
}

/// POST a JSON body to the Messages endpoint via `curl`.
///
/// The API key goes into a curl config file rather than a `-H` argument: command
/// lines are world-readable through `/proc`, and leaking a credential to every
/// process on a shared build machine is not an acceptable default.
fn post(cfg: &Config, body: &str) -> Result<String> {
    let dir = tempdir()?;
    let config_path = dir.path().join("curl.conf");
    let body_path = dir.path().join("body.json");

    std::fs::write(&body_path, body).context("写入请求体失败")?;
    // `header = "..."` in a curl config escapes only backslash and quote.
    let escaped_key = cfg.api_key.replace('\\', "\\\\").replace('"', "\\\"");
    std::fs::write(
        &config_path,
        format!("header = \"x-api-key: {escaped_key}\"\n"),
    )
    .context("写入 curl 配置失败")?;
    restrict_permissions(&config_path)?;

    let out = Command::new("curl")
        .args([
            "-sS",
            "--fail-with-body",
            "--max-time",
            &cfg.timeout_secs.to_string(),
            "-X",
            "POST",
            &format!("{}/v1/messages", cfg.base_url),
            "-H",
            "content-type: application/json",
            "-H",
            "anthropic-version: 2023-06-01",
            "--config",
            &config_path.to_string_lossy(),
            "--data-binary",
            &format!("@{}", body_path.to_string_lossy()),
        ])
        .stdin(Stdio::null())
        .output()
        .context("启动 curl 失败，请确认已安装 curl")?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // The body is included even on failure, and usually explains why.
        anyhow::bail!(
            "请求失败（{}）：{}{}",
            out.status,
            util::truncate_chars(stdout.trim(), 400),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(" / {}", stderr.trim())
            }
        );
    }
    Ok(stdout)
}

/// A private temporary directory that is removed when dropped.
fn tempdir() -> Result<TempDir> {
    let base = std::env::temp_dir();
    let unique = format!(
        "codeatlas-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let path = base.join(unique);
    std::fs::create_dir_all(&path).context("创建临时目录失败")?;
    restrict_permissions(&path)?;
    Ok(TempDir { path })
}

struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: the credential file must not outlive the request, but a
        // failure here should not mask the request's own outcome.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)?;
    let mut perms = meta.permissions();
    perms.set_mode(if meta.is_dir() { 0o700 } else { 0o600 });
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}
