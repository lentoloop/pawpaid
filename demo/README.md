# Historical mock demo

`./demo/pawpaid-demo.sh` is incompatible with this HEAD and exits with status 1 before starting a server, accessing a database, or making network calls. The hardened executable rejects mock payouts before database initialization. The script no longer selects mock mode.

Historical transcripts describe an earlier executable and do not validate this snapshot.

For current startup requirements, see [Local development](../README.md#install-and-run-locally). The supported payout mode is sandbox only. Historical mock fixtures and signed mock events cannot be converted to sandbox validation by changing a mode variable. Do not rerun the historical flows against populated state.
