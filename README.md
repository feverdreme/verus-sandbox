# Verus Sandbox 2

Runs an agent on a Verus proof task with only the sandbox's MCP tools. See [DESIGN_DOC.md](DESIGN_DOC.md) for why.

## Setup

Needs Rust, Docker (on a Mac, `colima start`), and either an API key or a logged-in `claude` or `codex` CLI.

```sh
./build.sh                # builds the vsb:local image on the current Docker context
cargo build --release
```

## Run

```sh
target/release/vsb run experiments/triangle                # your claude CLI (default)
target/release/vsb run experiments/triangle --agent codex  # your codex CLI
cp .env.example .env                                       # then add your Anthropic key
target/release/vsb run experiments/triangle --agent api
```

Options: `--model M`, `--env-file PATH`, `--no-judge`. The exit code is 0 only if the task was solved.

Each run writes `runs/<name>-<time>/`: `result.json`, `log.jsonl` (written continuously), the agent's transcript, `scratch.tar`, and `solution.rs` if one was accepted. The tar file comes from the agent, so extract it with care.

## Experiments

An experiment has `config.json`, `src/task.rs` and `src/dependencies.rs`. Dependency symbols are hidden unless they are listed under `public` or `interface_only`. Holes are `todo!("description")`.

| Experiment | What it exercises |
| --- | --- |
| `consecutive` | A pure proof with everything visible |
| `triangle` | A loop invariant, and a lemma visible only as an interface |
| `budget` | Induction over a predicate whose definition is hidden |

## Tests

```sh
cargo test
python3 tests/e2e.py      # needs Docker and vsb:local; makes no model calls
```

Reference solutions are in `tests/solutions`. They are never copied into the image or a container.

## Known limits

- **Codex:** untested. It has no dollar cap, and some built-in tools may not be possible to turn off. A run where codex reports using one is discarded, possibly after the tool has run.
- **Dependencies** must verify with `--no-cheating`, so they cannot use `assume` or `external_body` themselves.
- **Banned and hidden names** match by identifier. A local variable called `external` is rejected, and so is `.len()` if a method named `len` is hidden.
- **Cost:** the judge's cost is not included in `cost_usd`.
- **Trusted parts:** the Docker daemon, the Linux kernel, Verus, and timing side channels are outside what isolation covers.
