#!/usr/bin/env python3
"""Offline release-binary checks. Synthetic configuration; loopback requests only."""
import http.client
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1]).resolve())
root = Path(__file__).resolve().parent.parent
base = {
    "PATH": "/usr/bin:/bin",
    "PAYPAL_MODE": "sandbox",
    "PAYPAL_CLIENT_ID": "synthetic-client",
    "PAYPAL_CLIENT_SECRET": "synthetic-provider-secret",
    "PAYPAL_SANDBOX_RECIPIENT": "synthetic@example.invalid",
    "PAYPAL_WEBHOOK_ID": "synthetic-webhook",
    "JUDGE_CLAIMANT_PASSWORD": "synthetic-claimant-password-24",
    "JUDGE_FINANCE_PASSWORD": "synthetic-finance-password-24",
    "LLM_MODE": "manual",
    "RUST_BACKTRACE": "full",
}
checks = 0
for name in ["JUDGE_CLAIMANT_PASSWORD", "JUDGE_FINANCE_PASSWORD"]:
    for short in [None, "s" * 23]:
        env = dict(base, APP_ENV="development")
        if short is None:
            env.pop(name)
        else:
            env[name] = short
        result = subprocess.run([binary], env=env, capture_output=True, timeout=10)
        assert result.returncode != 0
        assert result.stderr == b"server startup or runtime failed; check deployment configuration\n"
        checks += 1
for mode in [None, "", "mock", "live", "bogus"]:
    env = dict(base)
    if mode is None:
        env.pop("PAYPAL_MODE")
    else:
        env["PAYPAL_MODE"] = mode
    result = subprocess.run([binary], env=env, capture_output=True, timeout=10)
    assert result.returncode != 0
    assert result.stderr == b"server startup or runtime failed; check deployment configuration\n"
    checks += 1
print(f"PASS release startup refusal and redaction: {checks} cases")

required = {
    "strict-transport-security": "max-age=31536000; includeSubDomains",
    "x-content-type-options": "nosniff",
    "referrer-policy": "no-referrer",
    "x-frame-options": "DENY",
}
for attempt in range(3):
    fixture = Path(tempfile.mkdtemp(prefix="release-runtime-", dir=root / "scratch-mq"))
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = dict(base, BIND_ADDR=f"127.0.0.1:{port}", DATABASE_URL=f"sqlite://{fixture / 'runtime.db'}?mode=rwc")
    process = subprocess.Popen([binary], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def request(method, path, body=None):
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        conn.request(method, path, body=body, headers={"Content-Type": "application/json"})
        response = conn.getresponse()
        status, headers, payload = response.status, dict(response.getheaders()), response.read()
        conn.close()
        for key, expected in required.items():
            assert headers.get(key) == expected
        csp = headers["content-security-policy"]
        assert "frame-ancestors 'none'" in csp and "script-src 'self'" in csp
        return status, payload

    try:
        for _ in range(100):
            try:
                assert request("GET", "/health")[0] == 200
                break
            except (ConnectionError, OSError):
                assert process.poll() is None, "runtime exited before health"
                time.sleep(0.1)
        else:
            raise AssertionError("runtime readiness timeout")
        # Missing signature headers must reject before any verifier/provider call.
        assert request("POST", "/webhooks/paypal/sandbox", b"{}")[0] == 401
        minute = int(time.time()) // 60
        statuses = []
        for _ in range(30):
            body = json.dumps({"username": "demo-finance", "password": "synthetic-wrong"})
            statuses.append(request("POST", "/api/session", body)[0])
        api_status = request("GET", "/api/config")[0]
        login_status = request("POST", "/api/session", b"{}")[0]
        if int(time.time()) // 60 != minute:
            continue
        assert statuses == [401] * 30
        assert api_status == login_status == 429
        print("PASS release headers: health, webhook rejection, failed login, API/login throttling")
        print("PASS runtime checks; no provider endpoint was invoked")
        break
    finally:
        process.terminate()
        process.communicate(timeout=10)
else:
    raise AssertionError("inconclusive: three minute rollovers")
