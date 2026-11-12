import { z } from "zod";
const id = z
  .string()
  .regex(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i);
const cents = z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER);
export const fieldsSchema = z.object({
  merchant: z.string().nullable(),
  date: z.string().nullable(),
  total_cents: cents.nullable(),
  currency: z.literal("USD").nullable(),
  uncertain: z.array(z.string()),
});
const claimSchema = z.object({
  id,
  event_id: id,
  claimant_id: id,
  budget_line_id: id,
  receipt_sha256: z.string(),
  fields: fieldsSchema,
  amount_cents: cents,
  revision: z.number().int(),
  state: z.enum([
    "Draft",
    "NeedsInfo",
    "Submitted",
    "DuplicateBlocked",
    "OverBudget",
    "Approved",
    "PayoutPending",
    "Paid",
    "PayoutFailed",
    "PayoutUnknown",
  ]),
});
const approvalSchema = z.object({
  id,
  claim_id: id,
  claim_revision: z.number(),
  approver_id: id,
  payee_id: id,
  amount_cents: cents,
  receipt_sha256: z.string(),
  expires_at: z.number(),
});
const payoutSchema = z.object({
  id,
  claim_id: id,
  approval_id: id,
  sender_batch_id: z.string(),
  paypal_batch_id: z.string().nullable(),
  item_id: z.string().nullable(),
  state: z.enum(["PayoutPending", "Paid", "PayoutFailed", "PayoutUnknown"]),
  reconciliation: z.array(z.object({
    source: z.string(),
    status: z.string(),
    paypal_batch_id: z.string().nullable(),
    item_id: z.string().nullable(),
  })).default([]),
});
export const responseSchema = z.object({
  claim: claimSchema,
  approval: approvalSchema.nullable(),
  payout: payoutSchema.nullable(),
});
export type ClaimResponse = z.infer<typeof responseSchema>;
export const configSchema = z.object({
  paypal_mode: z.enum(["mock", "sandbox"]),
  llm_mode: z.enum(["mock", "manual", "gemini-free"]),
  event_id: id,
  claimant_id: id,
});
export const budgetsSchema = z.object({
  budget_lines: z.array(
    z.object({
      id,
      event_id: id,
      category: z.string(),
      approved_cents: cents,
      reserved_cents: cents,
      paid_cents: cents,
    }),
  ),
});
export type Budgets = z.infer<typeof budgetsSchema>;
export const payeesSchema = z.object({
  payees: z.array(
    z.object({
      id,
      claimant_id: id,
      display_name: z.string(),
      sandbox_email: z.string(),
    }),
  ),
});
export const queueSchema = z.object({ claims: z.array(responseSchema) });
export class ApiFailure extends Error {
  constructor(readonly code: string) {
    super(code);
  }
}
export async function request<T>(
  path: string,
  schema: z.ZodType<T>,
  body?: unknown,
  token?: string,
): Promise<T> {
  token ??= sessionStorage.getItem("finance") || sessionStorage.getItem("claimant") || undefined;
  if (!token) throw new ApiFailure("FORBIDDEN");
  const response = await fetch(`/api${path}`, {
    method: body === undefined ? "GET" : "POST",
    headers: {
      "Content-Type": "application/json",
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  if (response.status === 403 || response.status === 401) {
    sessionStorage.removeItem("finance");
    sessionStorage.removeItem("claimant");
    location.reload();
    throw new ApiFailure("FORBIDDEN");
  }
  const data: unknown = await response.json();
  if (!response.ok) {
    const error = z.object({ code: z.string() }).safeParse(data);
    throw new ApiFailure(error.success ? error.data.code : "INVALID_RESPONSE");
  }
  return schema.parse(data);
}
export function dollars(value: number) {
  return `USD ${(value / 100).toFixed(2)}`;
}
export function parseCents(value: string) {
  if (!/^\d+(\.\d{1,2})?$/.test(value)) throw new ApiFailure("INVALID_INPUT");
  const [whole, fraction = ""] = value.split(".");
  return cents.parse(Number(whole) * 100 + Number(fraction.padEnd(2, "0")));
}
