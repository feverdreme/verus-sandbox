# Verus-Sandbox 2

This is the second itertion of an existing project.

## Motivation

Verus is a rust specification language and verifier, in which the programmer can specify precondtions, postconditions, and loop invariants in their code in  `verus! {}` block. There are existing efforts to automate Verus proofs, such as `AutoVerus` (https://arxiv.org/abs/2409.13082) and `VeruSAGE` (https://arxiv.org/abs/2512.18436), with their associated benchmarks `VerusBench` and `VeruSAGE-Bench`.

A typical problem would look like this:

```rust
use vstd::prelude::*;

verus! {
    spec fn is_even(x: int) -> bool {
        x % 2 == 0
    }

    proof fn double_is_even(x: int)
        ensures
            is_even(x + x),
    {
        todo!("prove that x + x is even")
    }

    fn double(x: u64) -> (result: u64)
        requires
            x <= u64::MAX / 2,
        ensures
            result == 2 * x,
            is_even(result as int),
    {
        proof {
            double_is_even(x as int);
        }
        x + x
    }
}
```

However, there are necessary precautions to take when setting up a verification experiment. We can categorize this as follows:

1. Sometimes we want to encapsulate or hide an implementation body to the agent. There's nothing stopping an agent from reading any part of the source code that it wants and "cheat" the verifier.
2. If the agent can't provide the necessary proof, it will use `axiom` or `assume`, which is disallowed.
3. The agent will look online for solutions to the problem.
4. THe agent will look through the system itself for solution files somewhere.

This motivates the need for a structured interaction interface with a verification agent to attempt a verus proof without limiting its abilities too much. There should still be a relatively simple abstraction barrier so as to leverage language model's freeform solution style and the ability to test multiple different things at once.

## Design 

We will implemenet a verus sandbox that allows the agent to interact with a given problem in a formal and correct way. This will have three components, an MCP server that restricts what the agent can edit or browse, a docker container that contains the immutable problem as well as a separated area of the filesystem that the agent can use as scratch, and a set of validators that perform sanity checks on proposed solutions as well as checking for disallowed symbols or techniques.

### MCP Server

The MCP server will provide the only tools allowed to an agent. It will:

- Keep an index of symbols in the source code
- Provide a `scan` tool that yields all public symbols and their visibility (either full function body and implementation or just the interface and specification)
- Provide a `read` tool that dumps the function body, implementation, or interface depending on the visibility rules
- Provide an `edit` tool that allows any editing needed to be done in the sandbox, with most bash commands allowed (including running executables). Commands run under the isolation rules in [Agent isolation](#agent-isolation), rather than relying on filtering the commands themselves.
- Provide a `docs` tool that allows reading the documentation of anything in the verus or rust standard library
- Provide a (toggable) `web_search` tool that searches the web. Optionally, an LLM-as-judge agent can be attached to this to redact any clear attempts at cheesing the solution by directly searching for it online
- Provide a `propose_solution` tool that allows submitting a proof solution that can be verified by the checker. See [Proposals](#proposals) for what a proposal may change
- Provide a `view_verus_output` tool that allows viewing the verus verifier feedback, with certain information redacted to prevent information leakage.

### Containerization

The container will be a docker image and managed on Colima. It will have the permissioned scratch directory for the agent as well as hosting the immutable source code of the experiment and any other tools or artifacts. There will be a script that compiles this container and provides the necessary configs and source code that allows this container to start with everything it needs.

The MCP server will also live on this container.

#### Agent isolation

Filtering commands cannot enforce visibility: an agent that can run code can read any file its user can read, including through `rustc` or Verus itself (`include!`, `#[path]`, build scripts, proc macros). Isolation is therefore enforced by the operating system:

- Agent commands run as a dedicated user that can write only to the scratch directory and cannot read the hidden source.
- Agent commands have no network access. `web_search` is served by the MCP server, not by the agent's shell.
- Verification runs as a different, trusted user, preferably in a separate container that the agent cannot reach.

#### Interface stubs

So that the agent can run Verus in scratch, the sandbox generates a version of the dependencies in which interface-only symbols keep their signature and specification but have no body (using `external_body` for functions and proofs, and `uninterp` for spec functions). Hidden symbols are left out. The real dependencies are linked only during trusted verification.

A solution that verifies against the stubs may behave differently against the real source, because the stubs hide spec function bodies that the solver can use when verifying against the real source. Trusted verification is authoritative.

### Validation Tools

There will be a set of validation and analysis tools:
- A symbol indexer that keeps track of all of the symbols in the experiment source and their visibility permissions. It rejects a config when it loads if anything the agent can see (a signature, specification, or visible body) mentions a hidden symbol, since the stubs would not compile
- A redactor that redacts verus output that leaks any private or nonvisible information to the agent
- A proposal checker that enforces the rules in [Proposals](#proposals)
- A verifier that takes an agent proposal and links it with the source code to see if it passes
- An LLM-as-judge sanity checker that looks for reward-hacking behaviors that make the proposed proof verify in an unintended way. It only flags runs for human review. The deterministic checks alone decide whether a proposal passes, so that results are reproducible. The judge runs outside the agent's environment, since it needs network access.
- Tools that enforce timeouts and cost limits. See [Limits](#limits)
- A logger to keep track of all actions, verus outputs, and tool calls. 

### Proposals

A task marks each hole that the agent may fill with `todo!("description of what goes here")`. Verus rejects `todo!()` in function bodies, so a proposal with an unfilled hole cannot pass. The same error stops Verus before it checks the rest of the file, so the agent needs to fill or temporarily stub every hole in scratch before it can get verification results.

Agents can also add their own helper functions and lemmas alongside the existing code. Additions are limited to new `spec`, `proof`, and exec functions. Each must have a name not already used in the task or its dependencies, since a new definition could otherwise shadow an imported symbol and change what a specification means.

Agents cannot add types, traits, `impl` blocks, imports, or macros. Experiments are therefore restricted to tasks whose intended solution is simple enough to need only functions.

The proposal checker compares the AST of the proposal with the AST of the original task, and rejects the proposal if anything has changed other than filled holes and permitted additions. This prevents the agent from weakening a specification instead of proving it.

The checker also rejects proposals whose filled holes or added functions introduce any of the following, including through a macro that could expand to one of them:

- `assume`, `admit`, `axiom`, and `assume_specification`
- `external_body`, `external`, and any other `#[verifier::...]` attribute that adds trust
- `exec_allows_no_decreases_clause`
- `unsafe`

These rules apply to the agent's code only. The generated stubs use `external_body` themselves.

### Limits

Every experiment is bounded by timeouts and cost limits, set in `config.json`:

- Timeouts on each tool call, on each Verus run, and on the whole experiment
- Cost limits on the agent's model usage, as well as on the number of proposals

Reaching an experiment-wide limit ends the run. A run that ends this way, or because the agent gives up or something crashes, is logged with the reason it ended.

## A typical experiment

An experiment would be a directory, for our purposes call it `ex1` that contains the following files:

- `config.json` that specifies all of the public symbols, symbols with only interface exposed, experiment name, timeouts and cost limits, any metadata, etc.
- `src/task.rs` that contains the code that the agent needs to verify
- `src/dependencies.rs` that contains any dependencies that need to be linked to the `task.rs` for it to compile. This is will visibility control comes in.

Once a solution is proposed and passes, or the run ends without one (see [Limits](#limits)), the outcome gets logged and then the container saves the scratch work, final logs, solution (if there is one), and shuts itself down and whatever tool is calling it exits cleanly. Note that logs should be continuously updated, and it makes sense for the experiment to either be mounted directly so files like logs can be editted directly or it should be copied onto the Docker image.

## Technical Details

- Implemented in Rust
- Tracked with git