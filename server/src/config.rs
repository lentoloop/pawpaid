use crate::adapters::MockPayoutFixture;
use contracts::{ApiError, ErrorCode};
use std::{collections::BTreeMap, net::SocketAddr};

#[cfg(test)]
const DEFAULT_FINANCE_TOKEN: &str = "synthetic-finance-local-only";

#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub development: bool,
    pub llm_mode: String,
    pub gemini_api_key: String,
    pub trust_loopback_proxy: bool,
    pub database_url: String,
    pub finance_token: String,
    pub fixture: MockPayoutFixture,
    pub paypal_mode: String,
    pub client_id: String,
    pub client_secret: String,
    pub recipient: String,
    pub webhook_id: String,
    pub claimant_password: String,
    pub finance_password: String,
}
impl Config {
    pub fn from_env() -> Result<Self, ApiError> {
        Self::from_values(std::env::vars().collect())
    }
    pub fn from_values(values: BTreeMap<String, String>) -> Result<Self, ApiError> {
        let paypal_mode = values.get("PAYPAL_MODE").cloned().unwrap_or_default();
        if paypal_mode != "sandbox"
            || values
                .get("LLM_MODE")
                .is_some_and(|mode| mode != "gemini-free" && mode != "manual")
        {
            return Err(ApiError::new(
                ErrorCode::ModeNotImplemented,
                "only sandbox payouts are permitted",
            ));
        }
        let llm_mode = values
            .get("LLM_MODE")
            .cloned()
            .unwrap_or_else(|| "manual".into());
        let gemini_api_key = values.get("GEMINI_API_KEY").cloned().unwrap_or_default();
        if llm_mode == "gemini-free"
            && (gemini_api_key.trim().is_empty()
                || values.get("GEMINI_FREE_TIER_ONLY").map(String::as_str) != Some("true"))
        {
            return Err(ApiError::new(ErrorCode::InvalidInput,
                "gemini-free requires GEMINI_API_KEY and GEMINI_FREE_TIER_ONLY=true (billing disabled project)"));
        }
        let bind: SocketAddr = values
            .get("BIND_ADDR")
            .map(String::as_str)
            .unwrap_or("127.0.0.1:3000")
            .parse()
            .map_err(|_| {
                ApiError::new(
                    ErrorCode::InvalidInput,
                    "BIND_ADDR must be a socket address",
                )
            })?;
        let database_url = values
            .get("DATABASE_URL")
            .cloned()
            .unwrap_or_else(|| "sqlite://receipt-roots.db?mode=rwc".into());
        let finance_token = values
            .get("FINANCE_TOKEN")
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if finance_token.trim().is_empty() {
            return Err(ApiError::new(
                ErrorCode::InvalidInput,
                "FINANCE_TOKEN cannot be empty",
            ));
        }
        if !bind.ip().is_loopback()
            && values.get("ALLOW_PUBLIC_BIND").map(String::as_str) != Some("true")
        {
            return Err(ApiError::new(
                ErrorCode::InvalidInput,
                "public bind requires ALLOW_PUBLIC_BIND=true",
            ));
        }
        let required = |name: &str| -> Result<String, ApiError> {
            values
                .get(name)
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorCode::InvalidInput,
                        format!("{name} must be supplied by environment"),
                    )
                })
        };
        // Both fixed judge identities are always provisioned out of band.
        let claimant_password = required("JUDGE_CLAIMANT_PASSWORD")?;
        let finance_password = required("JUDGE_FINANCE_PASSWORD")?;
        // Development must be explicit, and release binaries never waive password strength.
        let development = cfg!(debug_assertions)
            && values.get("APP_ENV").map(String::as_str) == Some("development");
        if !development
            && [&claimant_password, &finance_password]
                .iter()
                .any(|password| password.chars().count() < 24)
        {
            return Err(ApiError::new(
                ErrorCode::InvalidInput,
                "account passwords must contain at least 24 characters",
            ));
        }
        let (client_id, client_secret, recipient) = if paypal_mode == "sandbox" {
            (
                required("PAYPAL_CLIENT_ID")?,
                required("PAYPAL_CLIENT_SECRET")?,
                required("PAYPAL_SANDBOX_RECIPIENT")?,
            )
        } else {
            (
                String::new(),
                String::new(),
                "sandbox-pawhaven-clinic@example.invalid".into(),
            )
        };
        let webhook_id = if paypal_mode == "sandbox" {
            required("PAYPAL_WEBHOOK_ID")?
        } else {
            String::new()
        };
        let fixture = MockPayoutFixture::parse(
            values
                .get("MOCK_PAYPAL_FIXTURE")
                .map(String::as_str)
                .unwrap_or("success"),
        )?;
        Ok(Self {
            bind,
            development,
            llm_mode,
            gemini_api_key,
            trust_loopback_proxy: values.get("TRUST_LOOPBACK_PROXY").map(String::as_str)
                == Some("true"),
            database_url,
            finance_token,
            fixture,
            paypal_mode,
            client_id,
            client_secret,
            recipient,
            webhook_id,
            claimant_password,
            finance_password,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_verifier_requires_loopback() {
        for (bind, token, allowed) in [
            ("127.0.0.1:3000", None, true),
            ("[::1]:3000", None, true),
            ("0.0.0.0:3000", None, false),
            ("0.0.0.0:3000", Some("test-custom-token"), false),
            ("0.0.0.0:3000", Some(DEFAULT_FINANCE_TOKEN), false),
        ] {
            let mut values = BTreeMap::from([
                ("APP_ENV".into(), "development".into()),
                ("PAYPAL_MODE".into(), "sandbox".into()),
                ("PAYPAL_CLIENT_ID".into(), "synthetic".into()),
                ("PAYPAL_CLIENT_SECRET".into(), "synthetic".into()),
                (
                    "PAYPAL_SANDBOX_RECIPIENT".into(),
                    "synthetic@example.invalid".into(),
                ),
                ("PAYPAL_WEBHOOK_ID".into(), "synthetic".into()),
                ("BIND_ADDR".into(), bind.into()),
                (
                    "JUDGE_CLAIMANT_PASSWORD".into(),
                    "synthetic-claimant-password-24".into(),
                ),
                (
                    "JUDGE_FINANCE_PASSWORD".into(),
                    "synthetic-finance-password-24".into(),
                ),
            ]);
            if let Some(token) = token {
                values.insert("FINANCE_TOKEN".into(), token.into());
            }
            let result = Config::from_values(values);
            assert_eq!(result.is_ok(), allowed, "bind {bind}");
            if let Err(error) = result {
                assert_eq!(error.code, ErrorCode::InvalidInput);
                assert_eq!(error.message, "public bind requires ALLOW_PUBLIC_BIND=true");
            }
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("paypal_mode", &self.paypal_mode)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[test]
    fn explicit_public_bind_requires_configured_sandbox() {
        let mut values = BTreeMap::from([
            ("APP_ENV".into(), "development".into()),
            ("PAYPAL_MODE".into(), "sandbox".into()),
            ("PAYPAL_CLIENT_ID".into(), "synthetic".into()),
            ("PAYPAL_CLIENT_SECRET".into(), "synthetic".into()),
            (
                "PAYPAL_SANDBOX_RECIPIENT".into(),
                "synthetic@example.invalid".into(),
            ),
            ("PAYPAL_WEBHOOK_ID".into(), "synthetic".into()),
            ("ALLOW_PUBLIC_BIND".into(), "true".into()),
            ("BIND_ADDR".into(), "0.0.0.0:3000".into()),
            (
                "JUDGE_CLAIMANT_PASSWORD".into(),
                "synthetic-claimant-password-24".into(),
            ),
            (
                "JUDGE_FINANCE_PASSWORD".into(),
                "synthetic-finance-password-24".into(),
            ),
        ]);
        assert!(Config::from_values(values.clone()).is_ok());
        values.remove("PAYPAL_WEBHOOK_ID");
        assert!(Config::from_values(values.clone()).is_err());
        for name in [
            "PAYPAL_CLIENT_ID",
            "PAYPAL_CLIENT_SECRET",
            "PAYPAL_SANDBOX_RECIPIENT",
            "PAYPAL_WEBHOOK_ID",
        ] {
            values.insert(name.into(), "fake-placeholder".into());
        }
        assert!(Config::from_values(values.clone()).is_ok());
        for mode in ["mock", "", "live", "production", "bogus"] {
            values.insert("PAYPAL_MODE".into(), mode.into());
            assert_eq!(
                Config::from_values(values.clone()).unwrap_err().code,
                ErrorCode::ModeNotImplemented
            );
        }
    }
}

#[cfg(test)]
mod extraction_tests {
    use super::*;
    #[test]
    fn extraction_is_manual_unless_free_project_explicitly_configured() {
        let mut values = BTreeMap::from([
            ("APP_ENV".into(), "development".into()),
            ("PAYPAL_MODE".into(), "sandbox".into()),
            ("PAYPAL_CLIENT_ID".into(), "synthetic".into()),
            ("PAYPAL_CLIENT_SECRET".into(), "synthetic".into()),
            (
                "PAYPAL_SANDBOX_RECIPIENT".into(),
                "synthetic@example.invalid".into(),
            ),
            ("PAYPAL_WEBHOOK_ID".into(), "synthetic".into()),
            (
                "JUDGE_CLAIMANT_PASSWORD".into(),
                "synthetic-claimant-password-24".into(),
            ),
            (
                "JUDGE_FINANCE_PASSWORD".into(),
                "synthetic-finance-password-24".into(),
            ),
        ]);
        assert_eq!(
            Config::from_values(values.clone()).unwrap().llm_mode,
            "manual"
        );
        values.insert("LLM_MODE".into(), "gemini-free".into());
        assert!(Config::from_values(values.clone()).is_err());
        values.insert("GEMINI_API_KEY".into(), "synthetic-key".into());
        assert!(Config::from_values(values.clone()).is_err());
        values.insert("GEMINI_FREE_TIER_ONLY".into(), "true".into());
        let config = Config::from_values(values.clone()).unwrap();
        assert!(!format!("{config:?}").contains("synthetic-key"));
        for mode in ["mock", "openai", "anthropic", "paid", "bogus"] {
            values.insert("LLM_MODE".into(), mode.into());
            assert!(Config::from_values(values.clone()).is_err());
        }
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;
    fn valid() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("PAYPAL_MODE".into(), "sandbox".into()),
            ("PAYPAL_CLIENT_ID".into(), "synthetic".into()),
            ("PAYPAL_CLIENT_SECRET".into(), "synthetic".into()),
            (
                "PAYPAL_SANDBOX_RECIPIENT".into(),
                "synthetic@example.invalid".into(),
            ),
            ("PAYPAL_WEBHOOK_ID".into(), "synthetic".into()),
            ("JUDGE_CLAIMANT_PASSWORD".into(), "s".repeat(24)),
            ("JUDGE_FINANCE_PASSWORD".into(), "f".repeat(24)),
        ])
    }
    #[test]
    fn non_dev_passwords_are_required_and_at_least_24_characters() {
        assert!(Config::from_values(valid()).is_ok());
        for name in ["JUDGE_CLAIMANT_PASSWORD", "JUDGE_FINANCE_PASSWORD"] {
            for length in [0, 1, 23] {
                let mut values = valid();
                values.insert(name.into(), "x".repeat(length));
                assert!(Config::from_values(values).is_err());
            }
            let mut values = valid();
            values.remove(name);
            assert!(Config::from_values(values).is_err());
        }
    }
    #[test]
    fn missing_or_non_sandbox_mode_fails_closed() {
        let mut values = valid();
        values.remove("PAYPAL_MODE");
        assert!(Config::from_values(values).is_err());
        for mode in ["", "mock", "live", "Sandbox", "sandbox "] {
            let mut values = valid();
            values.insert("PAYPAL_MODE".into(), mode.into());
            assert!(Config::from_values(values).is_err());
        }
    }
    #[test]
    fn release_never_waives_short_passwords() {
        let mut values = valid();
        values.insert("APP_ENV".into(), "development".into());
        values.insert("JUDGE_FINANCE_PASSWORD".into(), "short-synthetic".into());
        assert_eq!(Config::from_values(values).is_ok(), cfg!(debug_assertions));
    }
}
