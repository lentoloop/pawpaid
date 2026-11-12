//! Only verified item facts enter this transaction. Verified batch notifications are ignored.
use crate::domain::{self, Ledger, Observation, Output};
use contracts::*;
use serde_json::Value;

/// Only payout item notifications may enter deduplication or financial reconciliation.
pub(crate) fn supported_event(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .is_some_and(|event| {
            event["event_type"]
                .as_str()
                .is_some_and(|t| t.starts_with("PAYMENT.PAYOUTS-ITEM."))
        })
}

pub fn apply(
    ledger: &mut Ledger,
    delivery_id: &str,
    verified: bool,
    confirmation: Option<Observation>,
) -> Result<Output, ApiError> {
    let delivery = ledger
        .inbox
        .get(delivery_id)
        .ok_or_else(domain::not_found)?
        .clone();
    if delivery.status != "pending" && delivery.status != "unmatched" {
        return Ok(Output::Reconciled(delivery.status));
    }
    let mut status = "rejected";
    let mut claim_id = None;
    if verified {
        if let Ok(event) = serde_json::from_slice::<Value>(&delivery.body) {
            if !supported_event(&delivery.body) {
                ledger.inbox.get_mut(delivery_id).unwrap().status = "ignored".into();
                return Ok(Output::Reconciled("ignored".into()));
            }
            if let Some(event_id) = event["id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256)
            {
                let key = format!("sandbox:{event_id}");
                if let Some(hash) = ledger
                    .sandbox_events
                    .get(&key)
                    .filter(|hash| *hash != &delivery.hash || delivery.status != "unmatched")
                {
                    status = if hash == &delivery.hash {
                        "replay"
                    } else {
                        "conflict"
                    };
                } else {
                    // The dedupe insert and observation/accounting commit share the locked transaction.
                    ledger.sandbox_events.insert(key, delivery.hash.clone());
                    status = "unmatched";
                    let resource = &event["resource"];
                    let sender = resource["payout_item"]["sender_item_id"].as_str();
                    let payout = ledger
                        .payouts
                        .values()
                        .find(|p| {
                            p.binding
                                .as_ref()
                                .is_some_and(|b| Some(b.sender_item_id.as_str()) == sender)
                        })
                        .cloned()
                        .or_else(|| {
                            // A corrupt sender may still be attributable through an established provider ID.
                            let mut matches = ledger.payouts.values().filter(|p| {
                                p.paypal_batch_id
                                    .as_deref()
                                    .is_some_and(|id| resource["payout_batch_id"] == id)
                                    || p.item_id
                                        .as_deref()
                                        .is_some_and(|id| resource["payout_item_id"] == id)
                            });
                            let first = matches.next()?.clone();
                            matches.next().is_none().then_some(first)
                        });
                    if let Some(payout) = payout.filter(|p| {
                        ledger
                            .requests
                            .get(&p.claim_id)
                            .is_some_and(|r| r.environment == "sandbox")
                    }) {
                        claim_id = Some(payout.claim_id);
                        // A verified delivery supplies a candidate batch ID, never proof by itself.
                        // details() must bind the batch header and item to the immutable request.
                        let mut candidate = ledger.clone();
                        let recovery = payout.paypal_batch_id.is_none();
                        let recovery_proof = confirmation.as_ref().filter(|q| {
                            q.source.starts_with("sandbox-item-query:")
                                && q.paypal_batch_id.as_deref()
                                    == resource["payout_batch_id"].as_str()
                                && q.item_id.as_deref() == resource["payout_item_id"].as_str()
                        });
                        let query_mismatch = confirmation.as_ref().is_some_and(|q| {
                            q.source.starts_with("sandbox-item-query:")
                                && matches!(
                                    q.status.as_str(),
                                    "Mismatch" | "Refunded" | "Reversed" | "Unknown"
                                )
                        });
                        let recovered = !query_mismatch
                            && (!recovery
                                || recovery_proof.is_some_and(|q| {
                                    // Validate the event's complete facts before applying even a query result.
                                    let mut known = payout.clone();
                                    known.paypal_batch_id = q.paypal_batch_id.clone();
                                    normalize(&known, resource, &event["event_type"], event_id)
                                        .is_some()
                                        && domain::observe(
                                            &mut candidate,
                                            payout.claim_id,
                                            q.clone(),
                                        )
                                        .is_ok()
                                }));
                        let current = candidate.payouts[&payout.claim_id].clone();
                        if recovered {
                            if let Some(observation) =
                                normalize(&current, resource, &event["event_type"], event_id)
                            {
                                let needs_query = observation.state == PayoutState::PayoutFailed
                                    && payout.state != PayoutState::PayoutFailed
                                    && payout.state != PayoutState::Paid;
                                let proof = confirmation.as_ref().is_some_and(|q| {
                                    q.state == PayoutState::PayoutFailed
                                        && q.status == "Failed"
                                        && q.paypal_batch_id == observation.paypal_batch_id
                                        && q.item_id == observation.item_id
                                        && serde_json::to_value(&q.binding).ok()
                                            == serde_json::to_value(&observation.binding).ok()
                                        && q.source.starts_with("sandbox-item-query:")
                                });
                                if (!needs_query || proof)
                                    && domain::observe(&mut candidate, payout.claim_id, observation)
                                        .is_ok()
                                {
                                    *ledger = candidate;
                                    status = "applied";
                                } else {
                                    status = "mismatch";
                                }
                            } else {
                                status = "mismatch";
                            }
                        } else {
                            status = if confirmation.is_none() {
                                "unmatched"
                            } else {
                                "mismatch"
                            };
                        }
                        if let Some(query) = confirmation.as_ref() {
                            ledger.evidence.entry(format!("webhook-query:{}", delivery.id)).or_insert_with(||
                                serde_json::json!({"claim_id":payout.claim_id, "delivery_id":delivery.id, "observation":query}));
                        }
                        if status == "mismatch" {
                            let record = ReconciliationRecord {
                                source: format!("sandbox-verified:{event_id}"),
                                status: "Mismatch".into(),
                                paypal_batch_id: resource["payout_batch_id"]
                                    .as_str()
                                    .map(str::to_owned),
                                item_id: resource["payout_item_id"].as_str().map(str::to_owned),
                            };
                            let p = ledger.payouts.get_mut(&payout.claim_id).unwrap();
                            if !p.reconciliation.contains(&record) {
                                p.reconciliation.push(record);
                            }
                        }
                    }
                }
            }
        }
    }
    let row = ledger.inbox.get_mut(delivery_id).unwrap();
    row.status = status.into();
    row.claim_id = claim_id;
    Ok(Output::Reconciled(status.into()))
}

fn normalize(p: &Payout, r: &Value, event_type: &Value, event_id: &str) -> Option<Observation> {
    let b = p.binding.as_ref()?;
    let batch = r["payout_batch_id"].as_str().filter(|s| !s.is_empty())?;
    let item = r["payout_item_id"].as_str().filter(|s| !s.is_empty())?;
    let facts = &r["payout_item"];
    let expected = crate::sandbox::request_body(p);
    if p.paypal_batch_id.as_deref() != Some(batch)
        || p.item_id.as_deref().is_some_and(|id| id != item)
        || facts["sender_item_id"] != b.sender_item_id
        || facts["receiver"] != b.receiver
        || facts["recipient_type"] != b.recipient_type
        || facts["amount"] != expected["items"][0]["amount"]
    {
        return None;
    }
    let raw = r["transaction_status"].as_str()?;
    let suffix = match raw {
        "SUCCESS" => "SUCCEEDED",
        "PENDING" => "PENDING",
        "PROCESSING" => "PROCESSING",
        "UNCLAIMED" => "UNCLAIMED",
        "FAILED" => "FAILED",
        "BLOCKED" => "BLOCKED",
        "ONHOLD" => "HELD",
        "CANCELED" => "CANCELED",
        "DENIED" => "DENIED",
        "RETURNED" => "RETURNED",
        "REFUNDED" => "REFUNDED",
        "REVERSED" => "REVERSED",
        _ => return None,
    };
    if event_type.as_str()? != format!("PAYMENT.PAYOUTS-ITEM.{suffix}") {
        return None;
    }
    let (state, status) = match raw {
        "SUCCESS" => (PayoutState::Paid, "Succeeded"),
        "FAILED" | "BLOCKED" | "DENIED" | "RETURNED" | "CANCELED" => {
            if p.state == PayoutState::Paid {
                (p.state, "Reversed")
            } else {
                (PayoutState::PayoutFailed, "Failed")
            }
        }
        "REFUNDED" => (p.state, "Refunded"),
        "REVERSED" => (p.state, "Reversed"),
        _ => (PayoutState::PayoutPending, "Pending"),
    };
    Some(Observation {
        state,
        status: status.into(),
        paypal_batch_id: Some(batch.into()),
        item_id: Some(item.into()),
        binding: Some(b.clone()),
        source: format!("sandbox-verified:{event_id}:{raw}"),
    })
}
