//! The MCP server that runs as root inside the container.

use crate::{
    checker,
    config::Experiment,
    docs,
    sandbox::{self, AGENT},
    source::{Index, Visibility},
    verus,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead, Read, Write},
    os::fd::AsRawFd,
    path::Path,
    time::Duration,
};

const EXPERIMENT: &str = "/experiment/experiment.json";
const STATE: &str = "/state/state.json";
const MAX_PROPOSAL: usize = 200_000;
const MAX_REPLY: usize = 30_000;

#[derive(Default, Serialize, Deserialize)]
struct State {
    proposals: u32,
    accepted: bool,
    last_output: Option<String>,
}

/// Reads the experiment from stdin and fills in the container's directories.
pub fn init() -> Result<()> {
    let mut text = String::new();
    io::stdin().read_to_string(&mut text)?;
    let experiment: Experiment = serde_json::from_str(&text)?;
    let index = Index::build(&experiment)?;
    fs::write(EXPERIMENT, &text)?;
    fs::write(STATE, serde_json::to_string(&State::default())?)?;
    for (name, text) in [
        ("lib.rs", verus::LIB),
        ("dependencies.rs", &index.stubs),
        ("task.rs", &experiment.task),
    ] {
        fs::write(Path::new("/problem").join(name), text)?;
    }
    let out = sandbox::run(
        AGENT,
        "cp /problem/*.rs /scratch/",
        "",
        Duration::from_secs(10),
    )?;
    ensure!(out.success(), "preparing scratch: {}", out.stderr);
    Ok(())
}

/// Serves MCP over stdio until stdin closes.
pub fn serve() -> Result<()> {
    // A reconnecting client waits for the previous server, so calls never overlap.
    let lock = fs::File::create("/state/lock")?;
    // SAFETY: the descriptor is open for the whole call.
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
        "locking state"
    );
    let experiment: Experiment = serde_json::from_str(&fs::read_to_string(EXPERIMENT)?)?;
    let index = Index::build(&experiment)?;
    let server = Server { experiment, index };

    for line in io::stdin().lock().lines() {
        let Ok(request) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        // Notifications have no id and get no reply.
        let Some(id) = request.get("id").filter(|id| !id.is_null()) else {
            continue;
        };
        let reply = match server.handle(&request) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": e.to_string()}}),
        };
        println!("{reply}");
        io::stdout().flush()?;
    }
    Ok(())
}

struct Server {
    experiment: Experiment,
    index: Index,
}

impl Server {
    fn handle(&self, request: &Value) -> Result<Value> {
        match request["method"].as_str().unwrap_or("") {
            "initialize" => Ok(json!({
                "protocolVersion": request["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "verus-sandbox", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let params = &request["params"];
                let name = params["name"].as_str().unwrap_or("");
                let (text, is_error) = match self.call(name, &params["arguments"]) {
                    Ok(text) => (text, false),
                    Err(e) => (format!("{e:#}"), true),
                };
                Ok(
                    json!({"content": [{"type": "text", "text": truncate(&text)}], "isError": is_error}),
                )
            }
            other => bail!("unknown method {other}"),
        }
    }

    fn call(&self, tool: &str, args: &Value) -> Result<String> {
        let arg = |name: &str| {
            args[name]
                .as_str()
                .with_context(|| format!("missing string argument `{name}`"))
        };
        match tool {
            "scan" => Ok(self.scan()),
            "read" => self.read(arg("symbol")?),
            "edit" => self.edit(arg("command")?),
            "docs" => docs::lookup(arg("query")?),
            "propose_solution" => self.propose(args["path"].as_str().unwrap_or("task.rs")),
            "view_verus_output" => Ok(load_state()?
                .last_output
                .unwrap_or("Nothing has been proposed yet.".into())),
            other => bail!("unknown tool {other}"),
        }
    }

    fn scan(&self) -> String {
        let visible = self
            .index
            .symbols
            .iter()
            .filter(|s| s.visibility != Visibility::Hidden);
        visible
            .map(|s| {
                let access = match s.visibility {
                    Visibility::Public => "full",
                    _ => "interface only",
                };
                let file = match s.file {
                    crate::source::File::Task => "task.rs",
                    crate::source::File::Dependencies => "dependencies.rs",
                };
                format!("{file}: {} {} ({access})\n", s.kind, s.name)
            })
            .collect()
    }

    fn read(&self, name: &str) -> Result<String> {
        self.index
            .symbols
            .iter()
            .find(|s| s.name == name && s.visibility != Visibility::Hidden)
            .map(|s| s.view.clone())
            // The same message whether the symbol is hidden or does not exist.
            .with_context(|| format!("no visible symbol `{name}`"))
    }

    fn edit(&self, command: &str) -> Result<String> {
        let timeout = Duration::from_secs(self.experiment.config.limits.tool_timeout_secs);
        let out = sandbox::run(AGENT, command, "", timeout)?;
        let status = match (out.timed_out, out.code) {
            (true, _) => format!("timed out after {}s", timeout.as_secs()),
            (_, Some(code)) => format!("exit code {code}"),
            (_, None) => "killed by a signal".into(),
        };
        // Truncate separately so that long stdout cannot hide stderr.
        let half = MAX_REPLY / 2 - 200;
        Ok(format!(
            "[{status}]\n{}{}",
            clip(&out.stdout, half),
            clip(&out.stderr, half)
        ))
    }

    fn propose(&self, path: &str) -> Result<String> {
        let mut state = load_state()?;
        ensure!(!state.accepted, "A solution has already been accepted.");
        let max = self.experiment.config.limits.max_proposals;
        ensure!(
            state.proposals < max,
            "The limit of {max} proposals has been reached."
        );

        // Read as the agent, so that a symlink cannot reach anything else.
        let quoted = path.replace('\'', r"'\''");
        let command = format!("head -c {} -- '{quoted}'", MAX_PROPOSAL + 1);
        let out = sandbox::run(AGENT, &command, "", Duration::from_secs(10))?;
        ensure!(out.success(), "cannot read {path}: {}", out.stderr);
        ensure!(
            out.stdout.len() <= MAX_PROPOSAL,
            "{path} is larger than {MAX_PROPOSAL} bytes"
        );
        let source = out.stdout;

        // Counted before checking, so that a crash cannot make a proposal free.
        state.proposals += 1;
        save_state(&state)?;
        let reasons = checker::check(&self.experiment, &self.index, &source);
        let (accepted, feedback) = if reasons.is_empty() {
            let v = verus::verify(&self.experiment, &self.index, &source)?;
            event(&json!({"event": "verus", "raw": v.raw}));
            (v.passed, v.feedback)
        } else {
            (
                false,
                format!("Rejected before verification:\n- {}", reasons.join("\n- ")),
            )
        };
        event(
            &json!({"event": "proposal", "n": state.proposals, "accepted": accepted,
            "source": source, "reasons": reasons}),
        );
        state.accepted = accepted;
        state.last_output = Some(feedback.clone());
        save_state(&state)?;

        if accepted {
            return Ok(format!("ACCEPTED. {feedback} You are done."));
        }
        let left = max - state.proposals;
        if left == 0 {
            event(&json!({"event": "proposal_limit"}));
        }
        Ok(format!("NOT ACCEPTED ({left} proposals left).\n{feedback}"))
    }
}

fn load_state() -> Result<State> {
    Ok(serde_json::from_str(&fs::read_to_string(STATE)?)?)
}

fn save_state(state: &State) -> Result<()> {
    Ok(fs::write(STATE, serde_json::to_string(state)?)?)
}

/// Sent to the host's log, not to the agent.
fn event(value: &Value) {
    eprintln!("{value}");
}

fn truncate(text: &str) -> String {
    clip(text, MAX_REPLY)
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated {} bytes]\n", &text[..end], text.len() - end)
}

fn tools() -> Value {
    let tool = |name: &str, description: &str, arg: &str, arg_description: &str, required: bool| {
        let (properties, required) = match arg {
            "" => (json!({}), json!([])),
            _ => (
                json!({arg: {"type": "string", "description": arg_description}}),
                if required { json!([arg]) } else { json!([]) },
            ),
        };
        json!({"name": name, "description": description, "inputSchema":
            {"type": "object", "properties": properties, "required": required}})
    };
    json!([
        tool(
            "scan",
            "List the symbols you can see, and whether you see their full source or only their interface.",
            "",
            "",
            false
        ),
        tool(
            "read",
            "Show a symbol: its full source, or its signature and specification.",
            "symbol",
            "A name from scan, such as `lemma` or `Type::method`",
            true
        ),
        tool(
            "edit",
            "Run a bash command in /scratch, your working directory. Use it to edit files and to run Verus: `verus --crate-type=lib lib.rs`. There is no network, and no process outlives the call.",
            "command",
            "The bash command",
            true
        ),
        tool(
            "docs",
            "Show documentation from vstd or the Rust standard library.",
            "query",
            "An identifier or path, such as `Seq::push`",
            true
        ),
        tool(
            "propose_solution",
            "Submit your completed task.rs. It may only fill the todo!() holes and add new spec, proof or exec functions inside verus!.",
            "path",
            "The file to submit. Defaults to task.rs in /scratch",
            false
        ),
        tool(
            "view_verus_output",
            "Show the result of your latest proposal again.",
            "",
            "",
            false
        )
    ])
}

#[cfg(test)]
mod tests {
    #[test]
    fn clips_on_char_boundaries() {
        assert!(super::clip("aé", 2).starts_with("a\n[truncated 2"));
        assert_eq!(super::clip("ok", 2), "ok");
    }
}
