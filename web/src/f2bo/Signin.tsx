import type { ReactNode } from "react";
import { text } from "../i18n";

export function ApprovedSignin({ form, switcher, restoreFailed = false }: {
  form: ReactNode;
  switcher: ReactNode;
  restoreFailed?: boolean;
}) {
  return <div className="f2-signin">
    <a className="skip" href="#main">{text("Skip to main content")}</a>
    <div className="sandbox-banner" role="status">{text("Test mode (demo). PayPal: sandbox. AI: mock. No real money. Sandbox payouts: maximum USD 10.00.")}</div>
    <header className="masthead"><span className="wordmark">Pawpaid</span><span className="masthead-account">{switcher}</span></header>
    <div className="layout">
      <aside className="aside"><h1>{text(restoreFailed ? "Sign in again" : "Sign in")}</h1>{!restoreFailed && <p className="aside-lede">{text("Synthetic clinic invoices only. Do not upload real data. One case is one animal, with a clinic estimate and clinic invoice. Payout goes to the clinic payee registered by the foster or rescue group.")}</p>}</aside>
      <main id="main"><div className="stack stack-narrow"><article className="ticket solo" aria-label={text(restoreFailed ? "Your saved sign-in could not be restored" : "Sign in")}><span className="rail" aria-hidden="true" />
        <div className="ticket-body"><h2>{text(restoreFailed ? "Your saved sign-in could not be restored" : "Sign in with your account")}</h2>{form}
          {!restoreFailed && <p className="org-line">{text("Need an account? Ask the organizer for an account.")}</p>}
        </div>
      </article></div></main>
    </div>
  </div>;
}
