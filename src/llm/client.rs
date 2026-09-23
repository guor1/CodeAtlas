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
use std::collections::BTreeMap;
use std::path::PathBuf;
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
    /// Read configuration the way Claude Code resolves it: the process
    /// environment first, then the `env` block of `~/.claude/settings.json`.
    ///
    /// The file fallback is what makes `catlas deepen` work from a plain shell.
    /// Behind a company gateway the credential and base URL typically live only
    /// in that file — Claude Code injects them into the processes it spawns, so
    /// without this a command that succeeded inside Claude Code would fail when
    /// run by hand, for no visible reason.
    pub fn from_env(model: Option<&str>) -> Result<Self> {
        let settings = settings_env();
        let var = |key: &str| pick(key, &settings);

        let base_url = var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());
        let api_key = var("ANTHROPIC_AUTH_TOKEN")
            .or_else(|| var("ANTHROPIC_API_KEY"))
            .with_context(|| {
                format!(
                    "未找到 API 凭证：请设置环境变量 ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY，\
                     或在 {} 的 env 块中配置",
                    settings_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "~/.claude/settings.json".into())
                )
            })?;
        let model = model
            .map(str::to_string)
            .or_else(|| var("ANTHROPIC_DEFAULT_SONNET_MODEL"))
            .unwrap_or_else(|| "claude-sonnet-5".to_string());
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            // A domain dossier for a large legacy area runs long: the state
            // table, rules and landmines together overflowed 8192 in practice,
            // and a truncated response is unparseable rather than merely short.
            max_tokens: 16384,
            model,
            timeout_secs: var("API_TIMEOUT_MS")
                .and_then(|ms| ms.parse::<u64>().ok())
                .map(|ms| (ms / 1000).max(1))
                .unwrap_or(300),
            budget_tokens: 0,
        })
    }
}

/// One setting, process environment winning over the settings file.
///
/// An empty environment variable counts as unset: exporting `FOO=` to clear a
/// value should fall through to the file, not select the empty string.
fn pick(key: &str, settings: &BTreeMap<String, String>) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| settings.get(key).cloned())
}

/// `~/.claude/settings.json`, honouring `CLAUDE_CONFIG_DIR` as Claude Code does.
fn settings_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("settings.json"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".claude").join("settings.json"))
}

/// The `env` block of the settings file, empty when it cannot be used.
///
/// Best-effort by design: a missing, unreadable or malformed file means "no
/// fallback available", never a hard error — the process environment may well
/// carry everything needed, and failing the run over an unrelated syntax error
/// in someone's editor config would be gratuitous.
fn settings_env() -> BTreeMap<String, String> {
    let Some(path) = settings_path() else { return BTreeMap::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return BTreeMap::new() };
    env_block(&text)
}

/// Parse the `env` object out of settings JSON, keeping only string values.
fn env_block(text: &str) -> BTreeMap<String, String> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return BTreeMap::new();
    };
    json.get("env")
        .and_then(serde_json::Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_block_reads_string_values_only() {
        let text = r#"{
            "env": {
                "ANTHROPIC_BASE_URL": "https://gateway.example.com",
                "CLAUDE_CODE_USE_BEDROCK": "0",
                "nested": { "ignored": true },
                "numeric": 42
            },
            "theme": "dark"
        }"#;
        let env = env_block(text);
        assert_eq!(env.get("ANTHROPIC_BASE_URL").unwrap(), "https://gateway.example.com");
        assert_eq!(env.get("CLAUDE_CODE_USE_BEDROCK").unwrap(), "0");
        assert!(!env.contains_key("nested"));
        assert!(!env.contains_key("numeric"));
    }

    #[test]
    fn env_block_tolerates_junk_and_absence() {
        assert!(env_block("not json at all").is_empty());
        assert!(env_block("{}").is_empty());
        assert!(env_block(r#"{"env": "not an object"}"#).is_empty());
    }

    #[test]
    fn settings_fill_in_what_the_environment_lacks() {
        let settings: BTreeMap<String, String> =
            [("CATLAS_TEST_ONLY_IN_FILE".to_string(), "from-file".to_string())]
                .into_iter()
                .collect();
        assert_eq!(pick("CATLAS_TEST_ONLY_IN_FILE", &settings).unwrap(), "from-file");
        assert!(pick("CATLAS_TEST_NOWHERE", &settings).is_none());
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        // A unique key keeps this from colliding with other tests in the
        // process; `set_var` is unsafe in edition 2024 because it races with
        // concurrent readers of the environment.
        let key = "CATLAS_TEST_PRECEDENCE";
        let settings: BTreeMap<String, String> =
            [(key.to_string(), "from-file".to_string())].into_iter().collect();
        unsafe { std::env::set_var(key, "from-env") };
        assert_eq!(pick(key, &settings).unwrap(), "from-env");

        // An empty variable means "unset", so the file still applies.
        unsafe { std::env::set_var(key, "") };
        assert_eq!(pick(key, &settings).unwrap(), "from-file");
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn settings_path_follows_claude_config_dir() {
        // Same caveat as above: this mutates process-wide state.
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", dir.path()) };
        assert_eq!(settings_path().unwrap(), dir.path().join("settings.json"));
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }
}
