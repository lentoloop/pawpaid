use crate::{
    domain::{Command, Output},
    Service,
};
use axum::{
    body::Bytes,
    extract::{
        rejection::{BytesRejection, JsonRejection, PathRejection},
        DefaultBodyLimit, Path, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use contracts::*;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub struct HttpError(pub ApiError);
impl From<ApiError> for HttpError {
    fn from(error: ApiError) -> Self {
        Self(error)
    }
}
impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            ErrorCode::InvalidInput => StatusCode::BAD_REQUEST,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Forbidden => StatusCode::FORBIDDEN,
            ErrorCode::WebhookUnverified => StatusCode::UNAUTHORIZED,
            ErrorCode::NeedsInfo | ErrorCode::PayeeNotAllowed | ErrorCode::WebhookMismatch => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            ErrorCode::StorageUnavailable
            | ErrorCode::AdapterUnavailable
            | ErrorCode::ModeNotImplemented => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::CONFLICT,
        };
        let code = self.0.code;
        let mut response = (status, Json(self.0)).into_response();
        // Preserve the public error class when the outer boundary redacts diagnostics.
        response.extensions_mut().insert(code);
        response
    }
}
fn json<T: DeserializeOwned>(input: Result<Json<T>, JsonRejection>) -> Result<T, HttpError> {
    input.map(|Json(value)| value).map_err(|_| {
        HttpError(ApiError::new(
            ErrorCode::InvalidInput,
            "invalid JSON request",
        ))
    })
}
fn path(input: Result<Path<String>, PathRejection>) -> Result<String, HttpError> {
    input
        .map(|Path(value)| value)
        .map_err(|_| HttpError(ApiError::new(ErrorCode::InvalidInput, "invalid URL path")))
}
fn uuid(input: &str) -> Result<Uuid, HttpError> {
    Uuid::parse_str(input)
        .map_err(|_| HttpError(ApiError::new(ErrorCode::InvalidInput, "invalid UUID")))
}
pub fn router(service: Arc<Service>) -> Router {
    Router::new()
        .route("/api/config", get(public_config))
        .route("/api/events/{id}/claims", get(claims))
        .route("/api/claims/{id}/review", get(review))
        .route("/api/claims", post(create))
        .route("/api/claims/{id}", get(read))
        .route("/api/claims/{id}/extract", post(extract))
        .route("/api/claims/{id}/submit", post(submit))
        .route("/api/claims/{id}/approve", post(approve))
        .route("/api/claims/{id}/disposition", post(disposition))
        .route("/api/claims/{id}/payout", post(payout).get(query_payout))
        .route("/api/events/{id}/budget-lines", get(budgets))
        .route("/api/payees", get(payees))
        .route("/api/webhooks/paypal", post(webhook))
        .fallback(|| async { HttpError(ApiError::new(ErrorCode::NotFound, "route not found")) })
        .method_not_allowed_fallback(|| async {
            HttpError(ApiError::new(ErrorCode::InvalidInput, "method not allowed"))
        })
        .layer(axum::middleware::from_fn_with_state(
            service.clone(),
            boundary,
        ))
        .layer(DefaultBodyLimit::max(131_072))
        .with_state(service)
}
async fn create(
    State(service): State<Arc<Service>>,
    input: Result<Json<CreateClaimRequest>, JsonRejection>,
) -> Result<Json<ClaimResponse>, HttpError> {
    Ok(Json(
        service
            .claim(Command::Create {
                id: ClaimId::new(),
                request: json(input)?,
            })
            .await?,
    ))
}
async fn read(
    State(service): State<Arc<Service>>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<ClaimResponse>, HttpError> {
    Ok(Json(
        service
            .claim(Command::Get(ClaimId(uuid(&path(id)?)?)))
            .await?,
    ))
}
async fn extract(
    State(service): State<Arc<Service>>,
    id: Result<Path<String>, PathRejection>,
    input: Result<Json<ExtractClaimRequest>, JsonRejection>,
) -> Result<Json<ClaimResponse>, HttpError> {
    json(input)?;
    Ok(Json(service.extract(ClaimId(uuid(&path(id)?)?)).await?))
}
async fn submit(
    State(service): State<Arc<Service>>,
    id: Result<Path<String>, PathRejection>,
    input: Result<Json<SubmitClaimRequest>, JsonRejection>,
) -> Result<Json<ClaimResponse>, HttpError> {
    Ok(Json(
        service
            .claim(Command::Submit {
                id: ClaimId(uuid(&path(id)?)?),
                request: json(input)?,
            })
            .await?,
    ))
}
async fn disposition(
    State(service): State<Arc<Service>>,
    id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    input: Result<Json<crate::domain::DispositionRequest>, JsonRejection>,
) -> Result<Json<ClaimResponse>, HttpError> {
    finance(&service, &headers)?;
    Ok(Json(
        service
            .claim(Command::Disposition {
                id: ClaimId(uuid(&path(id)?)?),
                actor: "demo-finance".into(),
                request: json(input)?,
                now: chrono::Utc::now().timestamp(),
            })
            .await?,
    ))
}
async fn approve(
    State(service): State<Arc<Service>>,
    id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    input: Result<Json<ApproveClaimRequest>, JsonRejection>,
) -> Result<Json<ClaimResponse>, HttpError> {
    finance(&service, &headers)?;
    let request = json(input)?;
    Ok(Json(
        service
            .claim(Command::Approve {
                approval_id: ApprovalId::new(),
                id: ClaimId(uuid(&path(id)?)?),
                payee_id: request.payee_id,
                now: chrono::Utc::now().timestamp(),
            })
            .await?,
    ))
}
async fn payout(
    State(service): State<Arc<Service>>,
    id: Result<Path<String>, PathRejection>,
    input: Result<Json<PayoutClaimRequest>, JsonRejection>,
) -> Result<Json<ClaimResponse>, HttpError> {
    json(input)?;
    Ok(Json(
        service
            .payout(ClaimId(uuid(&path(id)?)?), chrono::Utc::now().timestamp())
            .await?,
    ))
}
async fn query_payout(
    State(service): State<Arc<Service>>,
    Path(id): Path<String>,
) -> Result<Json<ClaimResponse>, HttpError> {
    Ok(Json(
        service
            .query_reserved(ClaimId(uuid(&id)?), chrono::Utc::now().timestamp(), 5)
            .await?,
    ))
}
async fn budgets(
    State(service): State<Arc<Service>>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<BudgetLinesResponse>, HttpError> {
    match service
        .storage
        .execute(Command::Budgets(EventId(uuid(&path(id)?)?)))
        .await?
    {
        Output::Budgets(response) => Ok(Json(response)),
        _ => unreachable!(),
    }
}
async fn payees(State(service): State<Arc<Service>>) -> Result<Json<PayeesResponse>, HttpError> {
    match service.storage.execute(Command::Payees).await? {
        Output::Payees(response) => Ok(Json(response)),
        _ => unreachable!(),
    }
}
async fn webhook(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    input: Result<Bytes, BytesRejection>,
) -> Result<Json<WebhookResponse>, HttpError> {
    let body = input.map_err(|_| {
        HttpError(ApiError::new(
            ErrorCode::InvalidInput,
            "invalid webhook body",
        ))
    })?;
    Ok(Json(service.webhook(&body, &headers).await?))
}

fn finance(service: &Service, headers: &HeaderMap) -> Result<(), HttpError> {
    let expected = format!("Bearer {}", service.finance_token);
    let supplied = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !bool::from(supplied.as_bytes().ct_eq(expected.as_bytes())) {
        return Err(HttpError(ApiError::new(
            ErrorCode::Forbidden,
            "finance credential required",
        )));
    }
    Ok(())
}

async fn public_config(State(service): State<Arc<Service>>) -> Json<PublicConfigResponse> {
    Json(PublicConfigResponse {
        paypal_mode: if service.paypal.sandbox() {
            RuntimeMode::Sandbox
        } else {
            RuntimeMode::Mock
        },
        llm_mode: if service.extractor.ai_enabled() {
            RuntimeMode::GeminiFree
        } else {
            RuntimeMode::Manual
        },
        event_id: crate::domain::EVENT_ID,
        claimant_id: crate::domain::CLAIMANT_ID,
    })
}

async fn claims(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<ClaimsResponse>, HttpError> {
    finance(&service, &headers)?;
    match service
        .storage
        .execute(Command::Claims(EventId(uuid(&path(id)?)?)))
        .await?
    {
        Output::Claims(response) => Ok(Json(response)),
        _ => Err(ApiError::new(ErrorCode::StorageUnavailable, "unexpected storage output").into()),
    }
}

async fn review(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<ClaimReviewResponse>, HttpError> {
    finance(&service, &headers)?;
    match service
        .storage
        .execute(Command::Review(ClaimId(uuid(&path(id)?)?)))
        .await?
    {
        Output::Review(response) => Ok(Json(response)),
        _ => Err(ApiError::new(ErrorCode::StorageUnavailable, "unexpected storage output").into()),
    }
}

/// Mint a local synthetic claimant capability. Provision it out of band; never return it in public config.
pub fn claimant_token(finance_token: &str, claimant: ClaimantId) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(finance_token.as_bytes()).unwrap();
    mac.update(format!("mock-claimant:{claimant}").as_bytes());
    let signature: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{claimant}.{signature}")
}

async fn boundary(
    State(service): State<Arc<Service>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path().to_owned();
    if path == "/api/config" {
        return next.run(request).await;
    }
    let is_finance = finance(&service, request.headers()).is_ok();
    let supplied = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or_default();
    let claimant = supplied
        .split_once('.')
        .and_then(|(id, _)| id.parse::<Uuid>().ok())
        .map(ClaimantId)
        .filter(|id| {
            bool::from(
                supplied
                    .as_bytes()
                    .ct_eq(claimant_token(&service.finance_token, *id).as_bytes()),
            )
        });
    let reject = || {
        HttpError(ApiError::new(
            ErrorCode::Forbidden,
            "scoped credential required",
        ))
        .into_response()
    };
    if !is_finance {
        let Some(owner) = claimant else {
            return reject();
        };
        if path.ends_with("/disposition")
            || path.ends_with("/approve")
            || path.ends_with("/payout")
            || path.ends_with("/review")
            || path.ends_with("/claims") && path != "/api/claims"
            || path == "/api/webhooks/paypal"
        {
            return reject();
        }
        if let Some(rest) = path.strip_prefix("/api/claims/") {
            let Some(id) = rest.split('/').next().and_then(|s| s.parse::<Uuid>().ok()) else {
                return HttpError(ApiError::new(ErrorCode::InvalidInput, "invalid claim UUID"))
                    .into_response();
            };
            match service.claim(Command::Get(ClaimId(id))).await {
                Ok(response) if response.claim.claimant_id == owner => {}
                _ => return reject(),
            }
        }
        if path.starts_with("/api/events/")
            && path != format!("/api/events/{}/budget-lines", crate::domain::EVENT_ID)
        {
            return reject();
        }
        if path == "/api/claims" {
            let bytes = match axum::body::to_bytes(
                std::mem::replace(request.body_mut(), axum::body::Body::empty()),
                131_072,
            )
            .await
            {
                Ok(bytes) => bytes,
                Err(_) => {
                    return HttpError(ApiError::new(
                        ErrorCode::InvalidInput,
                        "invalid JSON request",
                    ))
                    .into_response()
                }
            };
            if let Ok(value) = serde_json::from_slice::<CreateClaimRequest>(&bytes) {
                if value.claimant_id != owner || value.event_id != crate::domain::EVENT_ID {
                    return reject();
                }
            }
            *request.body_mut() = axum::body::Body::from(bytes);
        }
    }
    let response = next.run(request).await;
    if is_finance || !response.status().is_success() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, 1_048_576).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return HttpError(ApiError::new(
                ErrorCode::StorageUnavailable,
                "response unavailable",
            ))
            .into_response()
        }
    };
    let mut value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => {
            return HttpError(ApiError::new(
                ErrorCode::StorageUnavailable,
                "response unavailable",
            ))
            .into_response()
        }
    };
    if let Some(object) = value.as_object_mut() {
        if object.contains_key("claim") {
            object.insert("approval".into(), serde_json::Value::Null);
            object.insert("payout".into(), serde_json::Value::Null);
        }
        if let Some(payees) = object.get_mut("payees").and_then(|v| v.as_array_mut()) {
            payees.retain(|p| p["claimant_id"] == serde_json::to_value(claimant.unwrap()).unwrap());
        }
    }
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    Response::from_parts(
        parts,
        axum::body::Body::from(serde_json::to_vec(&value).unwrap()),
    )
}
