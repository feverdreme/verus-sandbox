#!/usr/bin/env python3
"""Runs the real launcher, container and verifier with a scripted stand-in for `claude`.

Needs the vsb:local image and a running Docker. Makes no model calls.
"""
import json, os, pathlib, re, shutil, subprocess, sys, tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
VSB = ROOT / "target/debug/vsb"
FAKE = '''#!%s
import json, pathlib, subprocess, sys
here = pathlib.Path(__file__).parent
args = sys.argv
print(json.dumps({"type": "system", "subtype": "init", "tools": json.loads((here / "tools.json").read_text())}), flush=True)
server = json.loads(args[args.index("--mcp-config") + 1])["mcpServers"]["vsb"]
p = subprocess.Popen([server["command"], *server["args"]], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def rpc(i, method, params):
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\\n"); p.stdin.flush()
    return json.loads(p.stdout.readline())
rpc(0, "initialize", {"protocolVersion": "2025-06-18"})
out = [rpc(1, "tools/list", {})["result"]["tools"]]
for i, (name, a) in enumerate(json.loads((here / "calls.json").read_text())):
    out.append(rpc(i + 2, "tools/call", {"name": name, "arguments": a})["result"]["content"][0]["text"])
(here / "out.json").write_text(json.dumps(out))
p.stdin.close(); p.wait()
print(json.dumps({"type": "result", "subtype": "success", "total_cost_usd": 0.0}), flush=True)
''' % sys.executable


def run(experiment, calls, config=None, tools=("mcp__vsb__scan",)):
    """Returns (result.json, replies to each call, run directory)."""
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp).resolve()
        (tmp / "claude").write_text(FAKE); (tmp / "claude").chmod(0o755)
        (tmp / "tools.json").write_text(json.dumps(tools))
        (tmp / "calls.json").write_text(json.dumps(calls))
        exp = tmp / "exp"
        shutil.copytree(ROOT / "experiments" / experiment, exp)
        if config:
            c = json.loads((exp / "config.json").read_text())
            c["limits"].update(config)
            (exp / "config.json").write_text(json.dumps(c))
        env = dict(os.environ, PATH=f"{tmp}:{os.environ['PATH']}")
        p = subprocess.run([VSB, "run", exp, "--no-judge"], cwd=tmp, env=env, capture_output=True, text=True)
        runs = list((tmp / "runs").glob("*")) if (tmp / "runs").exists() else []
        assert runs, p.stderr
        result = json.loads((runs[0] / "result.json").read_text())
        out = json.loads((tmp / "out.json").read_text()) if (tmp / "out.json").exists() else []
        log = (runs[0] / "log.jsonl").read_text() if (runs[0] / "log.jsonl").exists() else ""
        saved = {f.name for f in runs[0].iterdir()}
        assert (p.returncode == 0) == (result["status"] == "solved"), (p.returncode, result)
        return result, out[1:], log, saved


def put(path, text):
    return ["edit", {"command": f"cat > {path} <<'EOF_FILE'\n{text}\nEOF_FILE"}]


PROPOSE = ["propose_solution", {}]
def task(e): return (ROOT / f"experiments/{e}/src/task.rs").read_text()
def solution(e): return (ROOT / f"tests/solutions/{e}.rs").read_text()
def bash(c): return ["edit", {"command": c}]


def test_solutions():
    for e in ["consecutive", "triangle", "budget"]:
        # It must verify against the stubs as well as the real dependencies.
        result, out, log, saved = run(e, [["scan", {}], put("task.rs", solution(e)),
                                     bash("verus --crate-type=lib lib.rs"), PROPOSE])
        assert "0 errors" in out[2], out[2]
        assert result["status"] == "solved", (result, out)
        assert {"log.jsonl", "scratch.tar", "solution.rs", "agent.jsonl"} <= saved, saved


def test_rejections():
    t = task("consecutive")
    cases = {
        "unfilled": t,
        "weakened": solution("consecutive").replace("is_even(n * (n + 1))", "true"),
        "assume": re.sub(r"todo!\(.*\)", "assume(false);", t),
        "external_body": solution("consecutive").replace("} // verus!", "#[verifier::external_body] proof fn z() ensures false {}\n}"),
        "wrong proof": re.sub(r"todo!\(.*\)", "", t),
    }
    calls = [c for text in cases.values() for c in (put("task.rs", text), PROPOSE)]
    result, out, *_ = run("consecutive", calls, {"max_proposals": 10})
    for name, reply in zip(cases, out[1::2]):
        assert reply.startswith("NOT ACCEPTED"), (name, reply)
    assert "postcondition not satisfied" in out[9], out[9]
    assert result["status"] == "gave_up" and result["proposals"] == 5, result


def test_limits():
    result, *_ = run("consecutive", [PROPOSE] * 2, {"max_proposals": 2})
    assert result["status"] == "proposal_limit", result
    result, out, *_ = run("consecutive", [bash("sleep 100")], {"tool_timeout_secs": 2})
    assert "timed out" in out[0], out
    result, *_ = run("consecutive", [bash("sleep 100")], {"experiment_timeout_secs": 5})
    assert result["status"] == "timeout", result


def test_extra_tools_void_the_run():
    result, *_ = run("consecutive", [], tools=["Bash"])
    assert result["status"] == "crashed", result


def test_isolation():
    result, out, *_ = run("budget", [
        bash("id -u; id -G"),
        bash("cat /experiment/* /state/* /verify/*; ls /experiment /state /verify"),
        bash("grep -rsl UNIT_PRICE / --include='*' --exclude-dir=proc --exclude-dir=sys 2>/dev/null | head"),
        bash("touch /problem/x /x /usr/x /tmp/x /dev/shm/x /opt/x 2>&1; find / -xdev -writable -type d -not -path '/proc/*' 2>/dev/null"),
        bash("cat /proc/1/environ /proc/1/mem; for p in /proc/[0-9]*; do cat $p/environ; done 2>&1 | grep -ci unit"),
        bash("timeout 5 bash -c 'echo > /dev/tcp/1.1.1.1/53' && echo REACHED; ls /sys/class/net; grep -c . /proc/net/route"),
        bash("(setsid sleep 300 &) ; echo started"),
        bash("pgrep -u 1001 sleep || echo none"),
        bash(":(){ :|:& };:; sleep 1"),
        bash("echo alive"),
        bash("ln -s /experiment/experiment.json link.rs; ln -s /verify/dependencies.rs link2.rs"),
        ["propose_solution", {"path": "link.rs"}],
        ["propose_solution", {"path": "link2.rs"}],
        ["read", {"symbol": "affordable"}],
        ["read", {"symbol": "UNIT_PRICE"}],
        ["read", {"symbol": "no_such_thing"}],
        ["scan", {}],
        bash("cat dependencies.rs; printf '#[path=\"/verify/dependencies.rs\"] mod x;' > z.rs; verus --crate-type=lib z.rs 2>&1 | tail -3"),
        put("task.rs", task("budget").replace('todo!("prove that the total is affordable; add a helper lemma if you need induction")', "assert(UNIT_PRICE == 7);")), PROPOSE,
        put("task.rs", re.sub(r"todo!\(.*\)", "lemma_affordable_add(1, 2);", task("budget"))), PROPOSE,
        ["docs", {"query": "Seq::push"}], ["docs", {"query": "Option"}],
    ], {"max_proposals": 10})
    for i, o in enumerate(out):
        if "-v" in sys.argv: print(f"--- {i}\n{o[:1500]}")
    assert out[0].split()[-2:] == ["1001", "1001"], out[0]
    for o in out[:-2]:
        assert "cost % " not in o and "= 7;" not in o, o
    assert out[1].count("Permission denied") >= 3, out[1]
    assert "REACHED" not in out[5], out[5]
    assert "none" in out[7], out[7]
    assert "alive" in out[9], out[9]
    assert "Permission denied" in out[11] and "Permission denied" in out[12], out[11:13]
    assert out[14].replace("UNIT_PRICE", "X") == out[15].replace("no_such_thing", "X")
    assert "UNIT_PRICE" not in out[16]
    assert "not available" in out[19], out[19]
    assert "precondition not satisfied" in out[21], out[21]
    assert "/opt/docs/vstd/seq.rs" in out[22].splitlines()[0] and "Option" in out[23], out[22][:300]
    assert result["status"] == "gave_up"


if __name__ == "__main__":
    subprocess.run(["cargo", "build", "-q"], cwd=ROOT, check=True)
    failed = 0
    for name, test in list(globals().items()):
        if name.startswith("test_") and (len([a for a in sys.argv[1:] if a != "-v"]) == 0 or name in sys.argv):
            try:
                test(); print("ok  ", name, flush=True)
            except AssertionError as e:
                failed += 1; print("FAIL", name, str(e)[:3000], flush=True)
    left = subprocess.run(["docker", "ps", "-aq", "--filter", "name=vsb-"], capture_output=True, text=True).stdout
    assert not left, "containers were left behind"
    sys.exit(failed > 0)
