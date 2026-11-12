import { useLayoutEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { initialLocale, setLocale, type Locale } from "../i18n";
import fixtures from "./fixtures.json";
import "./public.css";
import "./fonts.css";

const routes = {
  "01-case": "/case",
  "02-rescue-timeline": "/case/updates",
  "04-identity": "/case/identity",
};
const path = location.pathname.replace(/\/$/, "");
const screen = path === routes["04-identity"] ? "04-identity" : path === routes["02-rescue-timeline"] ? "02-rescue-timeline" : "01-case";
const query = new URLSearchParams(location.search);
const requestedLocale = query.get("lang");
const startingLocale = requestedLocale === "en" || requestedLocale === "zh-Hant" ? requestedLocale : initialLocale();

const statePages: Record<string, string> = {
  "01-case:loading": "01-case-loading", "01-case:error": "01-case-error",
  "01-case:notfound": "01-case-notfound", "01-case:image-failed": "01-case-image-failed",
  "02-rescue-timeline:loading": "02-updates-loading", "02-rescue-timeline:error": "02-updates-error",
  "02-rescue-timeline:notfound": "02-updates-notfound",
  ...Object.fromEntries(["loading", "error", "draft-saved", "publish-locked", "verified", "verify-failed", "verify-pending"].map(state => [`04-identity:${state}`, `04-identity-${state}`])),
};
const fixtureKey = statePages[`${screen}:${query.get("state")}`];
const fixture = Object.entries(fixtures).find(([key]) => key === fixtureKey)?.[1] ?? fixtures[screen];

function PublicApp() {
  const [locale, updateLocale] = useState<Locale>(startingLocale);
  const host = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    setLocale(locale);
    document.title = fixture[locale].title;
    const root = host.current;
    if (!root) return;
    root.querySelectorAll<HTMLAnchorElement>("a").forEach(link => {
      const file = link.getAttribute("href") || "";
      if (file.startsWith("#")) return;
      const target = file.replace(/-en\.html$|\.html$/, "");
      const destination = (target === "02-rescue-timeline" || target.startsWith("02-updates-")) ? routes["02-rescue-timeline"] : target.startsWith("04-identity") ? routes["04-identity"] : routes["01-case"];
      const nextLocale = link.lang === "en" || link.lang === "zh-Hant" ? link.lang : locale;
      const params = new URLSearchParams({ lang: nextLocale });
      if (link.closest(".lang") && query.has("state")) params.set("state", query.get("state") || "");
      link.href = `${destination}?${params}`;
    });
    // The donation card is a visual fixture. It has no intake or submit action.
    root.querySelectorAll<HTMLButtonElement>(".donate button").forEach(button => {
      button.type = "button";
      button.setAttribute("aria-disabled", "true");
    });
    // Identity fixtures demonstrate the approved states without collecting data.
    root.querySelectorAll<HTMLInputElement>(".field input").forEach(input => { input.readOnly = true; });
    const state = query.get("state")?.toUpperCase();
    if (screen === "04-identity" && state && ["A", "B", "C", "D"].includes(state)) {
      root.querySelectorAll<HTMLElement>(".st").forEach((card, index) => { card.hidden = state !== ["A", "B", "C"][index]; });
      const thanks = root.querySelector<HTMLElement>(".thanks");
      if (thanks) thanks.hidden = state !== "D";
    }
    root.querySelectorAll<HTMLImageElement>("img").forEach(img => {
      img.onerror = () => {
        const fallback = document.createElement("span");
        fallback.className = "win soft";
        fallback.setAttribute("role", "img");
        fallback.setAttribute("aria-label", img.alt);
        fallback.textContent = locale === "en" ? "Demo photo unavailable" : "示範照片暫時無法顯示";
        img.replaceWith(fallback);
      };
    });
  }, [locale]);
  return <div className="public-page" data-screen={screen} lang={locale} ref={host}
    onClick={event => {
      const link = event.target instanceof Element ? event.target.closest<HTMLAnchorElement>(".lang a") : null;
      if (!link || (link.lang !== "en" && link.lang !== "zh-Hant")) return;
      event.preventDefault();
      const nextLocale = link.lang;
      setLocale(nextLocale);
      history.replaceState(null, "", link.href);
      updateLocale(nextLocale);
    }}
    dangerouslySetInnerHTML={{ __html: fixture[locale].html }} />;
}

const mount = document.getElementById("root");
if (mount) createRoot(mount).render(<PublicApp />);
