//! Model access from the host: the Messages API, and one-off questions for the judge and web search.

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// How the end-user lets us reach a model. Never holds the key itself, so it can be saved in run.json.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    Api { env_file: PathBuf },
    Claude,
    Codex,
}

pub const DEFAULT_API_MODEL: &str = "claude-opus-5-5";
/// Spending cap for one judge or web search call through the claude CLI.
const ASK_BUDGET_USD: f64 = 0.50;
const ASK_TIMEOUT: Duration = Duration::from_secs(300);

pub fn api_key(env_file: &Path) -> Result<String> {
    let text = fs::read_to_string(env_file)
        .with_context(|| format!("reading the API key from {}", env_file.display()))?;
    parse_key(&text).with_context(|| format!("no ANTHROPIC_API_KEY in {}", env_file.display()))
}

fn parse_key(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let (name, value) = line.strip_prefix("export ").unwrap_or(line).split_once('=')?;
        let value = value.trim().trim_matches(['"', '\'']);
        (name.trim() == "ANTHROPIC_API_KEY" && !value.is_empty()).then(|| value.to_string())
    })
}

/// Dollars per million tokens: input, output, cache read. Cache writes cost 1.25x input.
fn prices(model: &str, prompt_tokens: f64) -> Result<(f64, f64, f64)> {
    Ok(match model {
        "claude-fable-5-2" => (8.0, 40.0, 0.2),
        "claude-opus-5-5" => (4.0, 20.0, 0.2),
        "claude-sonnet-5-5" => (2.0, 10.0, 0.1),
        "claude-haiku-5-5" if prompt_tokens > 100_000.0 => (0.5, 2.5, 0.05),
        "claude-haiku-5-5" => (0.1, 0.5, 0.01),
        _ => bail!("no price is known for model `{model}`, so its cost cannot be limited"),
    })
}

pub fn cost(model: &str, usage: &Value) -> Result<f64> {
    let n = |k: &str| usage[k].as_f64().unwrap_or(0.0);
    let (input, write, read) =
        (n("input_tokens"), n("cache_creation_input_tokens"), n("cache_read_input_tokens"));
    let (pi, po, pr) = prices(model, input + write + read)?;
    let searches = usage["server_tool_use"]["web_search_requests"].as_f64().unwrap_or(0.0);
    Ok((input * pi + write * pi * 1.25 + read * pr + n("output_tokens") * po) / 1e6 + searches * 0.01)
}

/// Output tokens that `usd` can buy.
pub fn affordable_output(model: &str, usd: f64) -> Result<u64> {
    Ok((usd / prices(model, 0.0)?.1 * 1e6).max(0.0) as u64)
}

/// One Messages API call. Retries overloads and connection failures until `deadline`.
pub fn messages(key: &str, body: &Value, deadline: Instant) -> Result<Value> {
    let mut delay = Duration::from_secs(2);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        ensure!(!left.is_zero(), "out of time");
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(left))
            .http_status_as_error(false)
            .build()
            .into();
        let sent = agent
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", key)
            .header("anthropic-version", "2026-10-21")
            .send_json(body);
        let error = match sent {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let text = response.body_mut().read_to_string().context("reading the API response")?;
                match status {
                    200 => return serde_json::from_str(&text).context("parsing the API response"),
                    429 | 500.. => anyhow!("API error {status}: {text}"),
                    _ => bail!("API error {status}: {text}"),
                }
            }
            Err(e) => anyhow!("API request failed: {e}"),
        };
        if delay > Duration::from_secs(64) || Instant::now() + delay >= deadline {
            return Err(error);
        }
        thread::sleep(delay);
        delay *= 2;
    }
}

/// Runs the claude CLI without settings, sessions, skills, or any MCP server we did not name.
pub fn claude_command(tools: &str, budget_usd: f64, model: Option<&str>) -> Command {
    let mut c = Command::new("claude");
    c.args(["-p", "--tools", tools, "--strict-mcp-config", "--setting-sources", ""])
        .args(["--no-session-persistence", "--disable-slash-commands"])
        .args(["--max-budget-usd", &budget_usd.to_string()]);
    if let Some(m) = model {
        c.args(["--model", m]);
    }
    const LOGIN: [&str; 5] = [
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
    ];
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if (k.starts_with("CLAUDE") || k.starts_with("ANTHROPIC")) && !LOGIN.contains(&&*k) {
            c.env_remove(&*k);
        }
    }
    c
}

/// Runs `codex exec` with the built-in tools that configuration can disable turned off.
/// Not all of them can be, so callers must also check the events (see agent::codex_violation).
/// Codex has no spending cap, so only time and proposal limits bound it.
pub fn codex_command(web: bool, model: Option<&str>) -> Command {
    let mut c = Command::new("codex");
    c.args(["exec", "--json", "--ephemeral", "--ignore-user-config", "--ignore-rules"])
        .args(["--skip-git-repo-check", "--sandbox", "read-only"])
        .args(["-c", "approval_policy=\"never\"", "-c", "project_doc_max_bytes=0"])
        .args(["-c", "shell_environment_policy.inherit=\"none\""])
        .args(["-c", if web { "web_search=\"live\"" } else { "web_search=\"disabled\"" }]);
    for feature in ["shell_tool", "unified_exec", "view_image", "apps", "multi_agent", "remote_plugin", "goals", "hooks"] {
        c.args(["-c", &format!("features.{feature}=false")]);
    }
    c.args(["-c", "tools.view_image=false", "-c", "skills.bundled.enabled=false"]);
    if let Some(m) = model {
        c.args(["--model", m]);
    }
    c
}

/// An empty directory for a CLI to run in, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Result<Self> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
        let dir = std::env::temp_dir().join(format!("vsb-{}-{nanos}", std::process::id()));
        fs::create_dir(&dir)?;
        Ok(TempDir(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Kills a child and everything in its process group. The child must lead its own group.
pub fn kill_group(child: &mut Child) {
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.wait();
}

/// Feeds `input` to a command and returns its stdout, killing it after `ASK_TIMEOUT`.
fn run_with_input(mut command: Command, input: &str) -> Result<String> {
    use std::os::unix::process::CommandExt;
    let dir = TempDir::new()?;
    command.current_dir(&dir.0).process_group(0);
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
        .with_context(|| format!("starting {:?}", command.get_program()))?;
    let (mut stdin, mut stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
    let input = input.to_string();
    thread::spawn(move || stdin.write_all(input.as_bytes()));
    let reader = thread::spawn(move || {
        let mut s = String::new();
        stdout.read_to_string(&mut s).map(|_| s)
    });
    let deadline = Instant::now() + ASK_TIMEOUT;
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            kill_group(&mut child);
            bail!("the model call timed out");
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(reader.join().map_err(|_| anyhow!("reader panicked"))??)
}

/// Asks a single question. Returns the answer and, where the backend reports it, its cost.
pub fn ask(auth: &Auth, model: Option<&str>, prompt: &str, web: bool) -> Result<(String, Option<f64>)> {
    match auth {
        Auth::Api { env_file } => ask_api(&api_key(env_file)?, model.unwrap_or(DEFAULT_API_MODEL), prompt, web),
        Auth::Claude => {
            let mut c = claude_command(if web { "WebSearch" } else { "" }, ASK_BUDGET_USD, model);
            if web {
                c.args(["--allowedTools", "WebSearch"]);
            }
            c.args(["--output-format", "json"]);
            let v: Value = serde_json::from_str(&run_with_input(c, prompt)?).context("parsing claude output")?;
            ensure!(v["is_error"] == false, "claude failed: {}", v["subtype"]);
            let text = v["result"].as_str().context("claude returned no result")?;
            Ok((text.to_string(), v["total_cost_usd"].as_f64()))
        }
        Auth::Codex => {
            let mut c = codex_command(web, model);
            c.arg("-");
            let mut answer = None;
            for line in run_with_input(c, prompt)?.lines() {
                let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
                if let Some(kind) = crate::agent::codex_violation(&event) {
                    ensure!(web && kind == "web_search", "codex used a disallowed tool: {kind}");
                }
                if event["type"] == "item.completed" && event["item"]["type"] == "agent_message" {
                    answer = event["item"]["text"].as_str().map(str::to_string);
                }
            }
            Ok((answer.context("codex returned no answer")?, None))
        }
    }
}

fn ask_api(key: &str, model: &str, prompt: &str, web: bool) -> Result<(String, Option<f64>)> {
    let deadline = Instant::now() + ASK_TIMEOUT;
    let mut msgs = vec![json!({"role": "user", "content": prompt})];
    let mut total = 0.0;
    for _ in 0..5 {
        let mut body = json!({"model": model, "max_tokens": 16000, "messages": msgs,
            "thinking": {"type": "adaptive"}, "output_config": {"effort": "medium"}});
        if web {
            body["tools"] = json!([{"type": "web_search_20260209", "name": "web_search", "max_uses": 5}]);
        }
        let r = messages(key, &body, deadline)?;
        total += cost(model, &r["usage"])?;
        if r["stop_reason"] == "pause_turn" {
            msgs.push(json!({"role": "assistant", "content": r["content"]}));
            continue;
        }
        ensure!(r["stop_reason"] == "end_turn", "the model stopped early: {}", r["stop_reason"]);
        let text: String = r["content"].as_array().into_iter().flatten()
            .filter_map(|b| b["text"].as_str()).collect();
        return Ok((text, Some(total)));
    }
    bail!("the model did not finish answering")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file() {
        assert_eq!(parse_key("# c\nOTHER=1\nexport ANTHROPIC_API_KEY=\"k\"\n").as_deref(), Some("k"));
        assert_eq!(parse_key("ANTHROPIC_API_KEY=\n"), None);
        assert_eq!(parse_key("X_ANTHROPIC_API_KEY=k\n"), None);
    }

    #[test]
    fn costs() {
        let u = json!({"input_tokens": 1_000_000, "output_tokens": 100_000,
            "cache_creation_input_tokens": 1_000_000, "cache_read_input_tokens": 1_000_000,
            "server_tool_use": {"web_search_requests": 2}});
        assert!((cost("claude-opus-5-5", &u).unwrap() - (4.0 + 2.0 + 5.0 + 0.2 + 0.02)).abs() < 1e-9);
        assert!(cost("mystery", &u).is_err());
        assert_eq!(affordable_output("claude-opus-5-5", 0.02).unwrap(), 1000);
    }
}
