# Verus-Sandbox 2

This is the second itertion of an existing project.

## Motivation

Verus is a rust specification language and verifier, in which the programmer can specify precondtions, postconditions, and loop invariants in their code in  `verus! {}` block. There are existing efforts to automate Verus proofs, such as `AutoVerus` (https://arxiv.org/abs/2409.13082) and `VeruSAGE` (https://arxiv.org/abs/2512.18436), with their associated benchmarks `VerusBench` and `VeruSAGE-Bench`.

A typical problem would look like this:

```rust
verus! {
    spec fn is_even(x: int) -> bool {
        // Some predicate on x, like x % 2 = 0
    }

    proof fn is_even_proof(x: int) {
        // todo
    }

    fn foo(x: int)
        requires /**/
        ensures is_even(x) {
        /* impl */
    }
}
```
> Note: Check that the above is valid verus. I may not be completely correct on the syntax.

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
- Provide an `edit` tool that allows any editing needed to be done in the sandbox, with most bash commands allowed (including running executables), provided that the user executing those commands and only iteract within the sandbox directory itself.
- Provide a `docs` tool that allows reading the documentation of anything in the verus or rust standard library
- Provide a (toggable) `web_search` tool that searches the web. Optionally, an LLM-as-judge agent can be attached to this to redact any clear attempts at cheesing the solution by directly searching for it online
- Provide a `propose_solution` tool that allows submitting a proof solution that can be verified by the checker
- Provide a `view_verus_output` tool that allows viewing the verus verifier feedback, with certain information redacted to prevent information leakage.

### Containerization

The container will be a docker image and managed on Colima. It will have the permissioned scratch directory for the agent as well as hosting the immutable source code of the experiment and any other tools or artifacts. There will be a script that compiles this container and provides the necessary configs and source code that allows this container to start with everything it needs.

The MCP server will also live on this container.

### Validation Tools

There will be a set of validation and analysis tools:
- A symbol indexer that keeps track of all of the symbols in the experiment source and their visibility permissions
- A redactor that redacts verus output that leaks any private or nonvisible information to the agent
- A verifier that takes an agent proposal and links it with the source code to see if it passes
- An LLM-as-judge sanity checker to make sure the agent did not exhibit any reward-hacking behaviors that make the proposed proof verify in an unintended way.
- A logger to keep track of all actions, verus outputs, and tool calls. 

## A typical experiment

An experiment would be a directory, for our purposes call it `ex1` that contains the following files:

- `config.json` that specifies all of the public symbols, symbols with only interface exposed, experiment name, any metadata, etc.
- `src/task.rs` that contains the code that the agent needs to verify
- `src/dependencies.rs` that contains any dependencies that need to be linked to the `task.rs` for it to compile. This is will visibility control comes in.

Once a solution is proposed an passes, it gets logged and then the container saves the scratch work, final logs, solution, and shuts itself down and whatever tool is calling it exits cleanly. Note that logs should be continuously updated, and it makes sense for the experiment to either be mounted directly so files like logs can be editted directly or it should be copied onto the Docker image.

## Technical Details

- Implemented in Rust
- Tracked with git