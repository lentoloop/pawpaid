# Pawpaid

Pawpaid is a pet medical reimbursement demo with a Rust server, a React web app, and a SQL ledger. A claimant submits a synthetic clinic invoice against an animal case budget. Finance reviews the claim and can send a sandbox payout to the configured clinic recipient.

Public case pages are static demo pages with fictional names, amounts, and case histories. The "Donate (demo)" button does nothing. There is no donation collection or real clinic verification. Judge demo accounts exist. Credentials are provided in the Devpost submission and are not included here.

## PayPal and claim flow

PayPal is the payment sponsor integration. The application uses the sandbox Payouts API only, plus OAuth token acquisition and sandbox webhook signature verification. It does not use PayPal Checkout or move real money. `server/src/sandbox.rs` fixes the provider origin to `api-m.sandbox.paypal.com` and disables redirects. `server/src/config.rs` refuses startup unless `PAYPAL_MODE=sandbox`.

1. The claimant creates a claim with synthetic invoice text, enters and confirms receipt fields, and submits it against a budget line and payee. Submission reserves the amount in the ledger.
2. Finance reviews the submitted claim and approves or rejects it. Approval records the reviewed invoice facts.
3. Finance requests a payout for an approved claim. The server records the payout intent before calling PayPal. The recipient comes from server configuration, not the payout request. Each claim permits one payout, capped at US$10.00, with at most 20 attempts per Taipei calendar day across the site.
4. Finance checks payout status. The server queries the known PayPal batch and item and compares their facts against the stored intent. Automatic status updates mark a claim paid only after a matching single-item SUCCESS. Ambiguous results retain the reservation and do not trigger another send. Verified sandbox webhook events can also support reconciliation.

Finance can record a manual disposition that adjusts reserved and paid totals. A paid ledger state alone therefore does not prove PayPal reported SUCCESS. The implementation is in `server/src/http.rs`, `server/src/domain.rs`, `server/src/lib.rs`, `server/src/sandbox.rs`, and `server/src/webhook.rs`.

## AI use

The live deployment runs `LLM_MODE=manual`. Receipt fields are entered by hand. There is no live AI receipt extraction.

The code includes an optional `gemini-free` text extraction mode in `server/src/extract.rs`. It requires `GEMINI_API_KEY` and `GEMINI_FREE_TIER_ONLY=true`. This mode is off on the live deployment. The free-tier flag is an operator assertion, not a billing check. The adapter uses Gemini 2.5 Flash on synthetic invoice text, with no image input, and falls back to manual entry when extraction is unavailable. Human confirmation is required.

The four demo animal photos are AI-generated. Their provenance is recorded in `web/public/pawpaid/photos/SOURCE.md`. The code was written with AI coding assistants, including Codex and Claude Code. These development tools are not live application features.

## Hosting and database

The demo is hosted on Render. The repository includes a multi-stage `Dockerfile`, `docker/start.sh`, and `docker/nginx.conf`; it has no `render.yaml`. The container builds the web app and server, then uses Nginx to serve static assets and proxy API requests. The inherited Dockerfile sets an obsolete mock payout default. Override it with the required sandbox setting when running the container, or startup fails.

The deployment uses Neon Postgres for infrastructure. Neon is not a sponsor integration. `server/src/main.rs` selects SQLite for SQLite connection URLs and Postgres otherwise. Local development defaults to SQLite, and schema migrations are under `migrations/`.

## Install and run locally

Prerequisites are Rust 1.99.1 with Cargo, Node 26.13.0 with npm, and sandbox PayPal application credentials with access to Payouts. The repository pins the toolchain and includes dependency lockfiles. Optional browser tests require Chrome or Chromium. Postgres is optional for local development.

Configure the environment variables described below in your shell or a private environment manager. The server does not load dotenv files. No environment files or credential values are supplied in this snapshot. Use synthetic invoice text only.

Build and start the server from the repository root:

```sh
cargo build --locked -p receipt-roots-server
cargo run --locked -p receipt-roots-server
```

The default server address is `http://127.0.0.1:3000`. Startup applies database migrations and initializes synthetic demo records. In a second terminal, start the web app:

```sh
cd web
npm ci
npm run dev
```

Open `http://127.0.0.1:5173`. Vite proxies API requests to the Rust server. Enter the account name and password configured for your local claimant or finance role. The public snapshot replaces live account identifiers in source and fixtures with synthetic identifiers. Optional username environment variables let you choose local account names without changing source.

Build the static web app with:

```sh
cd web
npm run typecheck
npm run build
```

The result is `web/dist`. The standalone Rust server does not serve these assets. The Docker packaging supplies that static hosting layer.

## Environment variables

This list contains names and purposes only. Required settings must be supplied privately before server startup.

| Name | Purpose |
| --- | --- |
| `PAYPAL_MODE` | Required sandbox-only payout mode. |
| `PAYPAL_CLIENT_ID` | Required sandbox application identifier for OAuth. |
| `PAYPAL_CLIENT_SECRET` | Required sandbox application credential for OAuth. |
| `PAYPAL_SANDBOX_RECIPIENT` | Required fixed sandbox payout recipient. |
| `PAYPAL_WEBHOOK_ID` | Required sandbox webhook registration identifier for signature verification. |
| `JUDGE_CLAIMANT_PASSWORD` | Required claimant login password. Outside explicit debug development mode, it must have at least 24 characters. |
| `JUDGE_FINANCE_PASSWORD` | Required finance login password with the same length rule. |
| `JUDGE_CLAIMANT_USERNAME` | Optional claimant login name override for the sanitized snapshot. |
| `JUDGE_FINANCE_USERNAME` | Optional finance login name override. Names must be distinct and nonempty. |
| `DATABASE_URL` | Database connection string. Selects local SQLite or Postgres. |
| `BIND_ADDR` | Rust server bind address. |
| `ALLOW_PUBLIC_BIND` | Explicit permission for a non-loopback server bind. |
| `TRUST_LOOPBACK_PROXY` | Enables the packaged loopback proxy's client address handling. |
| `APP_ENV` | Enables development behavior only in debug builds. |
| `FINANCE_TOKEN` | Internal finance capability, generated at startup if absent. It does not replace judge session login. |
| `LLM_MODE` | Selects manual receipt entry or the optional Gemini text adapter. |
| `GEMINI_API_KEY` | Credential needed only for optional Gemini extraction. |
| `GEMINI_FREE_TIER_ONLY` | Required free-tier-only operator assertion for optional extraction. |
| `MOCK_PAYPAL_FIXTURE` | Internal fixture selector retained for mock adapter tests. It does not enable mock server startup. |
| `API_BASE` | Optional Vite development proxy destination. |
| `CHROME_PATH` | Optional browser executable path for web tests. |
| `TEST_DATABASE_URL` | Dedicated Postgres 17 database for integration tests. Tests create and drop isolated schemas. |
| `UI_E2E_PORT` | Optional preview server port for web fixture tests. |
| `PAWPAID_SHOTS_DIR` | Optional output directory for synthetic web test screenshots. |

## Run tests

The full Rust suite requires a dedicated Postgres 17 test database configured through `TEST_DATABASE_URL`. Do not point tests at the live demo database. Run tests from the repository root:

```sh
cargo test --locked --workspace
```

To run the Rust suite without its Postgres tests:

```sh
cargo test --locked --workspace -- --skip postgres_tests
```

Run web checks after installing dependencies:

```sh
cd web
npm run typecheck
npm run build
npm run lint
npm test
```

Web tests use Playwright Core and require a local Chrome or Chromium executable. `npm test` covers fixture pages and localization; it is not proof of live PayPal transfers. Some design-reference checks skip when the original private design inputs are absent. Historical mock tests exercise internal adapters, while runtime startup permits only sandbox mode. Historical evidence, internal reports, environment files, and private deployment configuration are omitted from this snapshot.

## License

MIT. Copyright (c) 2026 ktwu1997. Bundled fonts retain their own license notices under `web/public/fonts/`.
