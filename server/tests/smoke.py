#!/usr/bin/env python3
"""Run an isolated mock server and a full reimbursement using real curl calls."""
import datetime
import hashlib
import hmac
import json
import os
import pathlib
import shlex
import socket
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[2]
BINARY = ROOT / "target/debug/receipt-roots-server"
EVENT = "00000000-0000-0000-0000-000000000001"
FINANCE = "synthetic-smoke-finance"
WEBHOOK_ID = "synthetic-webhook-s1"
WEBHOOK_KEY = b"synthetic-only-not-a-real-secret"


def run():
    with tempfile.TemporaryDirectory(prefix="receipt-roots-smoke-") as directory:
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        env = dict(os.environ, PAYPAL_MODE="mock", LLM_MODE="mock",
                   MOCK_PAYPAL_FIXTURE="success", FINANCE_TOKEN=FINANCE,
                   BIND_ADDR=f"127.0.0.1:{port}",
                   DATABASE_URL=f"sqlite://{directory}/smoke.db?mode=rwc")
        with open(f"{directory}/server.log", "w+") as log:
            process = subprocess.Popen([str(BINARY)], cwd=ROOT, env=env,
                                       stdout=log, stderr=log)
            try:
                for _ in range(100):
                    if process.poll() is not None:
                        log.seek(0)
                        raise RuntimeError(log.read())
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                            break
                    except OSError:
                        time.sleep(0.05)
                else:
                    raise RuntimeError("mock server did not start")
                print("Receipt Roots S1 mock smoke")
                print(datetime.datetime.now(datetime.timezone.utc).isoformat())
                print("All identities, receipts, credentials, and signatures are synthetic.")
                count = 0

                def curl(method, path, body=None, headers=(), expected=200, auth=True):
                    nonlocal count
                    args = ["curl", "--silent", "--show-error",
                            "--max-time", "10", "--request", method, base + path,
                            "--write-out", "\nHTTP_STATUS:%{http_code}"]
                    if auth:
                        args += ["--header", f"Authorization: Bearer {FINANCE if auth is True else auth}"]
                    if body is not None:
                        args += ["--header", "Content-Type: application/json", "--data-binary", body]
                    for header in headers:
                        args += ["--header", header]
                    print("$ " + shlex.join(args))
                    result = subprocess.run(args, check=True, capture_output=True, text=True)
                    print(result.stdout)
                    raw, status = result.stdout.rsplit("\nHTTP_STATUS:", 1)
                    assert status == str(expected), result.stdout
                    count += 1
                    return json.loads(raw)

                budgets = curl("GET", f"/api/events/{EVENT}/budget-lines")["budget_lines"]
                payee = curl("GET", "/api/payees")["payees"][0]
                claimant_id = payee["claimant_id"]
                capability = claimant_id + "." + hmac.new(
                    FINANCE.encode(), f"mock-claimant:{claimant_id}".encode(), hashlib.sha256).hexdigest()
                claim = curl("POST", "/api/claims", json.dumps({
                    "event_id": EVENT, "budget_line_id": budgets[0]["id"],
                    "claimant_id": payee["claimant_id"], "receipt_text": "mock:clean"}), auth=capability)
                claim_id = claim["claim"]["id"]
                path = f"/api/claims/{claim_id}"
                stranger_id = "00000000-0000-0000-0000-000000000099"
                stranger = stranger_id + "." + hmac.new(
                    FINANCE.encode(), f"mock-claimant:{stranger_id}".encode(), hashlib.sha256).hexdigest()
                variants = [claim_id, f"%{ord(claim_id[0]):02X}" + claim_id[1:],
                            claim_id.replace("-", "%2d", 1).replace("-", "%2D", 1),
                            "%2F" + claim_id, claim_id + "%2f"]
                rejected = 0
                for encoded in variants:
                    for method, suffix, body in [("GET", "", None),
                                                  ("POST", "/extract", "{}"),
                                                  ("POST", "/submit", json.dumps({
                                                      "fields": {"merchant": "Synthetic shop", "date": "2026-10-01",
                                                                 "total_cents": 1250, "currency": "USD", "uncertain": []},
                                                      "items": [{"quantity": 1, "unit_cents": 1250}]}))]:
                        error = curl(method, f"/api/claims/{encoded}{suffix}", body,
                                     expected=403 if encoded == claim_id else 400, auth=stranger)
                        assert error["code"] == ("FORBIDDEN" if encoded == claim_id else "INVALID_INPUT")
                        rejected += 1
                unchanged = curl("GET", path, auth=capability)
                assert unchanged["claim"] == claim["claim"]
                extracted = curl("POST", path + "/extract", "{}", auth=capability)
                assert extracted["approval"] is None
                submitted = curl("POST", path + "/submit", json.dumps({
                    "fields": extracted["claim"]["fields"],
                    "items": [{"quantity": 1, "unit_cents": 1250}]}), auth=capability)
                assert submitted["claim"]["state"] == "Submitted"
                approved = curl("POST", path + "/approve", json.dumps({"payee_id": payee["id"]}))
                assert approved["claim"]["state"] == "Approved"
                pending = curl("POST", path + "/payout", "{}")
                payout = pending["payout"]
                assert pending["claim"]["state"] == "PayoutPending"
                event = json.dumps({"paypal_event_id": "synthetic-smoke-event-1",
                    "payout_id": payout["id"], "sender_batch_id": payout["sender_batch_id"],
                    "paypal_batch_id": payout["paypal_batch_id"], "item_id": payout["item_id"],
                    "amount_cents": 1250, "currency": "USD", "payee_id": payee["id"], "kind": "Succeeded"})
                signature = hmac.new(WEBHOOK_KEY, WEBHOOK_ID.encode() + b"\n" + event.encode(), hashlib.sha256).hexdigest()
                curl("POST", "/api/webhooks/paypal", event,
                     [f"X-Mock-Webhook-Id: {WEBHOOK_ID}", f"X-Mock-Signature: {signature}"])
                paid = curl("GET", path)
                assert paid["claim"]["state"] == "Paid"
                assert paid["payout"]["state"] == "Paid"
                final_budget = curl("GET", f"/api/events/{EVENT}/budget-lines")["budget_lines"][0]
                assert final_budget["paid_cents"] == 1250
                assert final_budget["reserved_cents"] == 0
                for method, route, body in [
                    ("GET", path, None),
                    ("POST", path + "/payout", "{}"),
                    ("POST", path + "/approve", "{}"),
                    ("GET", f"/api/events/{EVENT}/claims", None),
                    ("POST", "/api/claims", "{}"),
                ]:
                    error = curl(method, route, body, expected=403, auth=False)
                    assert error["code"] == "FORBIDDEN"
                duplicate = curl("POST", "/api/claims", json.dumps({
                    "event_id": EVENT, "budget_line_id": budgets[0]["id"],
                    "claimant_id": payee["claimant_id"], "receipt_text": "mock:clean"}))
                error = curl("POST", f"/api/claims/{duplicate['claim']['id']}/submit", json.dumps({
                    "fields": extracted["claim"]["fields"],
                    "items": [{"quantity": 1, "unit_cents": 1250}]}), expected=409)
                assert error["code"] == "DUPLICATE_RECEIPT"
                excess = curl("POST", "/api/claims", json.dumps({
                    "event_id": EVENT, "budget_line_id": budgets[0]["id"],
                    "claimant_id": payee["claimant_id"], "receipt_text": "synthetic over-budget receipt"}))
                fields = dict(extracted["claim"]["fields"], merchant="Other synthetic shop", total_cents=10000)
                error = curl("POST", f"/api/claims/{excess['claim']['id']}/submit", json.dumps({
                    "fields": fields, "items": [{"quantity": 1, "unit_cents": 10000}]}), expected=409)
                assert error["code"] == "OVER_BUDGET"
                final_budget = curl("GET", f"/api/events/{EVENT}/budget-lines")["budget_lines"][0]
                assert final_budget["paid_cents"] == 1250
                assert final_budget["reserved_cents"] == 0
                print(f"PASS: {count} curl requests; final claim Paid; duplicate and over-budget rejected; 5 unauthorized requests rejected; {rejected} non-owner claim requests rejected; paid_cents=1250; reserved_cents=0")
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


if __name__ == "__main__":
    run()
