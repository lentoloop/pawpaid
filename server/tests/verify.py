#!/usr/bin/env python3
"""Run S1 gates and retain the real output without exposing environment values."""
import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]


def run():
    os.chdir(ROOT)
    env = dict(os.environ, CARGO_BUILD_JOBS="2")
    common = subprocess.check_output(["git", "rev-parse", "--git-common-dir"], text=True).strip()
    alternate = pathlib.Path(common).resolve() / "s1-server-index"
    if alternate.exists():
        env["GIT_INDEX_FILE"] = str(alternate)

    def check(args):
        print("$ " + " ".join(args), flush=True)
        result = subprocess.run(args, env=env, capture_output=True, text=True)
        print(result.stdout + result.stderr, end="", flush=True)
        if result.returncode:
            raise RuntimeError(f"gate failed with exit {result.returncode}")
        return result.stdout + result.stderr

    check(["rustc", "--version"])
    check(["cargo", "fmt", "--check"])
    check(["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])
    tests = check(["cargo", "test", "--workspace"])
    count = sum(map(int, re.findall(r"test result: ok\. (\d+) passed", tests)))
    print(f"Workspace tests passed: {count}", flush=True)
    schema = ROOT / "contracts/schema/Claim.json"
    original = schema.read_bytes()
    try:
        schema.write_bytes(original + b" ")
        result = subprocess.run(["cargo", "test", "-p", "contracts", "tests::schema_drift", "--", "--exact"],
                                env=env, capture_output=True, text=True)
        if result.returncode == 0 or "schema drift for Claim" not in result.stdout + result.stderr:
            raise RuntimeError("schema negative probe did not detect the changed file")
        print("Schema negative probe: expected failure detected for Claim.json", flush=True)
    finally:
        schema.write_bytes(original)
    assert schema.read_bytes() == original
    check(["cargo", "test", "-p", "contracts", "tests::schema_drift", "--", "--exact"])
    check(["cargo", "build", "-p", "receipt-roots-server"])
    for name, value in [("PAYPAL_MODE", "sandbox"), ("PAYPAL_MODE", "live"), ("PAYPAL_MODE", "production"), ("LLM_MODE", "live")]:
        startup_env = dict(env, PAYPAL_MODE="mock", LLM_MODE="mock", DATABASE_URL="must-not-open")
        startup_env[name] = value
        result = subprocess.run([str(ROOT / "target/debug/receipt-roots-server")], env=startup_env,
                                capture_output=True, text=True, timeout=5)
        if result.returncode == 0 or "only mock is supported" not in result.stderr:
            raise RuntimeError(f"startup did not reject {name}={value} before opening the database")
    print("Startup probes: sandbox, live, and production rejected before database access", flush=True)
    smoke = check([sys.executable, "server/tests/smoke.py"])
    (ROOT / "docs/evidence/s1-smoke.txt").write_text(smoke)
    check(["git", "diff", "--check"])
    check(["git", "diff", "--cached", "--check"])
    names = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], env=env)
    paths = sorted(set(name.decode() for name in names.split(b"\0") if name))
    patterns = [r"sk-(ant-|proj-)?[A-Za-z0-9_-]{20,}", r"AKIA[0-9A-Z]{16}",
                r"-----BEGIN ([A-Z]+ )?PRIVATE KEY-----",
                r'''(?i:ANTHROPIC_API_KEY|PAYPAL_CLIENT_SECRET|PAYPAL_CLIENT_ID)\s*[:=]\s*["']?[A-Za-z0-9_/-]{12,}''']
    args = ["rg", "--line-number", "--no-heading"]
    for pattern in patterns:
        args += ["-e", pattern]
    args += ["--"] + [name for name in paths if pathlib.Path(name).is_file()]
    scan = subprocess.run(args, env=env, capture_output=True, text=True)
    if scan.returncode != 1:
        raise RuntimeError("credential-pattern scan failed; inspect files locally without copying potential credentials into evidence")
    print(f"Credential-pattern grep: 0 matches across {len(paths)} files using {len(patterns)} patterns", flush=True)
    print("PASS: fmt; clippy with 0 warnings; workspace tests; schema drift and negative probe; 10 curl requests; diff checks; credential-pattern grep", flush=True)


if __name__ == "__main__":
    run()
