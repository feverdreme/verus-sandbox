//! Drives an agent whose only tools are the sandbox's MCP tools.

use crate::{llm, proxy};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, channel},
    },
    thread,
    time::{Duration, Instant},
};

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const TOOLS: [&str; 7] =
    ["scan", "read", "edit", "docs", "propose_solution", "view_verus_output", "web_search"];
/// How long the agent gets to finish after its solution is accepted.
const GRACE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Solved,
    GaveUp,
    Timeout,
    CostLimit,
    ProposalLimit,
    Crashed,
}

pub struct Outcome {
    pub status: Status,
    pub detail: Option<String>,
    pub cost_usd: Option<f64>,
    /// The agent had access it should not have had, so even an accepted solution does not count.
    pub void: bool,
}

pub struct Session<'a> {
    pub dir: &'a Path,
    pub description: &'a str,
    pub deadline: Instant,
    /// What the agent itself may spend, after setting aside the web search share.
    pub budget_usd: f64,
    pub model: Option<&'a str>,
    pub tool_timeout: Duration,
    solved_at: Option<Instant>,
}

impl<'a> Session<'a> {
    pub fn new(dir: &'a Path, config: &'a crate::config::Config, model: Option<&'a str>) -> Self {
        let l = &config.limits;
        let search = if config.web_search { proxy::SEARCH_SHARE } else { 0.0 };
        Session {
            dir,
            description: &config.description,
            deadline: Instant::now() + Duration::from_secs(l.experiment_timeout_secs),
            budget_usd: l.max_cost_usd * (1.0 - search),
            model,
            tool_timeout: Duration::from_secs(l.tool_timeout_secs.max(l.verus_timeout_secs).max(300) + 60),
            solved_at: None,
        }
    }

    /// Why the agent must stop now, if it must.
    fn stop(&mut self) -> Option<(Status, &'static str)> {
        if self.dir.join("solution.rs").exists() {
            let at = *self.solved_at.get_or_insert_with(Instant::now);
            if at.elapsed() > GRACE {
                return Some((Status::Solved, "stopped after its solution was accepted"));
            }
        }
        if INTERRUPTED.load(Ordering::Relaxed) {
            Some((Status::Crashed, "interrupted"))
        } else if self.dir.join("ended").exists() {
            Some((Status::ProposalLimit, "used every proposal"))
        } else if Instant::now() >= self.deadline {
            Some((Status::Timeout, "experiment timeout"))
        } else {
            None
        }
    }

    fn system_prompt(&self) -> String {
        format!(
            "You are solving a Verus verification task. Your only tools are those of the vsb MCP server.\n\n\
             Task: {}\n\n\
             The problem is in /problem (read-only): task.rs, with holes marked `todo!(\"...\")`, and \
             dependencies.rs, which may show only the interface of some symbols. Work in /scratch, \
             using `edit` to run shell commands, including `verus`. There is no network access.\n\n\
             - `scan` lists symbols and `read` shows one.\n\
             - `docs` searches the Verus and Rust standard library documentation.\n\
             - `propose_solution` submits a complete task.rs. Proposals are limited, so check your work \
             with verus in /scratch first. `view_verus_output` shows the latest verification feedback.\n\n\
             A proposal may fill the holes and add new spec, proof and exec functions inside verus!. It \
             may not change anything else, or add imports, types, traits, impls or macros. assume, \
             admit, axioms, external_body and unsafe are rejected. Stop once a proposal is accepted.",
            self.description
        )
    }
}

pub fn run(auth: &llm::Auth, mut s: Session) -> Result<Outcome> {
    match auth {
        llm::Auth::Claude => claude(&mut s),
        llm::Auth::Codex => codex(&mut s),
        llm::Auth::Api { env_file } => api(&llm::api_key(env_file)?, &mut s),
    }
}

fn mcp_command(dir: &Path) -> Result<(PathBuf, [String; 2])> {
    Ok((std::env::current_exe()?, ["proxy".into(), dir.display().to_string()]))
}

/// Spawns a child in its own process group and streams its stdout lines.
fn spawn(command: &mut Command, stderr: File) -> Result<(Child, Receiver<String>)> {
    let mut child = command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("starting {:?}", command.get_program()))?;
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = channel();
    thread::spawn(move || stdout.lines().map_while(Result::ok).try_for_each(|l| tx.send(l)));
    Ok((child, rx))
}

/// Runs a CLI agent, passing each JSON event to `on_event`, which may end the run.
fn run_cli(
    s: &mut Session,
    mut command: Command,
    mut on_event: impl FnMut(&Value) -> Option<(Status, String)>,
) -> Result<Option<(Status, String)>> {
    let cwd = llm::TempDir::new()?;
    command.current_dir(&cwd.0);
    let mut transcript = File::create(s.dir.join("agent.jsonl"))?;
    let (mut child, rx) = spawn(&mut command, File::create(s.dir.join("agent.stderr"))?)?;
    let prompt = "Solve the task.";
    let fed = child.stdin.take().unwrap().write_all(prompt.as_bytes());
    let end = loop {
        if let Err(e) = &fed {
            break Some((Status::Crashed, format!("could not send the prompt: {e}")));
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                if let Err(e) = writeln!(transcript, "{line}") {
                    break Some((Status::Crashed, format!("writing the transcript: {e}")));
                }
                if let Ok(event) = serde_json::from_str(&line)
                    && let Some(end) = on_event(&event)
                {
                    break Some(end);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break None,
        }
        if let Some((status, why)) = s.stop() {
            break Some((status, why.to_string()));
        }
    };
    if end.is_some() {
        llm::kill_group(&mut child);
        return Ok(end);
    }
    // Its stdout is closed, so it is exiting.
    let exit = child.wait()?;
    Ok((!exit.success()).then(|| (Status::Crashed, format!("the agent exited with {exit}"))))
}

fn claude(s: &mut Session) -> Result<Outcome> {
    let (exe, args) = mcp_command(s.dir)?;
    let mut c = llm::claude_command("", s.budget_usd, s.model);
    let config = json!({"mcpServers": {"vsb": {"type": "stdio", "command": exe, "args": args}}});
    let allowed: Vec<_> = TOOLS.iter().map(|t| format!("mcp__vsb__{t}")).collect();
    c.args(["--mcp-config", &config.to_string(), "--allowedTools", &allowed.join(",")])
        .args(["--system-prompt", &s.system_prompt()])
        .args(["--output-format", "stream-json", "--verbose"])
        .env("MCP_TOOL_TIMEOUT", s.tool_timeout.as_millis().to_string());
    let (mut cost, mut result, mut void, mut checked) = (None, None, false, false);
    let end = run_cli(s, c, |event| {
        checked |= event["subtype"] == "init";
        if let Some(t) = claude_violation(event) {
            void = true;
            return Some((Status::Crashed, format!("claude had a tool it should not have: {t}")));
        }
        if event["type"] == "result" {
            cost = event["total_cost_usd"].as_f64();
            result = event["subtype"].as_str().map(str::to_string);
            if event["is_error"] == true && result.as_deref() == Some("success") {
                result = event["result"].as_str().map(str::to_string);
            }
        }
        None
    })?;
    let (status, detail) = match (result.as_deref(), end) {
        // A budget stop also makes the CLI exit with an error.
        (Some("error_max_budget_usd"), _) => (Status::CostLimit, None),
        (_, Some((status, why))) => (status, Some(why)),
        (Some("success"), None) => (Status::GaveUp, None),
        (other, None) => (Status::Crashed, Some(other.unwrap_or("no result from claude").into())),
    };
    // Without an init event, the tools were never checked.
    Ok(Outcome { status, detail, cost_usd: cost, void: void || !checked })
}

/// A tool or server in claude's init event, or a tool it used, that is not ours.
fn claude_violation(event: &Value) -> Option<String> {
    let ours = |t: &str| t.strip_prefix("mcp__vsb__").is_some_and(|t| TOOLS.contains(&t));
    if event["type"] == "system" && event["subtype"] == "init" {
        let Some(tools) = event["tools"].as_array() else { return Some("unknown tool list".into()) };
        return tools.iter().map(|t| t.as_str().unwrap_or("?")).find(|t| !ours(t)).map(str::to_string);
    }
    if event["type"] == "assistant" {
        return event["message"]["content"].as_array()?.iter()
            .filter(|b| b["type"] == "tool_use" || b["type"] == "server_tool_use")
            .map(|b| b["name"].as_str().unwrap_or("?"))
            .find(|t| !ours(t))
            .map(str::to_string);
    }
    None
}

/// Codex cannot turn every built-in tool off, so any item that is not known to be harmless ends
/// the run. The event can arrive after the tool has run, which is why the run is discarded.
pub fn codex_violation(event: &Value) -> Option<String> {
    if !event["type"].as_str()?.starts_with("item.") {
        return None;
    }
    let item = &event["item"];
    match item["type"].as_str().unwrap_or("unknown item") {
        "agent_message" | "reasoning" | "todo_list" | "plan_update" | "error" => None,
        "mcp_tool_call" if item["server"] == "vsb" => None,
        other => Some(other.to_string()),
    }
}

fn codex(s: &mut Session) -> Result<Outcome> {
    eprintln!("warning: codex has no spending cap, so max_cost_usd is not enforced for the agent");
    let (exe, args) = mcp_command(s.dir)?;
    let mut c = llm::codex_command(false, s.model);
    // JSON strings and arrays are valid TOML.
    for (key, value) in [
        ("mcp_servers.vsb.command", json!(exe)),
        ("mcp_servers.vsb.args", json!(args)),
        ("mcp_servers.vsb.enabled_tools", json!(TOOLS)),
        ("mcp_servers.vsb.required", json!(true)),
        ("mcp_servers.vsb.default_tools_approval_mode", json!("approve")),
        ("mcp_servers.vsb.tool_timeout_sec", json!(s.tool_timeout.as_secs())),
        ("developer_instructions", json!(s.system_prompt())),
    ] {
        c.args(["-c", &format!("{key}={value}")]);
    }
    c.arg("-");
    let (mut completed, mut void) = (false, false);
    let end = run_cli(s, c, |event| {
        completed |= event["type"] == "turn.completed";
        let kind = codex_violation(event)?;
        void = true;
        Some((Status::Crashed, format!("codex used a tool outside the sandbox: {kind}")))
    })?;
    let (status, detail) = match end {
        Some((status, why)) => (status, Some(why)),
        None if completed => (Status::GaveUp, None),
        None => (Status::Crashed, Some("codex ended without completing its turn".into())),
    };
    Ok(Outcome { status, detail, cost_usd: None, void })
}

/// A minimal MCP client over the proxy.
struct Mcp {
    child: Child,
    rx: Receiver<String>,
    next_id: u64,
}

impl Mcp {
    fn start(s: &mut Session) -> Result<Mcp> {
        let (exe, args) = mcp_command(s.dir)?;
        let stderr = File::create(s.dir.join("agent.stderr"))?;
        let (child, rx) = spawn(Command::new(exe).args(args), stderr)?;
        let mut mcp = Mcp { child, rx, next_id: 0 };
        let init = json!({"protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "vsb", "version": "0.1.0"}});
        mcp.request(s, "initialize", init)?;
        mcp.write(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        Ok(mcp)
    }

    fn write(&mut self, message: &Value) -> Result<()> {
        writeln!(self.child.stdin.as_mut().unwrap(), "{message}").context("writing to the MCP server")
    }

    fn request(&mut self, s: &mut Session, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = s.deadline.min(Instant::now() + s.tool_timeout);
        loop {
            match self.rx.recv_timeout(Duration::from_millis(250)) {
                Ok(line) => {
                    let mut v: Value = serde_json::from_str(&line).context("parsing an MCP message")?;
                    if v["id"] == id {
                        if !v["error"].is_null() {
                            bail!("MCP error: {}", v["error"]);
                        }
                        return Ok(v["result"].take());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => bail!("the MCP server exited"),
            }
            if INTERRUPTED.load(Ordering::Relaxed) || Instant::now() > deadline {
                bail!("{method} did not finish in time");
            }
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        llm::kill_group(&mut self.child);
    }
}

fn api(key: &str, s: &mut Session) -> Result<Outcome> {
    let mut spent = 0.0;
    let (status, detail) = match api_loop(key, s, &mut spent) {
        Ok(end) => end,
        // Running out of time mid-call surfaces as an error.
        Err(e) => match s.stop() {
            Some((status, why)) => (status, Some(why.into())),
            None => (Status::Crashed, Some(format!("{e:#}"))),
        },
    };
    Ok(Outcome { status, detail, cost_usd: Some(spent), void: false })
}

fn api_loop(key: &str, s: &mut Session, spent: &mut f64) -> Result<(Status, Option<String>)> {
    let model = s.model.unwrap_or(llm::DEFAULT_API_MODEL).to_string();
    llm::cost(&model, &json!({}))?; // Fail before spending anything if the model has no price.
    let mut mcp = Mcp::start(s)?;
    let listed = mcp.request(s, "tools/list", json!({}))?;
    let tools: Vec<Value> = listed["tools"].as_array().context("no tools listed")?.iter()
        .map(|t| json!({"name": t["name"], "description": t["description"], "input_schema": t["inputSchema"]}))
        .collect();
    let system = s.system_prompt();
    // Append-only: thinking blocks are only valid in the conversation that produced them.
    let mut msgs = vec![json!({"role": "user", "content": "Solve the task."})];
    let mut transcript = OpenOptions::new().create(true).append(true).open(s.dir.join("agent.jsonl"))?;
    let mut truncated = 0;
    loop {
        if let Some((status, why)) = s.stop() {
            return Ok((status, Some(why.into())));
        }
        // Bounds the overshoot past the limit to one request's input.
        let max_tokens = llm::affordable_output(&model, s.budget_usd - *spent)?.min(16000);
        if max_tokens < 1024 {
            return Ok((Status::CostLimit, None));
        }
        let body = json!({"model": model, "max_tokens": max_tokens, "system": system, "tools": tools,
            "messages": msgs, "thinking": {"type": "adaptive"}, "output_config": {"effort": "high"},
            "cache_control": {"type": "ephemeral"}});
        let r = llm::messages(key, &body, s.deadline)?;
        *spent += llm::cost(&model, &r["usage"])?;
        writeln!(transcript, "{r}")?;
        msgs.push(json!({"role": "assistant", "content": r["content"]}));
        let calls: Vec<Value> = r["content"].as_array().into_iter().flatten()
            .filter(|b| b["type"] == "tool_use").cloned().collect();
        let reason = r["stop_reason"].as_str().unwrap_or("");
        let reply = match reason {
            "end_turn" => return Ok((Status::GaveUp, None)),
            "refusal" => return Ok((Status::Crashed, Some("the model refused".into()))),
            "pause_turn" => continue,
            "tool_use" => {
                truncated = 0;
                let mut results = vec![];
                for call in &calls {
                    results.push(call_tool(&mut mcp, s, call)?);
                }
                json!(results)
            }
            "max_tokens" => {
                truncated += 1;
                if truncated == 3 {
                    return Ok((Status::Crashed, Some("the model kept exceeding the output limit".into())));
                }
                // A call cut off by the limit must not run, but still needs a result.
                let mut blocks: Vec<Value> = calls.iter()
                    .map(|c| json!({"type": "tool_result", "tool_use_id": c["id"], "is_error": true,
                        "content": "Not run: the output limit was reached."}))
                    .collect();
                blocks.push(json!({"type": "text", "text": "The output limit was reached. Continue, more briefly."}));
                json!(blocks)
            }
            other => return Ok((Status::Crashed, Some(format!("unexpected stop reason `{other}`")))),
        };
        msgs.push(json!({"role": "user", "content": reply}));
        writeln!(transcript, "{}", msgs.last().unwrap())?;
    }
}

fn call_tool(mcp: &mut Mcp, s: &mut Session, call: &Value) -> Result<Value> {
    let params = json!({"name": call["name"], "arguments": call["input"]});
    let r = mcp.request(s, "tools/call", params)?;
    let text: Vec<&str> =
        r["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect();
    Ok(json!({"type": "tool_result", "tool_use_id": call["id"],
        "content": text.join("\n"), "is_error": r["isError"] == true}))
}

/// The end of a file, for showing to the judge.
pub fn tail(path: &Path, bytes: usize) -> String {
    let data = fs::read(path).unwrap_or_default();
    String::from_utf8_lossy(&data[data.len().saturating_sub(bytes)..]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_tools() {
        let init = |t: Value| json!({"type": "system", "subtype": "init", "tools": t});
        assert_eq!(claude_violation(&init(json!(["mcp__vsb__scan"]))), None);
        assert_eq!(claude_violation(&init(json!(["mcp__vsb__scan", "Bash"]))).as_deref(), Some("Bash"));
        assert!(claude_violation(&init(Value::Null)).is_some());
        let used = |n| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": n}]}});
        assert_eq!(claude_violation(&used("mcp__vsb__edit")), None);
        assert_eq!(claude_violation(&used("mcp__other__edit")).as_deref(), Some("mcp__other__edit"));
    }

    #[test]
    fn codex_tools() {
        let item = |t| json!({"type": "item.started", "item": {"type": t, "server": "vsb"}});
        assert_eq!(codex_violation(&item("mcp_tool_call")), None);
        assert_eq!(codex_violation(&item("reasoning")), None);
        for t in ["command_execution", "file_change", "web_search", "something_new"] {
            assert_eq!(codex_violation(&item(t)).as_deref(), Some(t));
        }
        assert_eq!(codex_violation(&json!({"type": "turn.completed"})), None);
        let other = json!({"type": "item.started", "item": {"type": "mcp_tool_call", "server": "x"}});
        assert!(codex_violation(&other).is_some());
    }
}
