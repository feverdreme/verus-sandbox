//! Looks up definitions and their doc comments in vstd and the Rust standard library.

use anyhow::{Result, ensure};
use std::{fs, path::Path, process::Command};

const MAX_RESULTS: usize = 12;
const MAX_LINES: usize = 25;

pub fn lookup(query: &str) -> Result<String> {
    let name = query
        .trim()
        .trim_end_matches('!')
        .rsplit("::")
        .next()
        .unwrap_or("");
    let valid = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    ensure!(
        valid,
        "query must be an identifier or path, such as `Seq::push`"
    );
    let parent = query
        .trim()
        .rsplit("::")
        .nth(1)
        .unwrap_or("")
        .to_lowercase();

    let pattern =
        format!(r"\b(fn|struct|enum|union|trait|type|const|mod|macro|macro_rules!)\s+{name}\b");
    let out = Command::new("rg")
        .args([
            "--no-heading",
            "--line-number",
            "--sort=path",
            "--glob=*.rs",
        ])
        .arg(pattern)
        .args([
            "/opt/verus/vstd",
            "/opt/docs/core",
            "/opt/docs/alloc",
            "/opt/docs/std",
        ])
        .output()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut hits: Vec<(&str, usize)> = stdout
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, ':');
            Some((parts.next()?, parts.next()?.parse().ok()?))
        })
        .collect();
    // Prefer files named after the parent: `Seq::push` matches vstd/seq.rs first.
    hits.sort_by_key(|(path, _)| {
        let stem = Path::new(path)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        (
            parent.is_empty() || stem != parent,
            !path.starts_with("/opt/docs/vstd"),
        )
    });
    if hits.is_empty() {
        return Ok(format!(
            "Nothing named `{name}` in vstd or the standard library."
        ));
    }
    let mut result = String::new();
    for (path, line) in hits.iter().take(MAX_RESULTS) {
        let text = fs::read_to_string(path)?;
        result += &format!("// {path}:{line}\n{}\n\n", definition(&text, *line));
    }
    if hits.len() > MAX_RESULTS {
        result += &format!(
            "({} more; use a longer path to narrow)\n",
            hits.len() - MAX_RESULTS
        );
    }
    Ok(result)
}

/// The doc comments and attributes above a definition, and its lines up to a body or blank line.
fn definition(text: &str, line: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let at = line - 1;
    let mut start = at;
    while start > 0 {
        let above = lines[start - 1].trim_start();
        if !(above.starts_with("///") || above.starts_with("#[")) || at - start >= MAX_LINES {
            break;
        }
        start -= 1;
    }
    let mut end = at;
    while end + 1 < lines.len() && end - at < MAX_LINES {
        let l = lines[end].trim();
        if l.is_empty() || l.ends_with(';') || l == "{" || l == "}" {
            break;
        }
        end += 1;
    }
    lines[start..=end].join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn includes_docs_and_signature() {
        let text = "x\n\n/// Adds.\n#[a]\nfn add()\n    ensures\n        true,\n{\n    body\n}\n";
        assert_eq!(
            super::definition(text, 5),
            "/// Adds.\n#[a]\nfn add()\n    ensures\n        true,\n{"
        );
    }
}
