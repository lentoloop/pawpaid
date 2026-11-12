import "./f2bo/csp";
import React, { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { createPortal } from "react-dom";
import templates from "./templates.json";
import { ApprovedSignin } from "./f2bo/Signin";
import { initialLocale, localize, setLocale, text, type Locale } from "./i18n";
import { z } from "zod";
import {
  ApiFailure,
  budgetsSchema,
  configSchema,
  dollars,
  parseCents,
  payeesSchema,
  queueSchema,
  request,
  responseSchema,
  type Budgets,
  type ClaimResponse,
} from "./api";
import "./f2bo/fonts.css";
import "./f2bo/f2.css";
import sharedTemplates from "./f2bo/shared.json";
import "./tokens.css";
import "./app.css";

const caseCopy: ReadonlyArray<readonly [string, string]> = [
  ["Receipt Roots", "Pawpaid"],
  ["Choose an approved budget line for an expense you paid personally.", "Choose one animal case. Compare the clinic estimate with the clinic invoice. The foster or rescue group registered the clinic payee."],
  ["Only finance can approve this claim. The recipient must be a preset sandbox volunteer.", "Only finance can approve this clinic invoice. The clinic payee is the synthetic sandbox clinic registered by the foster or rescue group."],
  ["The recipient is fixed to the claimant's allowed sandbox preset.", "The clinic payee stays attached to the foster or rescue group that registered it."],
  ["Available is approved minus reserved minus paid.", "Available is the clinic estimate minus reserved minus paid. Raised (demo) is fixture display only, not donations received."],
  ["Synthetic volunteer", "Synthetic foster or rescue group"],
  ["sandbox volunteer", "sandbox clinic"],
  ["Volunteer meals", "Animal case · follow-up care"],
  ["Printing and signage", "Animal case · urgent care"],
  ["Synthetic supplies", "Animal case · clinic estimate"],
  ["Synthetic shop", "Synthetic animal clinic"],
  ["Volunteer", "Foster or rescue group"],
  ["volunteer", "foster or rescue group"],
  ["Event budget", "Animal cases"],
  ["event budget", "animal cases"],
  ["Budget line", "Animal case"],
  ["budget line", "animal case"],
  ["Category", "Animal case"],
  ["Approved amount", "Approved invoice amount"],
  ["Recipient name", "Clinic payee name"],
  ["Preset sandbox recipient", "Registered clinic payee (sandbox)"],
  ["preset recipient", "registered clinic payee"],
  ["recipient", "clinic payee"],
  ["Recipient", "Clinic payee"],
  ["Merchant", "Clinic"],
  ["merchant", "clinic"],
  ["Receipt", "Clinic invoice"],
  ["receipt", "clinic invoice"],
  ["Claimant", "Foster or rescue group"],
  ["Claim amount", "Clinic invoice amount"],
  ["Claim status", "Clinic invoice status"],
];
function pawpaidCopy(text: string): string {
  if (text.trim().includes("@")) return text;
  return caseCopy.reduce((copy, [before, after]) => copy.replaceAll(before, after), text)
    .replace(/\bBudget\b/g, "Clinic estimate")
    .replace(/\bbudget\b/g, "clinic estimate")
    .replace(/\bclaims\b/g, "clinic invoices")
    .replace(/\bclaim\b/g, "clinic invoice");
}
const statusLabels: Record<ClaimResponse["claim"]["state"], string> = {
  Draft: "Draft", NeedsInfo: "Needs information", DuplicateBlocked: "Duplicate blocked",
  OverBudget: "Over clinic estimate", Submitted: "Submitted", Approved: "Approved",
  PayoutPending: "Payment pending", Paid: "Paid", PayoutFailed: "Payment failed", PayoutUnknown: "Payment status unknown",
};
function originalLabel(element: Element): string {
  return element.getAttribute("data-original-label") || element.textContent?.trim() || "";
}
const pawpaidTemplates = templates;

function isPage(value: string): value is keyof typeof templates {
  return Object.hasOwn(templates, value);
}
const route =
  location.pathname.split("/").pop()?.replace(".html", "") || "01-budget";
const page = isPage(route) ? route : "01-budget";
const params = new URLSearchParams(location.search);
const gallery = params.has("fixtures");
const runtimeTemplates = Object.fromEntries(Object.entries(pawpaidTemplates).map(([key, states]) => [key, states.map(html => {
  const fragment = document.createElement("template");
  fragment.innerHTML = html;
  fragment.content.querySelectorAll(".inset, .annotation").forEach(element => element.remove());
  return fragment.innerHTML;
})]));
function isShared(value: string): value is keyof typeof sharedTemplates { return Object.hasOwn(sharedTemplates, value); }
function sharedName(): keyof typeof sharedTemplates {
  const value = params.get("shared") || "";
  return isShared(value) ? value : "s-error";
}
const selected = params.get("claim") || sessionStorage.getItem("claim") || "";
const claimId = z
  .string()
  .regex(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i)
  .safeParse(selected);
const storedId = claimId.success ? claimId.data : "";
const defaults: Record<string, number> = {
  "01-budget": 3,
  "02-new-claim": 0,
  "03-review": 0,
  "04-claim-status": 0,
  "05-finance-queue": 0,
  "06-finance-review": 0,
  "07-payout-status": 0,
};
function go(target: string, id = storedId) {
  location.href = `/${target}.html${id ? `?claim=${encodeURIComponent(id)}` : ""}`;
}
function App({ locale, switcher }: { locale: Locale; switcher: React.ReactNode }) {
  const root = useRef<HTMLDivElement>(null);
  const [switchHost, setSwitchHost] = useState<Element | null>(null);
  const boundFields = useRef("");
  const uncertaintyResolved = useRef(true);
  const [index, setIndex] = useState(
    gallery
      ? defaults[page] || 0
      : page === "01-budget"
        ? 1
        : storedId && page !== "02-new-claim"
          ? 1
          : defaults[page] || 0,
  );
  useLayoutEffect(() => {
    setSwitchHost(root.current?.querySelector(".identity") || null);
  }, [index]);
  const [record, setRecord] = useState<ClaimResponse | null>(null);
  const [budgets, setBudgets] = useState<Budgets>({ budget_lines: [] });
  const [queue, setQueue] = useState<ClaimResponse[]>([]);
  const [busy, setBusy] = useState(false);
  const [ready, setReady] = useState(false);
  const [error, setError] = useState("");
  const [config, setConfig] = useState<z.infer<typeof configSchema> | null>(
    null,
  );
  const [payees, setPayees] = useState<z.infer<typeof payeesSchema>>({
    payees: [],
  });
  const loadedAt = useRef<Date | null>(null);
  const checkedProvider = useRef(false);
  function show(saved: ClaimResponse, checked = false) {
    loadedAt.current = new Date();
    checkedProvider.current = checked;
    setRecord(saved);
    uncertaintyResolved.current = saved.claim.fields.uncertain.length === 0;
    sessionStorage.setItem("claim", saved.claim.id);
    const state = saved.claim.state;
    if (page === "03-review")
      setIndex(
        state === "NeedsInfo"
          ? 4
          : state === "DuplicateBlocked"
            ? 6
            : state === "OverBudget"
              ? 7
              : state === "Submitted"
                ? 8
                : saved.claim.fields.total_cents === null
                  ? 0
                  : 3,
      );
    if (page === "04-claim-status")
      setIndex(
        {
          Draft: 3,
          NeedsInfo: 3,
          DuplicateBlocked: 4,
          OverBudget: 4,
          Submitted: 5,
          Approved: 6,
          PayoutPending: 7,
          Paid: 8,
          PayoutFailed: 9,
          PayoutUnknown: 10,
        }[state],
      );
    if (page === "06-finance-review")
      setIndex(
        state === "Approved"
          ? saved.approval && saved.approval.expires_at * 1000 > Date.now()
            ? 6
            : 7
          : state === "Submitted"
            ? 2
            : 5,
      );
    if (page === "07-payout-status")
      setIndex(
        {
          Draft: 0,
          NeedsInfo: 0,
          Submitted: 0,
          DuplicateBlocked: 0,
          OverBudget: 0,
          Approved: 0,
          PayoutPending: 3,
          Paid: 4,
          PayoutFailed: 5,
          PayoutUnknown: 6,
        }[state],
      );
  }
  async function refresh() {
    const cfg = await request("/config", configSchema);
    setConfig(cfg);
    const [lines, recipients] = await Promise.all([
      request(`/events/${cfg.event_id}/budget-lines`, budgetsSchema),
      request("/payees", payeesSchema),
    ]);
    setBudgets(lines);
    setPayees(recipients);
    if (page === "01-budget") setIndex(lines.budget_lines.length ? 3 : 0);
    if (
      storedId &&
      page !== "01-budget" &&
      page !== "02-new-claim" &&
      page !== "05-finance-queue"
    )
      show(await request(`/claims/${storedId}`, responseSchema));
    if (page === "05-finance-queue") {
      if (sessionStorage.getItem("claimant") && !sessionStorage.getItem("finance")) {
        setError("ROLE_REQUIRED"); setIndex(2); setReady(true); return;
      }
      const token = sessionStorage.getItem("finance") || sessionStorage.getItem("claimant");
      if (token) {
        const result = await request(
          `/events/${cfg.event_id}/claims`,
          queueSchema,
          undefined,
          token,
        );
        setQueue(result.claims);
        setIndex(result.claims.length ? 3 : 0);
      } else setIndex(2);
    }
    setReady(true);
  }
  function failed(cause: unknown) {
    const code = cause instanceof ApiFailure ? cause.code : "UNAVAILABLE";
    setError(code);
    setReady(false);
    if (page === "01-budget") setIndex(2);
    if (page === "02-new-claim") setIndex(3);
    if (page === "03-review")
      setIndex(
        code === "DUPLICATE_RECEIPT" ? 6 : code === "OVER_BUDGET" ? 7 : 2,
      );
    if (page === "04-claim-status" && !record) setIndex(2);
    if (page === "05-finance-queue") setIndex(2);
    if (page === "06-finance-review")
      setIndex(code === "FORBIDDEN" ? 3 : code === "APPROVAL_INVALID" ? 7 : 5);
    if (page === "07-payout-status" && !record) setIndex(2);
  }
  useEffect(() => {
    if (!gallery) void refresh().catch(failed);
  }, []);
  useEffect(() => {
    if (gallery || !root.current) return;
    const host = root.current;
    const banner = host.querySelector(".sandbox-banner");
    if (banner && config) {
      banner.setAttribute("data-literal", "true");
      banner.textContent = text("Test mode (demo). PayPal: sandbox. AI: mock. No real money. Sandbox payouts: maximum USD 10.00.")
        .replace(/PayPal[:：]\s*(?:sandbox|沙盒)/, `PayPal${locale === "en" ? ": " : "："}${config.paypal_mode === "sandbox" ? text("sandbox") : locale === "en" ? "simulated" : "模擬"}`)
        .replace("AI: mock", `AI: ${config.llm_mode}`).replace("AI：模擬", `AI：${text(config.llm_mode)}`);
    }
    host.querySelectorAll<HTMLInputElement>('input[type="password"]').forEach(input => {
      const label = input.closest<HTMLElement>(".field, label");
      if (label) label.hidden = true;
      input.hidden = true;
      input.required = false;
    });
    host.querySelectorAll<HTMLButtonElement>("button:not(.locale-button)").forEach(button => {
      if (originalLabel(button) === "Send mock payout" && config?.paypal_mode === "sandbox") button.textContent = "Send sandbox payout";
    });
    host.querySelectorAll<HTMLLIElement>("li").forEach((li) => {
      const label = li.querySelector("button") ? originalLabel(li.querySelector("button")!) : "";
      if (label === "Mark resolved") li.hidden = uncertaintyResolved.current;
      if (label === "Mark unresolved") li.hidden = !uncertaintyResolved.current;
    });
    host.querySelectorAll<HTMLButtonElement>("button:not(.locale-button)").forEach((b) => {
      if (b.dataset.initialDisabled === undefined)
        b.dataset.initialDisabled = String(b.disabled);
      b.disabled = b.dataset.initialDisabled === "true";
    });
    host
      .querySelectorAll<HTMLAnchorElement>('a[href$=".html"]')
      .forEach((a) => {
        const url = new URL(a.href);
        if (storedId) url.searchParams.set("claim", storedId);
        a.href = url.pathname + url.search;
      });
    host.querySelectorAll<HTMLElement>("main a, main button").forEach((a) => {
      if (originalLabel(a) === "Open claim")
        a.onclick = (event) => {
          event.preventDefault();
          const input =
            host.querySelector<HTMLInputElement>(".state-panel input");
          const value = z
            .string()
            .regex(
              /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i,
            )
            .safeParse(input?.value);
          if (value.success)
            go(
              page === "04-claim-status"
                ? "04-claim-status"
                : "06-finance-review",
              value.data,
            );
          else input?.setAttribute("aria-invalid", "true");
        };
    });
    host.querySelectorAll<HTMLSelectElement>(".role select").forEach((s) => {
      s.onchange = () =>
        go(s.value === "finance" ? "05-finance-queue" : "01-budget");
    });
    // The approved budget page contains illustrative rows. The application only lists actual API rows.
    if (page === "01-budget" && ready) {
      const first = host.querySelector("table");
      const row = first?.querySelector("tbody tr");
      if (first && row) {
        const tbody = first.querySelector("tbody");
        if (tbody) {
          tbody.replaceChildren();
          for (const line of budgets.budget_lines) {
            const copy = row.cloneNode(true);
            if (copy instanceof HTMLElement) {
              const cells = copy.querySelectorAll("td");
              const values = [
                pawpaidCopy(line.category),
                dollars(line.approved_cents),
                dollars(line.reserved_cents),
                dollars(line.paid_cents),
                dollars(
                  line.approved_cents - line.reserved_cents - line.paid_cents,
                ),
              ];
              values.forEach((v, i) => {
                const cell = cells[i];
                if (cell) cell.textContent = v;
              });
              const b = copy.querySelector("button");
              if (b) {
                b.dataset.budget = line.id;
                b.disabled =
                  line.approved_cents - line.reserved_cents - line.paid_cents <=
                  0;
              }
              tbody.append(copy);
            }
          }
        }
        host.querySelectorAll(".table-wrap").forEach((el, i) => {
          if (i > 0) el.remove();
        });
        host.querySelectorAll(".inset").forEach((el) => el.remove());
        const totals = host.querySelectorAll(".event-totals dd");
        const sums = [
          budgets.budget_lines.reduce((n, l) => n + l.approved_cents, 0),
          budgets.budget_lines.reduce((n, l) => n + l.reserved_cents, 0),
          budgets.budget_lines.reduce((n, l) => n + l.paid_cents, 0),
          budgets.budget_lines.reduce(
            (n, l) => n + l.approved_cents - l.reserved_cents - l.paid_cents,
            0,
          ),
        ];
        totals.forEach((el, i) => { el.textContent = dollars(sums[i] || 0); });
        const available = host.querySelector("[data-available]");
        if (available) available.innerHTML = `<small>${text("Available (USD)")}</small>${dollars(sums[3] || 0).replace("USD ", "")}`;
      }
    }
    if (page === "02-new-claim" && config) {
      const line = budgets.budget_lines.find(line => line.id === sessionStorage.getItem("budget")) || budgets.budget_lines[0];
      const values = host.querySelectorAll(".event-current > span");
      if (values[1]) values[1].textContent = pawpaidCopy(line?.category || "Not available");
      if (values[3]) values[3].textContent = dollars(line?.approved_cents || 0);
      if (values[5]) values[5].textContent = config.claimant_id;
    }
    if (record) {
      host.querySelectorAll<HTMLElement>("[data-claim-state]").forEach(panel => { panel.hidden = panel.dataset.claimState !== record.claim.state; });
      if (loadedAt.current) {
        const date = loadedAt.current;
        const pad = (value: number) => String(value).padStart(2, "0");
        const stamp = `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
        host.querySelectorAll(".source").forEach(source => {
          if (!source.textContent?.startsWith("Source:") && !source.textContent?.startsWith("來源")) return;
          const colon = locale === "en" ? ": " : "：";
          const stop = locale === "en" ? ". " : "。";
          source.textContent = text("Source") + colon + text(checkedProvider.current ? "Check sandbox payout result" : "Saved status") + stop + text("Last updated") + " " + stamp;
        });
      }
      const recipient = payees.payees.find(
        (p) => p.claimant_id === record.claim.claimant_id,
      );
      const facts: Record<string, string> = {
        "Clinic payee": recipient?.display_name || "Not available",
        "Current stage": statusLabels[record.claim.state],
        "Approver ID": record.approval?.approver_id || "Not available",
        "Recipient name": recipient?.display_name === "Synthetic volunteer" ? "Synthetic animal clinic" : recipient?.display_name || "Not available",
        "Sandbox email": recipient?.sandbox_email || "Not available",
        "Payee ID": recipient?.id || "Not available",
        "Claim ID": record.claim.id,
        Revision: String(record.claim.revision),
        "Receipt hash": record.claim.receipt_sha256,
        Merchant: record.claim.fields.merchant || "Not available",
        "Clinic invoice date": record.claim.fields.date || "Not available",
        "Claim amount": dollars(record.claim.amount_cents),
        Amount: dollars(record.claim.amount_cents),
        Claimant: record.claim.claimant_id,
        Category: pawpaidCopy(budgets.budget_lines.find(line => line.id === record.claim.budget_line_id)?.category || "Not available"),
        "Clinic estimate": dollars(budgets.budget_lines.find(line => line.id === record.claim.budget_line_id)?.approved_cents || 0),
        "Approved amount": dollars(record.approval?.amount_cents || 0),
        "Approval ID": record.approval?.id || "Not available",
        "Payout ID": record.payout?.id || "Not available",
        "Sender batch ID": record.payout?.sender_batch_id || "Not available",
        "Provider batch ID": record.payout?.paypal_batch_id || "No data",
        "Provider item ID": record.payout?.item_id || "No data",
        "Approved revision": String(record.approval?.claim_revision || 0),
        "Approval expires": record.approval
          ? new Date(record.approval.expires_at * 1000).toISOString()
          : "Not available",
      };
      host.querySelectorAll("dl > div").forEach((el) => {
        const key = el.querySelector("dt") ? originalLabel(el.querySelector("dt")!) : "";
        const dd = el.querySelector("dd");
        if (dd && facts[key] !== undefined) {
          const badge = dd.querySelector(".status");
          if (badge) {
            badge.textContent = facts[key];
            badge.className = "status" + (record.claim.state === "PayoutFailed" ? " status-danger" : record.claim.state === "PayoutUnknown" ? " status-open" : " status-accent");
          } else dd.textContent = facts[key];
        }
      });
      const line = budgets.budget_lines.find(line => line.id === record.claim.budget_line_id);
      const estimate = line?.approved_cents || 0;
      const remaining = line ? line.approved_cents - line.reserved_cents - line.paid_cents : 0;
      host.querySelectorAll(".money-stub dd").forEach((el, i) => { el.textContent = dollars([estimate, record.claim.amount_cents, remaining][i] || 0); });
      host.querySelectorAll(".ticket-head h2").forEach(el => { el.textContent = pawpaidCopy(line?.category || record.claim.fields.merchant || "Not available"); });
      host.querySelectorAll<HTMLButtonElement>('button[data-original-label="Approve claim"]').forEach(button => { button.textContent = text("Approve USD 7.50").replace("USD 7.50", dollars(record.claim.amount_cents)); });
      host.querySelectorAll(".recap").forEach(el => { el.textContent = `${record.claim.id} (${text("Revision")} ${record.claim.revision}): ${dollars(record.claim.amount_cents)}. ${text("Approving does not send payment.")}`; });
      host.querySelectorAll(".ticket-body > .id").forEach(el => { el.textContent = record.claim.id; el.setAttribute("title", record.claim.id); });
      host.querySelectorAll(".ticket-head .date").forEach(el => { el.textContent = record.claim.fields.date || text("Not available"); });
      host.querySelectorAll(".amount").forEach(el => {
        const label = el.querySelector("small");
        const amount = dollars(record.claim.fields.total_cents ?? record.claim.amount_cents);
        el.replaceChildren(...(label ? [label, amount.replace("USD ", "")] : [amount]));
      });
      if (
        page === "03-review" &&
        boundFields.current !==
          `${record.claim.id}:${record.claim.revision}:${index}`
      ) {
        boundFields.current = `${record.claim.id}:${record.claim.revision}:${index}`;
        const inputs = host.querySelectorAll<HTMLInputElement>(
          "input:not([type=checkbox])",
        );
        const values = [
          record.claim.fields.merchant || "",
          record.claim.fields.date || "",
          record.claim.fields.total_cents === null
            ? ""
            : (record.claim.fields.total_cents / 100).toFixed(2),
          record.claim.fields.currency || "USD",
          "1",
          record.claim.fields.total_cents === null
            ? ""
            : (record.claim.fields.total_cents / 100).toFixed(2),
        ];
        inputs.forEach((el, i) => {
          el.value = values[i] || "";
        });
      }
    }
    if (page === "03-review" && host.querySelector("#total")) {
      const form = host.querySelector("form");
      let summary = host.querySelector("[data-invoice-summary]");
      if (!summary) {
        summary = document.createElement("div");
        summary.setAttribute("data-invoice-summary", "true");
        host.querySelector(".review-grid > section:last-child")?.prepend(summary);
      }
      const updateTotals = () => {
        const value = (id: string) => host.querySelector<HTMLInputElement>(`#${id}`)?.value || "";
        const amount = (input: string) => { try { return parseCents(input); } catch { return null; } };
        const total = amount(value("total"));
        const unit = amount(value("unit"));
        const quantity = Number(value("qty"));
        const items = unit !== null && Number.isFinite(quantity) && quantity > 0 ? Math.round(unit * quantity) : null;
        const totalLabel = locale === "en" ? "Draft total" : "草稿總額";
        const itemLabel = locale === "en" ? "Calculated item total · " : "計算項目總額・";
        const pending = locale === "en" ? "Calculated by the server after submit" : "送出後由伺服器核算";
        if (summary) summary.textContent = `${totalLabel}${locale === "en" ? ": " : "："}${total === null ? text("Not available") : dollars(total)}${locale === "en" ? ". " : "。"}${pending}`;
        host.querySelectorAll("[data-item-total]").forEach(el => { el.textContent = itemLabel + (items === null ? text("Not available") : dollars(items)); });
      };
      if (form) form.oninput = updateTotals;
      updateTotals();
    }
    if (record?.payout) {
      const batch = record.payout.paypal_batch_id;
      if (!host.querySelector("[data-payout-mode]")) {
        const evidence = document.createElement("p");
        evidence.setAttribute("data-payout-mode", "true");
        host.querySelector(".outcome h2")?.after(evidence);
      }
      host.querySelectorAll("[data-payout-mode]").forEach(el => {
        el.textContent = batch
          ? (locale === "en" ? `A PayPal sandbox payout was sent. Batch ID: ${batch}` : `已送出 PayPal 沙盒付款。批次編號：${batch}`)
          : config?.paypal_mode === "mock" ? text("Mock payout. No sandbox payment was sent.")
          : locale === "en" ? "No PayPal sandbox batch ID is available yet." : "目前尚無 PayPal 沙盒批次編號。";
      });
    }
    if (page === "05-finance-queue" && queue.length) {
      const stack = host.querySelector("main > .stack");
      const prototype = stack?.querySelector(".ticket");
      if (stack && prototype) {
        stack.replaceChildren();
        for (const saved of queue) {
          const copy = prototype.cloneNode(true);
          if (!(copy instanceof HTMLElement)) continue;
          const heading = copy.querySelector("h2");
          if (heading) heading.textContent = saved.claim.fields.merchant || text("Not available");
          const date = copy.querySelector(".date");
          if (date) date.textContent = saved.claim.fields.date || text("Not available");
          const id = copy.querySelector(".ticket-body > .id");
          if (id) id.textContent = saved.claim.id;
          const values = [saved.claim.claimant_id, "Animal case · clinic estimate", statusLabels[saved.claim.state]];
          copy.querySelectorAll("dd").forEach((dd, i) => { dd.textContent = values[i] || ""; });
          const amount = copy.querySelector(".amount");
          if (amount) amount.textContent = dollars(saved.claim.amount_cents);
          const link = copy.querySelector<HTMLAnchorElement>("a.btn");
          if (link) link.href = `/06-finance-review.html?claim=${saved.claim.id}`;
          stack.append(copy);
        }
      }
    }
    host.querySelectorAll<HTMLLIElement>("li").forEach((li) => {
      const label = li.querySelector("button") ? originalLabel(li.querySelector("button")!) : "";
      if (label === "Mark resolved") li.hidden = uncertaintyResolved.current;
      if (label === "Mark unresolved") li.hidden = !uncertaintyResolved.current;
    });
    host.querySelectorAll<HTMLButtonElement>("button:not(.locale-button)").forEach((b) => {
      if (
        originalLabel(b).includes("Submit claim") ||
        originalLabel(b).includes("Approve claim")
      ) {
        const checkbox = host.querySelector<HTMLInputElement>(
          "input[type=checkbox]",
        );
        b.disabled =
          !checkbox?.checked ||
          (page === "03-review" && !uncertaintyResolved.current);
        if (checkbox)
          checkbox.onchange = () => {
            b.disabled =
              !checkbox.checked ||
              (page === "03-review" && !uncertaintyResolved.current);
          };
      }
    });
    if (busy)
      host
        .querySelectorAll<HTMLButtonElement>("button:not(.locale-button)")
        .forEach((b) => (b.disabled = true));
    host.querySelector("[data-reconciliation]")?.remove();
    if (record?.payout?.reconciliation.length) {
      const warning = document.createElement("p");
      warning.dataset.reconciliation = "true";
      warning.setAttribute("role", "alert");
      warning.textContent = "Provider evidence needs finance reconciliation. This animal case is frozen. Do not pay again.";
      host.querySelector("main")?.prepend(warning);
      host.querySelectorAll<HTMLButtonElement>("button:not(.locale-button)").forEach((button) => {
        if (["Send mock payout", "Check payment status"].includes(originalLabel(button) || ""))
          button.disabled = true;
      });
    }
    host.querySelectorAll<HTMLAnchorElement>("[data-signout]").forEach(link => {
      link.onclick = () => { sessionStorage.removeItem("finance"); sessionStorage.removeItem("claimant"); };
    });
    host.querySelectorAll(".chip").forEach(chip => { if (/^[A-Z_]+$/.test(chip.textContent || "")) chip.setAttribute("data-literal", "true"); });
    host.dataset.error = error;
    host.querySelector("[data-api-error]:not(.alert-code)")?.remove();
    if (error && !(page === "05-finance-queue" && error === "ROLE_REQUIRED")) {
      const existing = host.querySelector('.ticket.solo:has(.ticket-rail-danger)');
      const existingChip = existing?.querySelector<HTMLElement>('.chip');
      if (existingChip) {
        existingChip.textContent = error;
        existingChip.dataset.errorCode = error;
        existingChip.dataset.literal = "true";
        existingChip.closest(".alert-code")?.setAttribute("data-api-error", "true");
        const explanation = existing?.querySelector("details p");
        if (explanation) explanation.textContent = text(error);
        return;
      }
      const alert = document.createElement("p");
      alert.dataset.apiError = "true";
      alert.setAttribute("role", "alert");
      alert.textContent = text(error) === error ? text("UNAVAILABLE") : text(error);
      const chip = document.createElement("code");
      chip.className = "chip";
      chip.dataset.errorCode = error;
      chip.textContent = error;
      alert.append(chip);
      host.querySelector("main")?.prepend(alert);
    }
  }, [index, record, budgets, queue, busy, ready, error, payees, config, locale]);
  useEffect(() => { if (root.current) localize(root.current); });
  async function action(label: string, button: HTMLButtonElement) {
    if (label === "Mark resolved" || label === "Mark unresolved") {
      uncertaintyResolved.current = label === "Mark resolved";
      return;
    }
    if (label === "Open claim") return;
    if (label === "Use mock text fixture") {
      const field = root.current?.querySelector("textarea");
      if (field) field.value = "mock:clean";
      return;
    }
    if (label === "Use this budget line") {
      if (!ready) return;
      sessionStorage.setItem("budget", button.dataset.budget || "");
      go("02-new-claim");
      return;
    }
    if (
      label === "Refresh budget" ||
      label === "Reload configuration" ||
      label === "Refresh saved status"
    ) {
      setError("");
      await refresh();
      return;
    }
    if (
      label === "Create claim and extract" ||
      label === "Create claim for manual entry"
    ) {
      if (!config) throw new ApiFailure("UNAVAILABLE");
      const text = root.current?.querySelector("textarea")?.value || "";
      if (!text.trim()) throw new ApiFailure("INVALID_INPUT");
      const line =
        budgets.budget_lines.find(
          (l) => l.id === sessionStorage.getItem("budget"),
        ) || budgets.budget_lines[0];
      if (!line) throw new ApiFailure("NOT_FOUND");
      const saved = await request("/claims", responseSchema, {
        event_id: config.event_id,
        budget_line_id: line.id,
        claimant_id: config.claimant_id,
        receipt_text: text,
      });
      sessionStorage.setItem("claim", saved.claim.id);
      if (label === "Create claim and extract")
        await request(`/claims/${saved.claim.id}/extract`, responseSchema, {});
      go("03-review", saved.claim.id);
      return;
    }
    const id = record?.claim.id || storedId;
    if (!id) return;
    if (label === "Extract again") {
      show(await request(`/claims/${id}/extract`, responseSchema, {}));
      return;
    }
    if (label === "Submit claim") {
      if (!root.current?.querySelector<HTMLInputElement>("input[type=checkbox]")?.checked || !uncertaintyResolved.current) throw new ApiFailure("INVALID_INPUT");
      const inputs = root.current?.querySelectorAll<HTMLInputElement>(
        "input:not([type=checkbox])",
      );
      if (!inputs) return;
      const values = Array.from(inputs, (i) => i.value);
      const [merchant, date, total, currency, quantity, unit] = values;
      if (
        !merchant ||
        !date ||
        !total ||
        currency !== "USD" ||
        !quantity ||
        !unit ||
        !/^\d+$/.test(quantity) ||
        Number(quantity) < 1
      )
        throw new ApiFailure("INVALID_INPUT");
      await request(`/claims/${id}/submit`, responseSchema, {
        fields: {
          merchant,
          date,
          total_cents: parseCents(total),
          currency,
          uncertain: [],
        },
        items: [{ quantity: Number(quantity), unit_cents: parseCents(unit) }],
      });
      go("04-claim-status", id);
      return;
    }
    if (label === "Approve claim") {
      const token = sessionStorage.getItem("finance") || "";
      const payee = payees.payees.find(
        (p) => p.claimant_id === record?.claim.claimant_id,
      );
      if (!payee) throw new ApiFailure("PAYEE_NOT_ALLOWED");
      const saved = await request(
        `/claims/${id}/approve`,
        responseSchema,
        { payee_id: payee.id },
        token,
      );
      sessionStorage.setItem("finance", token);
      show(saved);
      return;
    }
    if (label === "Send sandbox payout" || label === "Send mock payout" || label === "Check payment status") {
      const saved = await request(`/claims/${id}/payout`, responseSchema, {});
      if (page === "07-payout-status") show(saved, true);
      else go("07-payout-status", id);
      return;
    }
  }
  return (
    <>
    <div
      ref={root}
      onClick={(event) => {
        if (gallery || busy) return;
        const button =
          event.target instanceof Element
            ? event.target.closest("button")
            : null;
        if (!(button instanceof HTMLButtonElement) || button.disabled) return;
        setBusy(true);
        void action(originalLabel(button) || "", button)
          .catch(failed)
          .finally(() => setBusy(false));
      }}
      dangerouslySetInnerHTML={{
        __html: gallery
          ? (params.has("shared") && isShared(params.get("shared") || "") ? sharedTemplates[sharedName()] : params.has("state") ? pawpaidTemplates[page][Number(params.get("state"))] || "" : pawpaidTemplates[page].map(html => `<div class="gallery-state">${html}</div>`).join(""))
          : page === "05-finance-queue" && error === "ROLE_REQUIRED"
            ? sharedTemplates["s-permission-nonfinance"]
            : error && !record && (page === "04-claim-status" || page === "07-payout-status")
              ? sharedTemplates["s-error"]
              : runtimeTemplates[page]?.[index] || runtimeTemplates[page]?.[0] || "",
      }}
    />
    {switchHost && createPortal(switcher, switchHost)}
    </>
  );
}
function JudgeApp() {
  const [locale, updateLocale] = useState<Locale>(initialLocale);
  const [signedIn, setSignedIn] = useState(gallery && !params.has("signin"));
  useEffect(() => {
    setLocale(locale);
    const titles: Record<typeof page, string> = {
      "01-budget": "Animal cases", "02-new-claim": "New clinic invoice",
      "03-review": "Review clinic invoice", "04-claim-status": "Clinic invoice status",
      "05-finance-queue": "Approval queue", "06-finance-review": "Finance review",
      "07-payout-status": "Payout status",
    };
    document.title = text(signedIn ? `Pawpaid · ${titles[page]}` : "Pawpaid · Judge sign-in");
    document.querySelectorAll<HTMLElement>("[data-app-content]").forEach(localize);
  });
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  type SigninState = "initial" | "credentials" | "network" | "invalid-response" | "restore-failed";
  const fixture = params.get("signin");
  const initialSignin: SigninState = gallery && (fixture === "credentials" || fixture === "network" || fixture === "invalid-response" || fixture === "restore-failed") ? fixture : "initial";
  const [signinState, setSigninState] = useState<SigninState>(initialSignin);
  const [signingIn, setSigningIn] = useState(false);
  useEffect(() => {
    if (sessionStorage.getItem("finance") || sessionStorage.getItem("claimant")) {
      void request("/config", configSchema).then(() => setSignedIn(true)).catch(() => {
        sessionStorage.removeItem("finance"); sessionStorage.removeItem("claimant");
        setSignedIn(false); setSigninState("restore-failed");
      });
    }
  }, []);
  const switcher = <div className="locale-switch" data-locale-switch role="group" aria-label={text("Language")}>
    <button className="locale-button" type="button" lang="zh-Hant" aria-pressed={locale === "zh-Hant"} onClick={() => { setLocale("zh-Hant"); updateLocale("zh-Hant"); }}>繁中</button>
    <span aria-hidden="true"> / </span>
    <button className="locale-button" type="button" lang="en" aria-pressed={locale === "en"} onClick={() => { setLocale("en"); updateLocale("en"); }}>EN</button>
  </div>;
  if (signedIn) return <div data-app-content><App locale={locale} switcher={switcher} /></div>;
  const failure = {
    credentials: ["Wrong account or password", "Enter them again."],
    network: ["Sign-in is unavailable right now", "The service did not respond or is temporarily unavailable. Your account and password were not rejected. Try again in a moment."],
    "invalid-response": ["Invalid service response", "The service answered, but the response could not be read, so you are not signed in. Try again."],
  } satisfies Record<"credentials" | "network" | "invalid-response", readonly [string, string]>;
  const notice = signinState === "credentials" || signinState === "network" || signinState === "invalid-response" ? failure[signinState] : null;
  const form = signinState === "restore-failed" ? <>
    <p>{text("Sign in again to continue.")}</p><div className="actions"><button className="btn btn-primary" onClick={() => setSigninState("initial")}>{text("Sign in")}</button></div>
  </> : <form className="form" onSubmit={event => {
    event.preventDefault();
    if (signingIn || gallery) return;
    setSigningIn(true);
    void fetch("/api/session", {method: "POST", headers: {"Content-Type": "application/json"}, body: JSON.stringify({username, password})})
      .then(async response => {
        if (!response.ok) { setSigninState(response.status === 401 ? "credentials" : "network"); return; }
        try {
          const data = z.object({token: z.string().min(1), role: z.enum(["finance", "claimant"])}).parse(await response.json());
          sessionStorage.removeItem("finance"); sessionStorage.removeItem("claimant");
          sessionStorage.setItem(data.role, data.token); setPassword(""); setSignedIn(true);
        } catch { setSigninState("invalid-response"); }
      }).catch(() => setSigninState("network")).finally(() => setSigningIn(false));
  }}>
    {notice && <div className="alert" role="alert" id="signin-error"><h3>{text(notice[0])}</h3><p>{text(notice[1])}</p>{signinState === "invalid-response" && <p className="alert-code"><span className="chip" data-literal>INVALID_RESPONSE</span></p>}</div>}
    <div className="field"><label htmlFor="account">{text("Account")}</label><input className="field-input" id="account" autoComplete="username" aria-invalid={signinState === "credentials" || undefined} aria-describedby={notice ? "signin-error" : undefined} value={username} onChange={event => setUsername(event.target.value)} /></div>
    <div className="field"><label htmlFor="password">{text("Password")}</label><input id="password" type="password" autoComplete="current-password" required aria-invalid={signinState === "credentials" || undefined} aria-describedby={notice ? "signin-error" : undefined} value={password} onChange={event => setPassword(event.target.value)} /></div>
    <p className="field-foot"><span>{text("Valid for 24 hours after sign-in.")}</span></p>
    <div className="actions"><button className="btn btn-primary" type="submit" disabled={signingIn}>{text(signinState === "network" || signinState === "invalid-response" ? "Try again" : "Sign in")}</button></div>
  </form>;
  return <ApprovedSignin form={form} switcher={switcher} restoreFailed={signinState === "restore-failed"} />;
}
setLocale(initialLocale());
const mount = document.getElementById("root");
if (mount)
  createRoot(mount).render(
    <React.StrictMode>
      <JudgeApp />
    </React.StrictMode>,
  );
