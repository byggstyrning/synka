//! StreamBIM sign-in: region + e-mail + password, optional TOTP code (MFA). See
//! docs/PROVIDERS.md "StreamBIM" and docs/specs/streambim-endpoints.md.
//!
//! STUB: this step only fixes the public API. Nothing is read from or written to the state
//! directory yet, and every sign-in call fails with a Swedish "not supported yet" message.

#![allow(dead_code)]

use crate::auth::UserInfo;
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

const NOT_YET: &str = "StreamBIM stöds inte ännu";

/// Holds the StreamBIM session (refresh material and the signed-in user).
pub struct StreamBimAuth {
    http: reqwest::Client,
    state_dir: PathBuf,
}

/// What a password login leads to.
#[derive(Debug)]
pub enum LoginOutcome {
    SignedIn(UserInfo),
    /// The account has MFA: call `verify_mfa` with the TOTP code.
    MfaRequired,
}

impl StreamBimAuth {
    /// `state_dir` is where the token and profile files will live (`token-streambim.bin`,
    /// `user-streambim.json`). The stub does not touch it.
    pub fn new(http: reqwest::Client, state_dir: &Path) -> StreamBimAuth {
        StreamBimAuth {
            http,
            state_dir: state_dir.to_path_buf(),
        }
    }

    pub fn is_signed_in(&self) -> bool {
        false
    }

    pub fn user(&self) -> Option<UserInfo> {
        None
    }

    /// The region host once signed in, e.g. "app.streambim.com".
    pub fn host(&self) -> Option<String> {
        None
    }

    /// The region subdomain once signed in, e.g. "app".
    pub fn region(&self) -> Option<String> {
        None
    }

    pub async fn login(
        &self,
        _region: &str,
        _username: &str,
        _password: &str,
    ) -> Result<LoginOutcome> {
        Err(anyhow!(NOT_YET))
    }

    pub async fn verify_mfa(&self, _code: &str) -> Result<UserInfo> {
        Err(anyhow!(NOT_YET))
    }

    pub async fn sign_out(&self) -> Result<()> {
        Ok(())
    }

    pub async fn access_token(&self) -> Result<String> {
        Err(anyhow!("Inte inloggad på StreamBIM"))
    }

    pub async fn invalidate(&self) {}
}
