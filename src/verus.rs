//! Trusted verification against the real dependencies, and redaction of its output.

use crate::{
    config::Experiment,
    sandbox::{self, VERIFIER},
    source::{File, Index, Visibility},
};
use anyhow::Result;
use serde_json::Value;
use std::time::Duration;

pub struct Verification {
    pub passed: bool,
    /// Unredacted, for the log only.
    pub raw: String,
    /// Safe to show the agent.
    pub feedback: String,
}

pub const LIB: &str = "mod dependencies;\nmod task;\n";

pub fn verify(experiment: &Experiment, index: &Index, proposal: &str) -> Result<Verification> {
    for (name, text) in [
        ("lib.rs", LIB),
        ("dependencies.rs", &experiment.dependencies),
        ("task.rs", proposal),
    ] {
        sandbox::write_file(VERIFIER, &format!("/verify/{name}"), text)?;
    }
    let out = sandbox::run(
        VERIFIER,
        "verus --crate-type=lib --no-cheating --output-json --num-threads 2 lib.rs -- --error-format=json",
        "",
        Duration::from_secs(experiment.config.limits.verus_timeout_secs),
    )?;
    let raw = format!("{}\n{}", out.stdout, out.stderr);
    if out.timed_out {
        return Ok(Verification {
            passed: false,
            raw,
            feedback: "Verus timed out.".into(),
        });
    }
    // Both must agree: a missing or malformed report is never a pass.
    let report: Value = serde_json::from_str(&out.stdout).unwrap_or(Value::Null);
    let results = &report["verification-results"];
    let passed = out.success() && results["success"] == true && results["errors"] == 0;
    let diagnostics = redact(index, &out.stderr);
    let feedback = match (passed, diagnostics.is_empty()) {
        (true, _) => format!("Verified: {} functions, 0 errors.", results["verified"]),
        (false, true) => "Verification failed with no diagnostics that can be shown.".into(),
        (false, false) => diagnostics,
    };
    Ok(Verification {
        passed,
        raw,
        feedback,
    })
}

/// Rebuilds diagnostics from rustc's JSON, quoting only the agent's own file.
fn redact(index: &Index, stderr: &str) -> String {
    let hidden = index.hidden_names();
    let mut result = String::new();
    for line in stderr.lines() {
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if d["level"] != "error" && d["level"] != "warning" {
            continue;
        }
        let spans = d["spans"].as_array().map_or(&[][..], Vec::as_slice);
        if spans.is_empty() && d["message"].as_str().unwrap_or("").starts_with("aborting") {
            continue;
        }
        if !spans.is_empty() && !spans.iter().any(|s| is_task(s)) {
            result += "error: [redacted: in dependencies.rs]\n\n";
            continue;
        }
        let message = redact_names(d["message"].as_str().unwrap_or(""), &hidden);
        result += &format!("{}: {message}\n", d["level"].as_str().unwrap_or("error"));
        for span in spans {
            result += &describe_span(index, span, &hidden);
        }
        for child in d["children"].as_array().into_iter().flatten() {
            if let Some(m) = child["message"].as_str() {
                result += &format!("  note: {}\n", redact_names(m, &hidden));
            }
        }
        result.push('\n');
    }
    result
}

fn is_task(span: &Value) -> bool {
    span["file_name"]
        .as_str()
        .is_some_and(|f| f.ends_with("task.rs"))
}

fn describe_span(index: &Index, span: &Value, hidden: &[String]) -> String {
    let file = span["file_name"].as_str().unwrap_or("");
    let line = span["line_start"].as_u64().unwrap_or(0) as usize;
    let label = redact_names(span["label"].as_str().unwrap_or(""), hidden);
    if is_task(span) {
        let text = span["text"][0]["text"].as_str().unwrap_or("").trim();
        return format!("  --> task.rs:{line}: {text}    {label}\n");
    }
    if file.ends_with("dependencies.rs") {
        let symbol = index.symbols.iter().find(|s| {
            s.file == File::Dependencies
                && s.visibility != Visibility::Hidden
                && (s.lines.0..=s.lines.1).contains(&line)
        });
        return match symbol {
            Some(s) => format!("  --> dependencies.rs: in `{}`    {label}\n", s.name),
            None => "  --> [redacted]\n".into(),
        };
    }
    // The standard library is public.
    format!("  --> {file}:{line}    {label}\n")
}

fn redact_names(text: &str, hidden: &[String]) -> String {
    let mut result = String::new();
    let mut word = String::new();
    for c in text.chars().chain(['\0']) {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
            continue;
        }
        result += if hidden.contains(&word) {
            "[redacted]"
        } else {
            &word
        };
        word.clear();
        if c != '\0' {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Symbol;
    use serde_json::json;

    fn index() -> Index {
        let symbol = |name: &str, visibility, lines| Symbol {
            name: name.into(),
            kind: "fn".into(),
            file: File::Dependencies,
            visibility,
            lines,
            view: String::new(),
        };
        Index {
            symbols: vec![
                symbol("shown", Visibility::Interface, (1, 5)),
                symbol("secret", Visibility::Hidden, (6, 9)),
            ],
            stubs: String::new(),
        }
    }

    fn span(file: &str, line: u32, text: &str) -> Value {
        json!({"file_name": file, "line_start": line, "label": "here", "text": [{"text": text}]})
    }

    #[test]
    fn redacts_whole_words_only() {
        assert_eq!(
            redact_names("`secret` secrets", &["secret".into()]),
            "`[redacted]` secrets"
        );
    }

    #[test]
    fn quotes_only_the_task() {
        let d = json!({"level": "error", "message": "precondition of secret not satisfied",
            "spans": [span("task.rs", 3, "lemma();"), span("dependencies.rs", 2, "PRIVATE1"),
                      span("dependencies.rs", 7, "PRIVATE2")], "children": []});
        let out = redact(&index(), &d.to_string());
        assert!(
            out.contains("lemma();") && out.contains("in `shown`"),
            "{out}"
        );
        assert!(!out.contains("PRIVATE") && !out.contains("secret"), "{out}");
    }

    #[test]
    fn hides_errors_wholly_in_dependencies() {
        let d = json!({"level": "error", "message": "m PRIVATE", "children": [],
            "spans": [span("dependencies.rs", 7, "PRIVATE")]});
        assert!(!redact(&index(), &d.to_string()).contains("PRIVATE"));
    }
}
