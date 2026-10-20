//! settings.json in the state dir. Missing file or fields => defaults. Unknown fields kept
//! out (serde default). Written only when the user changes something.

use crate::auth::{
    check_loopback_redirect, dpapi_protect, dpapi_unprotect, InteraxoClient,
    INTERAXO_DEFAULT_REDIRECT,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// Validates a `set_setting` request against the ranges in docs/IPC.md. The Err text is
/// returned in the ack.
pub fn check_setting(key: &str, value: &Value) -> Result<(), String> {
    let int = |min: u64, max: u64| -> Result<(), String> {
        match value.as_u64() {
            Some(n) if (min..=max).contains(&n) => Ok(()),
            _ => Err(format!(
                "{key} måste vara ett heltal mellan {min} och {max}"
            )),
        }
    };
    match key {
        "aps_client_id" => {
            let id = value
                .as_str()
                .ok_or_else(|| "aps_client_id måste vara en sträng".to_string())?
                .trim();
            if id.len() > 256 {
                return Err("aps_client_id får vara högst 256 tecken".into());
            }
            if !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err("aps_client_id får bara innehålla A–Z, a–z, 0–9, _ och -".into());
            }
            Ok(())
        }
        "propagate_deletes" | "start_at_login" | "auto_update" => {
            if value.is_boolean() {
                Ok(())
            } else {
                Err(format!("{key} måste vara true eller false"))
            }
        }
        "interaxo_client_id" => {
            let id = value
                .as_str()
                .ok_or_else(|| "interaxo_client_id måste vara en sträng".to_string())?
                .trim();
            if id.len() > 256 {
                return Err("interaxo_client_id får vara högst 256 tecken".into());
            }
            if !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            {
                return Err(
                    "interaxo_client_id får bara innehålla A–Z, a–z, 0–9, _, - och .".into(),
                );
            }
            Ok(())
        }
        "interaxo_redirect_uri" => {
            let uri = value
                .as_str()
                .ok_or_else(|| "interaxo_redirect_uri måste vara en sträng".to_string())?
                .trim();
            if uri.is_empty() {
                Ok(())
            } else {
                check_loopback_redirect(uri)
            }
        }
        "interaxo_client_secret" => {
            let secret = value
                .as_str()
                .ok_or_else(|| "interaxo_client_secret måste vara en sträng".to_string())?;
            if secret.len() > 1024 {
                return Err("interaxo_client_secret får vara högst 1024 tecken".into());
            }
            Ok(())
        }
        "max_downloads" => int(1, 8),
        "max_uploads" => int(1, 4),
        "poll_active_secs" => int(15, 300),
        "log_level" => match value.as_str() {
            Some("info") | Some("debug") => Ok(()),
            _ => Err("log_level måste vara \"info\" eller \"debug\"".into()),
        },
        other => Err(format!("okänd inställning: {other}")),
    }
}

/// Built-in APS app (Desktop/SPA type, Authorization Code + PKCE, no secret). The app is still
/// named "Better Desktop Connector" in the APS developer portal; rename it to "Synka" there
/// (owner action). A public client id is not a secret.
pub const DEFAULT_CLIENT_ID: &str = "tFf8sYIHNWSrUiahpJPPxVujQTC3LJfAAEY3GZK3hEeLQ6jA";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// APS client id override; falls back to build-time SYNKA_APS_CLIENT_ID, then the built-in id.
    pub aps_client_id: Option<String>,
    /// Sync root override; default %USERPROFILE%\Synka.
    pub sync_root: Option<String>,
    /// Propagate local deletes to ACC (soft delete, recoverable in ACC).
    pub propagate_deletes: bool,
    /// Max concurrent hydrations.
    pub max_downloads: usize,
    /// Max concurrent uploads.
    pub max_uploads: usize,
    /// Fastest poll interval (seconds) while active.
    pub poll_active_secs: u64,
    /// Log level filter, e.g. "info" or "synka_engine=debug".
    pub log_level: String,
    /// Optional GitHub release channel "owner/repo". Empty (default): the engine never checks for
    /// updates; staff update from the internal OneDrive share.
    pub update_channel: String,
    /// Install updates by themselves when nothing is syncing. Only applies with an update_channel.
    pub auto_update: bool,
    /// ADMIN ONLY, settings.json only (not settable over IPC). Enables Autodesk's undocumented
    /// dm/v3 desktop-sync feed as change source. Must stay false unless Autodesk has approved
    /// its use in writing. When false the engine never calls any dm/v3 endpoint.
    pub desktop_sync_feed: bool,
    /// Interaxo OIDC client id ("bring your own client id"). None or blank: not configured.
    pub interaxo_client_id: Option<String>,
    /// Redirect URI registered for the Interaxo client. None or blank: `INTERAXO_DEFAULT_REDIRECT`.
    pub interaxo_redirect_uri: Option<String>,
    /// The Interaxo client secret: DPAPI-protected and base64-encoded (see `protect_secret`).
    pub interaxo_client_secret_protected: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            aps_client_id: None,
            sync_root: None,
            propagate_deletes: true,
            max_downloads: 4,
            max_uploads: 2,
            poll_active_secs: 30,
            log_level: "info".into(),
            update_channel: String::new(),
            auto_update: false,
            desktop_sync_feed: false,
            interaxo_client_id: None,
            interaxo_redirect_uri: None,
            interaxo_client_secret_protected: None,
        }
    }
}

/// Protect a client secret for settings.json: DPAPI (current Windows user), then base64.
#[allow(dead_code)] // the settings step stores secrets with it; tests use it now
pub fn protect_secret(secret: &str) -> anyhow::Result<String> {
    let blob = dpapi_protect(secret.as_bytes())?;
    Ok(base64::engine::general_purpose::STANDARD.encode(blob))
}

/// Reverse of `protect_secret`.
fn unprotect_secret(encoded: &str) -> anyhow::Result<String> {
    let blob = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    Ok(String::from_utf8(dpapi_unprotect(&blob)?)?)
}

impl Config {
    pub fn load(path: &Path) -> Config {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
    /// True when login has a nonblank effective client id, including the built-in default.
    /// This reports availability, not whether APS accepts the id or its registration.
    pub fn client_id_configured(&self) -> bool {
        !self.effective_client_id().0.is_empty()
    }

    /// Safe provenance for diagnostics; never includes the client id itself.
    pub fn client_id_source(&self) -> &'static str {
        self.effective_client_id().1
    }

    /// The normalized saved override for the settings UI; never includes a fallback id.
    pub fn explicit_client_id_override(&self) -> Option<&str> {
        self.aps_client_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
    }

    fn effective_client_id(&self) -> (&str, &'static str) {
        select_client_id(
            self.aps_client_id.as_deref(),
            option_env!("SYNKA_APS_CLIENT_ID"),
        )
    }

    /// Applies one `set_setting` value. `start_at_login` is validated here but stored by the
    /// engine (StartupTask), not in this file.
    pub fn apply_setting(&mut self, key: &str, value: &Value) -> Result<(), String> {
        check_setting(key, value)?;
        match key {
            "aps_client_id" => {
                self.aps_client_id = value
                    .as_str()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
            }
            "propagate_deletes" => {
                if let Some(b) = value.as_bool() {
                    self.propagate_deletes = b;
                }
            }
            "auto_update" => {
                if let Some(b) = value.as_bool() {
                    self.auto_update = b;
                }
            }
            "max_downloads" => {
                if let Some(n) = value.as_u64() {
                    self.max_downloads = n as usize;
                }
            }
            "max_uploads" => {
                if let Some(n) = value.as_u64() {
                    self.max_uploads = n as usize;
                }
            }
            "poll_active_secs" => {
                if let Some(n) = value.as_u64() {
                    self.poll_active_secs = n;
                }
            }
            "log_level" => {
                if let Some(s) = value.as_str() {
                    self.log_level = s.to_string();
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn client_id(&self) -> String {
        self.effective_client_id().0.to_string()
    }

    /// The Interaxo client as configured. The id is trimmed ("" when not configured), the
    /// secret is decrypted (None when absent, or when the stored blob cannot be decrypted:
    /// that is logged, not fatal), and the redirect is the default when blank.
    pub fn interaxo_client(&self) -> InteraxoClient {
        let client_id = self
            .interaxo_client_id
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        let client_secret = self
            .interaxo_client_secret_protected
            .as_deref()
            .map(str::trim)
            .filter(|blob| !blob.is_empty())
            .and_then(|blob| match unprotect_secret(blob) {
                Ok(secret) => Some(secret),
                Err(e) => {
                    tracing::warn!("interaxo_client_secret kan inte läsas, den ignoreras: {e:#}");
                    None
                }
            })
            .filter(|secret| !secret.is_empty());
        let redirect_uri = self
            .interaxo_redirect_uri
            .as_deref()
            .map(str::trim)
            .filter(|uri| !uri.is_empty())
            .unwrap_or(INTERAXO_DEFAULT_REDIRECT)
            .to_string();
        InteraxoClient {
            client_id,
            client_secret,
            redirect_uri,
        }
    }
}

/// Keep login, IPC availability and diagnostic provenance on the same selection path.
fn select_client_id<'a>(
    settings: Option<&'a str>,
    build_env: Option<&'a str>,
) -> (&'a str, &'static str) {
    if let Some(id) = settings.map(str::trim).filter(|id| !id.is_empty()) {
        return (id, "settings");
    }
    if let Some(id) = build_env.map(str::trim).filter(|id| !id.is_empty()) {
        return (id, "build_env");
    }
    (DEFAULT_CLIENT_ID, "built_in")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_client_id_is_available_for_login() {
        assert!(Config::default().client_id_configured());
        let (id, source) = select_client_id(None, None);
        assert!(!id.is_empty());
        assert!(id == DEFAULT_CLIENT_ID);
        assert_eq!(source, "built_in");
    }

    #[test]
    fn client_id_selection_trims_overrides_and_preserves_precedence() {
        let (id, source) = select_client_id(
            Some(" \tsettings-test-client\r\n"),
            Some("build-test-client"),
        );
        assert_eq!(id, "settings-test-client");
        assert_eq!(source, "settings");

        let (id, source) = select_client_id(Some(" \t\r\n"), Some(" \tbuild-test-client\n"));
        assert_eq!(id, "build-test-client");
        assert_eq!(source, "build_env");
    }

    #[test]
    fn blank_client_id_sources_fall_back_to_the_builtin() {
        for settings in [None, Some(""), Some(" \t\r\n")] {
            for build_env in [None, Some(""), Some(" \t\r\n")] {
                let (id, source) = select_client_id(settings, build_env);
                assert!(id == DEFAULT_CLIENT_ID);
                assert_eq!(source, "built_in");
            }
        }
    }

    #[test]
    fn availability_and_source_match_the_selected_settings_override() {
        // Client ids are opaque here: availability must not imply a valid APS registration.
        let config = Config {
            aps_client_id: Some(" \tunverified-test-override\r\n".into()),
            ..Config::default()
        };
        assert_eq!(config.client_id(), "unverified-test-override");
        assert!(config.client_id_configured());
        assert_eq!(config.client_id_source(), "settings");

        let blank = Config {
            aps_client_id: Some(" \t\r\n".into()),
            ..Config::default()
        };
        let (expected_id, expected_source) =
            select_client_id(None, option_env!("SYNKA_APS_CLIENT_ID"));
        assert!(blank.client_id() == expected_id);
        assert!(blank.client_id_configured());
        assert_eq!(blank.client_id_source(), expected_source);
    }

    #[test]
    fn client_id_setting_accepts_normalized_ids_and_blank_reset() {
        for value in [
            json!("test-client_ID_123"),
            json!(" \ttest-client_ID_123\r\n"),
            json!(""),
            json!(" \t\r\n"),
            json!("a".repeat(256)),
            json!(format!("  {}  ", "a".repeat(256))),
        ] {
            assert!(check_setting("aps_client_id", &value).is_ok(), "{value}");
        }
    }

    #[test]
    fn client_id_setting_rejects_non_strings_and_malformed_ids() {
        for value in [
            Value::Null,
            json!(true),
            json!(42),
            json!([]),
            json!({}),
            json!("https://aps.autodesk.com/app/test"),
            json!("client id"),
            json!("client\tid"),
            json!("client\nid"),
            json!("client\u{0000}id"),
            json!("client.id"),
            json!("client=id"),
            json!("klient-å"),
            json!("a".repeat(257)),
            json!(format!(" \t{}\n", "a".repeat(257))),
        ] {
            assert!(check_setting("aps_client_id", &value).is_err(), "{value}");
        }
    }

    #[test]
    fn applying_client_id_setting_trims_and_resets_to_fallback() {
        let mut config = Config::default();
        config
            .apply_setting("aps_client_id", &json!(" \ttest-client_ID_123\r\n"))
            .unwrap();
        assert_eq!(config.aps_client_id.as_deref(), Some("test-client_ID_123"));
        assert_eq!(
            config.explicit_client_id_override(),
            Some("test-client_ID_123")
        );
        assert_eq!(config.client_id(), "test-client_ID_123");
        assert_eq!(config.client_id_source(), "settings");

        for reset in ["", " \t\r\n"] {
            config.aps_client_id = Some("previous-client".into());
            config
                .apply_setting("aps_client_id", &json!(reset))
                .unwrap();
            assert!(config.aps_client_id.is_none());
            assert!(config.explicit_client_id_override().is_none());
            let (fallback, source) = select_client_id(None, option_env!("SYNKA_APS_CLIENT_ID"));
            assert_eq!(config.client_id(), fallback);
            assert_eq!(config.client_id_source(), source);
        }
    }

    #[test]
    fn invalid_client_id_setting_preserves_the_previous_override() {
        let mut config = Config {
            aps_client_id: Some("previous-client".into()),
            ..Config::default()
        };
        for invalid in [Value::Null, json!("client id"), json!("x".repeat(257))] {
            assert!(config.apply_setting("aps_client_id", &invalid).is_err());
            assert_eq!(config.aps_client_id.as_deref(), Some("previous-client"));
        }
    }

    #[test]
    fn explicit_override_snapshot_never_returns_the_effective_default() {
        let mut config = Config::default();
        assert!(config.client_id_configured());
        assert!(config.explicit_client_id_override().is_none());
        config.aps_client_id = Some(" \t\r\n".into());
        assert!(config.explicit_client_id_override().is_none());
        config.aps_client_id = Some(" \ttest-client\r\n".into());
        assert_eq!(config.explicit_client_id_override(), Some("test-client"));
    }

    #[test]
    fn defaults_turn_off_updates_and_private_feed() {
        let config = Config::default();
        assert!(config.update_channel.is_empty());
        assert!(!config.auto_update);
        assert!(!config.desktop_sync_feed);
    }

    #[test]
    fn desktop_sync_feed_is_not_an_ipc_setting() {
        assert!(check_setting("desktop_sync_feed", &json!(true)).is_err());
        let mut config = Config::default();
        assert!(config
            .apply_setting("desktop_sync_feed", &json!(true))
            .is_err());
        assert!(!config.desktop_sync_feed);
    }

    #[test]
    fn desktop_sync_feed_loads_from_settings_json() {
        assert!(
            serde_json::from_str::<Config>(r#"{"desktop_sync_feed":true}"#)
                .unwrap()
                .desktop_sync_feed
        );
    }

    #[test]
    fn interaxo_client_id_setting_accepts_identifiers_and_blank_reset() {
        for value in [
            json!("test-client_ID.123"),
            json!(" \ttest-client_ID.123\r\n"),
            json!(""),
            json!(" \t\r\n"),
            json!("a".repeat(256)),
        ] {
            assert!(
                check_setting("interaxo_client_id", &value).is_ok(),
                "{value}"
            );
        }
        for value in [
            Value::Null,
            json!(42),
            json!("client id"),
            json!("klient-å"),
            json!("https://interaxo.example/client"),
            json!("a".repeat(257)),
        ] {
            assert!(
                check_setting("interaxo_client_id", &value).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn interaxo_redirect_setting_is_blank_or_a_loopback_redirect() {
        for value in [
            json!(""),
            json!("  "),
            json!("http://localhost:47864/"),
            json!(" http://127.0.0.1:5000/interaxo "),
        ] {
            assert!(
                check_setting("interaxo_redirect_uri", &value).is_ok(),
                "{value}"
            );
        }
        for value in [
            Value::Null,
            json!("https://localhost:47864/"),
            json!("http://example.com:47864/"),
            json!("http://localhost/"),
        ] {
            assert!(
                check_setting("interaxo_redirect_uri", &value).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn interaxo_secret_setting_is_a_bounded_string() {
        assert!(check_setting("interaxo_client_secret", &json!("")).is_ok());
        assert!(check_setting("interaxo_client_secret", &json!("s".repeat(1024))).is_ok());
        assert!(check_setting("interaxo_client_secret", &json!("s".repeat(1025))).is_err());
        assert!(check_setting("interaxo_client_secret", &json!(true)).is_err());
    }

    #[test]
    fn interaxo_client_defaults_and_trimming() {
        let client = Config::default().interaxo_client();
        assert_eq!(client.client_id, "");
        assert_eq!(client.client_secret, None);
        assert_eq!(client.redirect_uri, INTERAXO_DEFAULT_REDIRECT);

        let config = Config {
            interaxo_client_id: Some(" \tinteraxo-client\r\n".into()),
            interaxo_redirect_uri: Some("  ".into()),
            ..Config::default()
        };
        let client = config.interaxo_client();
        assert_eq!(client.client_id, "interaxo-client");
        assert_eq!(client.redirect_uri, INTERAXO_DEFAULT_REDIRECT);

        let config = Config {
            interaxo_redirect_uri: Some(" http://127.0.0.1:5000/interaxo ".into()),
            ..Config::default()
        };
        assert_eq!(
            config.interaxo_client().redirect_uri,
            "http://127.0.0.1:5000/interaxo"
        );
    }

    #[test]
    fn interaxo_secret_round_trips_through_dpapi_and_is_not_stored_in_clear() {
        let blob = protect_secret("interaxo-secret-value").unwrap();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&blob)
            .unwrap();
        assert!(!raw
            .windows(b"interaxo-secret-value".len())
            .any(|w| w == b"interaxo-secret-value"));

        let config = Config {
            interaxo_client_secret_protected: Some(blob),
            ..Config::default()
        };
        assert_eq!(
            config.interaxo_client().client_secret.as_deref(),
            Some("interaxo-secret-value")
        );
    }

    #[test]
    fn undecryptable_interaxo_secret_is_ignored() {
        for stored in ["!!! not base64", "bm90IGEgZFBBUEkgYmxvYg=="] {
            let config = Config {
                interaxo_client_secret_protected: Some(stored.into()),
                ..Config::default()
            };
            assert_eq!(config.interaxo_client().client_secret, None, "{stored}");
        }
    }

    #[test]
    fn interaxo_settings_are_optional_in_settings_json() {
        let old: Config = serde_json::from_str(r#"{"propagate_deletes":false}"#).unwrap();
        assert_eq!(old.interaxo_client_id, None);
        assert_eq!(old.interaxo_redirect_uri, None);
        assert_eq!(old.interaxo_client_secret_protected, None);
        let new: Config = serde_json::from_str(
            r#"{"interaxo_client_id":"abc","interaxo_redirect_uri":"http://localhost:47864/"}"#,
        )
        .unwrap();
        assert_eq!(new.interaxo_client_id.as_deref(), Some("abc"));
    }
}
