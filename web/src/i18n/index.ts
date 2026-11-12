import { strings, type Locale } from "./strings";
export type { Locale } from "./strings";
export type Key = (typeof strings)[number]["key"];
type Dictionary = Record<Key, string>;
function dictionary(locale: Locale): Dictionary {
  // Every entry requires both locales; both dictionaries derive from that same key set.
  return Object.fromEntries(strings.map(entry => [entry.key, entry[locale]])) as Dictionary;
}
export const dictionaries = { en: dictionary("en"), "zh-Hant": dictionary("zh-Hant") } satisfies Record<Locale, Dictionary>;
export function initialLocale(): Locale {
  try {
    const saved = localStorage.getItem("pawpaid.locale");
    if (saved === "en" || saved === "zh-Hant") return saved;
  } catch { /* Storage can be disabled by the browser. */ }
  return navigator.language.toLowerCase().startsWith("zh") ? "zh-Hant" : "en";
}
let currentLocale = initialLocale();
export function setLocale(locale: Locale) {
  currentLocale = locale;
  document.documentElement.lang = locale;
  try { localStorage.setItem("pawpaid.locale", locale); } catch { /* Keep switching available without storage. */ }
}
export function t(key: Key): string { return dictionaries[currentLocale][key]; }
const byEnglish = new Map<string, Key>(strings.map(entry => [entry.en, entry.key]));
const codes: Record<string, Key> = {
  "NOT_FOUND": "01-budget.not-found",
  "STORAGE_UNAVAILABLE": "01-budget.storage-unavailable",
  "INVALID_INPUT": "02-new-claim.invalid-input",
  "PAYEE_NOT_ALLOWED": "02-new-claim.payee-not-allowed",
  "NEEDS_INFO": "03-review.needs-info",
  "FORBIDDEN": "06-finance-review.forbidden",
  "INVALID_TRANSITION": "06-finance-review.invalid-transition",
  "ADAPTER_UNAVAILABLE": "07-payout-status.adapter-unavailable",
  "UNAVAILABLE": "all.unavailable",
  "INVALID_RESPONSE": "all.invalid-response",
  "DUPLICATE_RECEIPT": "all.duplicate-receipt",
  "OVER_BUDGET": "all.over-budget",
  "APPROVAL_INVALID": "all.approval-invalid"
};
const byChinese = new Map<string, Key>(strings.map(entry => [entry["zh-Hant"], entry.key]));
export function text(value: string): string {
  const key = codes[value.trim()] || byEnglish.get(value.trim()) || byChinese.get(value.trim());
  return key ? value.replace(value.trim(), t(key)) : value;
}
const controls = new WeakMap<HTMLInputElement | HTMLTextAreaElement, { original: string; rendered: string }>();
const originals = new WeakMap<Node, { original: string; rendered: string }>();
const attributes = new WeakMap<Element, Map<string, { original: string; rendered: string }>>();
export function localize(host: HTMLElement) {
  const walker = document.createTreeWalker(host, NodeFilter.SHOW_TEXT);
  while (walker.nextNode()) {
    const node = walker.currentNode;
    if (node.parentElement?.closest('textarea, script, style, [data-locale-switch], [data-literal], [data-error-code]')) continue;
    const value = node.textContent || "";
    const saved = originals.get(node);
    const original = saved?.rendered === value ? saved.original : value;
    const rendered = text(original);
    node.textContent = rendered;
    originals.set(node, { original, rendered });
  }
  host.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>('input:not([type="password"]), textarea').forEach(control => {
    const saved = controls.get(control);
    // Preserve user edits and machine-readable fixtures. Translate only unchanged demo values.
    if (saved && saved.rendered !== control.value) return;
    if (!saved && control.value !== control.defaultValue) return;
    const original = saved?.original || control.value;
    const rendered = text(original);
    control.value = rendered;
    controls.set(control, { original, rendered });
  });
  host.querySelectorAll<HTMLElement>("[aria-label], [placeholder], [title], [data-label], option").forEach(element => {
    const saved = attributes.get(element) || new Map();
    for (const name of ["aria-label", "placeholder", "title", "data-label"]) {
      const value = element.getAttribute(name);
      if (!value) continue;
      const prior = saved.get(name);
      const original = prior?.rendered === value ? prior.original : value;
      const rendered = text(original);
      element.setAttribute(name, rendered);
      saved.set(name, { original, rendered });
    }
    attributes.set(element, saved);
  });
}

export function modeBanner(mode: string, entry: string): string {
  return t("all.mode-banner")
    .replace("{mode}", text(mode))
    .replace("{entry}", text(entry))
    .replace("{notice}", text("Synthetic data only. No real money."));
}
