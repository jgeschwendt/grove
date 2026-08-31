//! The response envelope — the single serializer for every body on the HTTP surface.
//!
//! ```jsonc
//! {"ok": true,  "data": <T>}
//! {"ok": false, "error": {"code": <ErrorCode>, "message": <string>, "data": <any>?}}
//! ```
//!
//! **Tagged on `ok`.** v1's Rust decoder was `#[serde(untagged)]` and discriminated on
//! which of `data`/`error` was present, never reading `ok` at all: `{"ok":true,
//! "error":{…}}` decoded as a failure and `{"ok":false,"data":…}` as a success. Here
//! `ok` is the tag and a body that disagrees with it is a decode error — the two cases
//! are pinned by tests below.
//!
//! **`error.data` is carried.** v1's `ServerError` decoded only `code` and `message`,
//! dropping the detail every Elixir error path emitted (`data.slug`, `data.reason`,
//! `data.status`). It survives here as [`ApiError::data`], readable typed through
//! [`ApiError::data_as`].

use std::fmt;

use serde::de::DeserializeOwned;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::ErrorCode;

/// A response body: either the route's payload, or the failure that replaced it.
#[derive(Clone, Debug, PartialEq)]
// stele:landmark api-envelope
pub enum Envelope<T> {
    Ok(T),
    Err(ApiError),
}

impl<T> Envelope<T> {
    /// A success envelope carrying `data`.
    pub const fn ok(data: T) -> Self {
        Self::Ok(data)
    }

    /// A failure envelope. Attach detail with
    /// `Envelope::Err(ApiError::new(..).with_data(..))` when the route has any.
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Err(ApiError::new(code, message))
    }

    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// The envelope as a `Result`, for a caller that wants `?`.
    pub fn into_result(self) -> Result<T, ApiError> {
        match self {
            Self::Ok(data) => Ok(data),
            Self::Err(err) => Err(err),
        }
    }
}

/// The `error` half of a failure envelope. `data` is route-specific detail — a slug,
/// a reason, a boot status — kept as [`Value`] so the envelope stays one type across
/// every route, and read back typed with [`Self::data_as`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Attach the route's detail payload (`serde_json::json!({"slug": slug})`, a
    /// serialized [`crate::BootStatus`], …).
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// Read `error.data` as `T`. `None` when there is no detail *or* when it is
    /// shaped differently than expected: an optional diagnostic must never turn a
    /// reported failure into a decode failure.
    #[must_use]
    pub fn data_as<T: DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_value(self.data.clone()?).ok()
    }

    /// `error.data` rendered for a human, or `None` when there is nothing to add.
    ///
    /// Two of the seven codes — `doctor_failed` and `remove_failed` — carry a
    /// deliberately generic message whose only content is `data.reason`, so a client
    /// that drops `data` prints a tautology ("`doctor_failed`: doctor failed") at the
    /// exact moment the daemon knows the TOML parse error's line and column. That is
    /// what this exists for: the wire half of "typed `error.data` reaches the client"
    /// was delivered, and the operator still saw nothing.
    ///
    /// `reason` when present (the shape the failure codes define), else the flat
    /// scalar fields — `slug`, `name`, `status` — joined, so a `not_found` names what
    /// was not found.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        let data = self.data.as_ref()?;
        if let Some(reason) = data.get("reason").and_then(Value::as_str) {
            return Some(reason.trim().to_owned());
        }
        let pairs: Vec<String> = data
            .as_object()?
            .iter()
            .filter_map(|(key, value)| match value {
                Value::String(s) => Some(format!("{key}={s}")),
                Value::Bool(_) | Value::Number(_) => Some(format!("{key}={value}")),
                _ => None,
            })
            .collect();
        (!pairs.is_empty()).then(|| pairs.join(" "))
    }
}

impl fmt::Display for ApiError {
    /// `code: message` — the code travels into whatever the CLI prints, so an
    /// operator can match what they see against the contract table.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl<T: Serialize> Serialize for Envelope<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut env = serializer.serialize_struct("Envelope", 2)?;
        match self {
            Self::Ok(data) => {
                env.serialize_field("ok", &true)?;
                env.serialize_field("data", data)?;
            }
            Self::Err(error) => {
                env.serialize_field("ok", &false)?;
                env.serialize_field("error", error)?;
            }
        }
        env.end()
    }
}

/// The decode shape: everything optional but `ok`, so the tag can be read *before*
/// deciding which half the body must carry. (An absent `Option` field is `None` to
/// serde already; `#[serde(default)]` here would demand `T: Default` of every
/// payload for no gain.)
#[derive(Deserialize)]
struct Raw<T> {
    ok: bool,
    data: Option<T>,
    error: Option<ApiError>,
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Envelope<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Raw::<T>::deserialize(deserializer)? {
            Raw {
                ok: true,
                data: Some(data),
                error: None,
            } => Ok(Self::Ok(data)),
            Raw {
                ok: false,
                data: None,
                error: Some(error),
            } => Ok(Self::Err(error)),
            Raw { ok: true, .. } => Err(serde::de::Error::custom(
                "`ok: true` envelope must carry `data` and no `error`",
            )),
            Raw { ok: false, .. } => Err(serde::de::Error::custom(
                "`ok: false` envelope must carry `error` and no `data`",
            )),
        }
    }
}

/// The carried v1 fixtures — the literal bytes `crates/grove/src/api.rs` pinned —
/// plus the two bodies its untagged decoder got wrong.
#[cfg(test)]
mod tests {
    use super::{ApiError, Envelope};
    use crate::routes::{DoctorData, HealthData, HealthStatus};
    use crate::{ErrorCode, RootStatus, RootStatusEntry};
    use grove_ops::env::{ShareOutcome, ShareStatus};
    use serde_json::json;

    #[test]
    fn parses_ok_envelope() {
        let json =
            r#"{"ok":true,"data":{"status":"ready","version":"0.1.0","home":"/home/.grove"}}"#;
        match serde_json::from_str::<Envelope<HealthData>>(json).unwrap() {
            Envelope::Ok(data) => {
                assert_eq!(data.status, HealthStatus::Ready);
                assert_eq!(data.version, "0.1.0");
            }
            Envelope::Err(_) => panic!("expected Ok variant"),
        }
    }

    #[test]
    fn parses_error_envelope() {
        let json = r#"{"ok":false,"error":{"code":"unavailable","message":"server booting"}}"#;
        match serde_json::from_str::<Envelope<HealthData>>(json).unwrap() {
            Envelope::Err(error) => {
                assert_eq!(error.code, ErrorCode::Unavailable);
                assert_eq!(error.message, "server booting");
                assert_eq!(error.data, None);
            }
            Envelope::Ok(_) => panic!("expected Err variant"),
        }
    }

    #[test]
    fn doctor_envelope_round_trips() {
        // The exact wire shape /api/doctor emits: lowercase status, `worktree`/
        // `reason` omitted when None.
        let json = r#"{"ok":true,"data":{"report":[
            {"slug":"o/r","path":".env","status":"ok"},
            {"slug":"o/r","worktree":"feat","path":".env","status":"conflict","reason":"real file"}
        ]}}"#;
        let expected = vec![
            ShareOutcome {
                slug: "o/r".into(),
                worktree: None,
                path: ".env".into(),
                status: ShareStatus::Ok,
                reason: None,
            },
            ShareOutcome {
                slug: "o/r".into(),
                worktree: Some("feat".into()),
                path: ".env".into(),
                status: ShareStatus::Conflict,
                reason: Some("real file".into()),
            },
        ];
        let Envelope::Ok(data) = serde_json::from_str::<Envelope<DoctorData>>(json).unwrap() else {
            panic!("expected Ok")
        };
        assert_eq!(data.report, expected);
        assert!(data.pools.is_empty(), "absent arrays default to empty");
        assert!(data.statuses.is_empty());
        assert!(data.checks.is_empty());

        // The round trip v1 never asserted: re-serializing reproduces the same bytes,
        // omit-when-None included. This is what makes the one serializer testable.
        assert_eq!(
            serde_json::to_value(Envelope::Ok(data)).unwrap(),
            json!({"ok": true, "data": {
                "report": [
                    {"slug":"o/r","path":".env","status":"ok"},
                    {"slug":"o/r","worktree":"feat","path":".env","status":"conflict","reason":"real file"}
                ],
                "pools": [],
                "statuses": [],
                "checks": []
            }})
        );
    }

    /// v1's untagged decoder read this as a *failure* — it never looked at `ok`.
    #[test]
    fn an_ok_true_body_carrying_an_error_is_rejected() {
        let json = r#"{"ok":true,"error":{"code":"not_found","message":"gone"}}"#;
        assert!(serde_json::from_str::<Envelope<HealthData>>(json).is_err());
    }

    /// The mirror image: `ok: false` with a payload is not a success.
    #[test]
    fn an_ok_false_body_carrying_data_is_rejected() {
        let json =
            r#"{"ok":false,"data":{"status":"ready","version":"0.1.0","home":"/home/.grove"}}"#;
        assert!(serde_json::from_str::<Envelope<HealthData>>(json).is_err());
    }

    #[test]
    fn a_body_without_ok_is_not_an_envelope() {
        let json = r#"{"data":{"status":"ready","version":"0.1.0","home":"/home/.grove"}}"#;
        assert!(serde_json::from_str::<Envelope<HealthData>>(json).is_err());
    }

    /// `error.data` survives the trip in both directions — the v1 defect this crate
    /// closes. The payload here is the `not_found` route's, `{"slug": …}`.
    #[test]
    fn error_data_round_trips_and_reads_back_typed() {
        let envelope: Envelope<DoctorData> = Envelope::Err(
            ApiError::new(ErrorCode::NotFound, "root not declared: o/r")
                .with_data(json!({"slug": "o/r"})),
        );
        let bytes = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            bytes,
            r#"{"ok":false,"error":{"code":"not_found","message":"root not declared: o/r","data":{"slug":"o/r"}}}"#
        );

        let Envelope::Err(err) = serde_json::from_str::<Envelope<DoctorData>>(&bytes).unwrap()
        else {
            panic!("expected Err")
        };
        assert_eq!(err.to_string(), "not_found: root not declared: o/r");
        assert_eq!(
            err.data_as::<RootStatusEntry>(),
            None,
            "a payload of another shape reads as absent, never as a decode failure"
        );
        assert_eq!(err.data, Some(json!({"slug": "o/r"})));
    }

    /// `error.data` is typed at the producer too: any `Serialize` payload goes in,
    /// and `data_as` brings the same type back.
    #[test]
    fn error_data_carries_a_typed_payload() {
        let entry = RootStatusEntry {
            slug: "o/r".into(),
            status: RootStatus::Cloning,
        };
        let err = ApiError::new(ErrorCode::Unavailable, "still cloning")
            .with_data(serde_json::to_value(&entry).unwrap());
        assert_eq!(err.data_as::<RootStatusEntry>(), Some(entry));
    }
}
