//! In-app updater. It is OFF unless `update_channel` (cfg.update_channel, "owner/repo") is set;
//! the default is empty, and then the engine makes no network request for updates. Staff
//! update from the internal OneDrive share (see packaging/share_release.ps1 and the delivery
//! README). When a channel is set: GET https://api.github.com/repos/{owner}/{repo}/releases/latest
//! (User-Agent required), pick the exact package asset for tag "vX.Y.Z", compare it with
//! CARGO_PKG_VERSION (semver). Install: stream to a unique temporary MSIX,
//! verify its Authenticode signer and manifest identity/version, then
//! Add-AppxPackage -ForceTargetApplicationShutdown (see install(): AddPackageAsync
//! needs an IIterable type that the windows crate does not re-export). Only when packaged and not Store
//! signed (Package::Current().SignatureKind() != Store). Check once a day and on demand.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, USER_AGENT};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use windows::ApplicationModel::{Package, PackageSignatureKind};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

const UA: &str = concat!("synka/", env!("CARGO_PKG_VERSION"));
const PACKAGE_NAME: &str = "Byggstyrning.Synka";
// TODO(cert): stays CN=FastADC until a new trusted signing certificate exists; see packaging/README.md.
const PACKAGE_PUBLISHER: &str = "CN=FastADC";
const SIGNING_CERT_DER: &[u8] = include_bytes!("../packaging/FastADC.cer");

#[derive(Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub asset_url: String,
}

/// Ok(None) when up to date or not applicable (unpackaged).
pub async fn check(http: &reqwest::Client, channel: &str) -> Result<Option<UpdateInfo>> {
    // No channel configured (the default): no network request.
    if channel.trim().is_empty() {
        return Ok(None);
    }
    if !sideloaded_package().await {
        return Ok(None);
    }

    let url = format!("https://api.github.com/repos/{channel}/releases/latest");
    let response = http
        .get(&url)
        .header(USER_AGENT, UA)
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        // A withdrawn last release makes /releases/latest return 404. Clear any
        // pending install instead of keeping its now-stale asset URL queued.
        return Ok(None);
    }
    let release: serde_json::Value = response.error_for_status()?.json().await?;

    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow!("release has no tag_name"))?;
    let latest = semver::Version::parse(tag.trim_start_matches('v'))
        .with_context(|| format!("release tag '{tag}' is not semver"))?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
    if latest <= current {
        return Ok(None);
    }

    let asset_url = release_asset_url(&release, channel, &latest);

    match asset_url {
        Some(asset_url) => Ok(Some(UpdateInfo {
            version: latest.to_string(),
            asset_url,
        })),
        None => {
            tracing::warn!(
                "update {latest} available but release has no matching signed x64 MSIX asset"
            );
            Ok(None)
        }
    }
}

fn release_asset_url(
    release: &serde_json::Value,
    channel: &str,
    version: &semver::Version,
) -> Option<String> {
    if !channel.split('/').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    }) || channel.matches('/').count() != 1
    {
        return None;
    }
    let name = format!("Synka_{version}.0_x64.msix");
    let path = format!("/{channel}/releases/download/v{version}/{name}");
    release["assets"].as_array()?.iter().find_map(|asset| {
        if asset["name"].as_str()? != name {
            return None;
        }
        let candidate = asset["browser_download_url"].as_str()?;
        let url = url::Url::parse(candidate).ok()?;
        (url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == path)
            .then(|| candidate.to_string())
    })
}

/// Download + hand to PackageManager. The app is shut down by Windows when it succeeds.
pub async fn install(http: &reqwest::Client, u: &UpdateInfo) -> Result<()> {
    let version = semver::Version::parse(&u.version).context("invalid update version")?;
    if !version.pre.is_empty()
        || !version.build.is_empty()
        || [version.major, version.minor, version.patch]
            .iter()
            .any(|v| *v > 65_535)
    {
        bail!("update version cannot be represented by an MSIX identity");
    }
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|e| anyhow!("creating update download name: {e}"))?;
    let path = std::env::temp_dir().join(format!(
        "synka-update-{:016x}.msix",
        u64::from_le_bytes(nonce)
    ));
    let resp = http
        .get(&u.asset_url)
        .header(USER_AGENT, UA)
        .send()
        .await?
        .error_for_status()?;
    let download = async {
        let mut file = tokio::fs::File::create(&path).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk?).await?;
        }
        file.flush().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = download {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    tracing::info!("downloaded update {} to {}", u.version, path.display());

    // Hand the package to Windows. Add-AppxPackage runs the same deployment as
    // PackageManager.AddPackageAsync(uri, None, ForceTargetApplicationShutdown); Windows
    // shuts this process down when the deployment starts, so the call only returns on failure.
    // (The windows crate's AddPackageAsync needs an IIterable type that is not re-exported
    // without a direct windows-collections dependency.)
    let path_text = path
        .to_str()
        .ok_or_else(|| anyhow!("update download path is not valid Unicode"))?
        .replace('\'', "''");
    let cert_hash = Sha256::digest(SIGNING_CERT_DER)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<String>();
    let expected_version = format!("{version}.0");
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // TODO(cert): the signer check below stays CN=FastADC until a new trusted signing certificate
        // exists; see packaging/README.md. Keep this comment out of the PowerShell script text.
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
$path = '{path_text}'
try {{
$signature = Get-AuthenticodeSignature -LiteralPath $path
if ($signature.Status -ne 'Valid' -or -not $signature.SignerCertificate) {{ throw 'The update signature is not valid.' }}
$sha = [Security.Cryptography.SHA256]::Create()
try {{ $actualCertHash = [BitConverter]::ToString($sha.ComputeHash($signature.SignerCertificate.RawData)).Replace('-', '') }}
finally {{ $sha.Dispose() }}
if ($actualCertHash -ne '{cert_hash}') {{ throw 'The update signer is not the trusted Synka release certificate.' }}
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead($path)
try {{
    $entry = $archive.GetEntry('AppxManifest.xml')
    if (-not $entry) {{ throw 'The update has no package manifest.' }}
    $reader = New-Object IO.StreamReader($entry.Open())
    try {{ [xml]$manifest = $reader.ReadToEnd() }}
    finally {{ $reader.Dispose() }}
}}
finally {{ $archive.Dispose() }}
$identity = $manifest.SelectSingleNode("/*[local-name()='Package']/*[local-name()='Identity']")
if (-not $identity -or $identity.GetAttribute('Name') -ne '{PACKAGE_NAME}' -or
    $identity.GetAttribute('Publisher') -ne '{PACKAGE_PUBLISHER}' -or
    $identity.GetAttribute('ProcessorArchitecture') -ne 'x64' -or
    $identity.GetAttribute('Version') -ne '{expected_version}') {{
    throw 'The update package identity or version does not match the release.'
}}
Add-AppxPackage -Path $path -ForceTargetApplicationShutdown -ErrorAction Stop
}}
finally {{ Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue }}"#
        );
        let script_utf16 = script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
        let encoded = base64::engine::general_purpose::STANDARD.encode(script_utf16);
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .context("starting powershell for Add-AppxPackage")?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            bail!("MSIX verification or installation failed: {}", detail.trim());
        }
        Ok(())
    })
    .await;
    let _ = tokio::fs::remove_file(&path).await;
    result??;
    Ok(())
}

/// True when running with package identity and the package is not Store-signed.
async fn sideloaded_package() -> bool {
    tokio::task::spawn_blocking(|| {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        match Package::Current() {
            Ok(pkg) => pkg
                .SignatureKind()
                .map(|k| k != PackageSignatureKind::Store)
                .unwrap_or(false),
            Err(_) => false,
        }
    })
    .await
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_channel_makes_no_request() {
        assert!(check(&reqwest::Client::new(), "").await.unwrap().is_none());
        assert!(check(&reqwest::Client::new(), "  ")
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn release_asset_must_match_version_channel_and_https_origin() {
        let version = semver::Version::parse("0.2.0").unwrap();
        let channel = "byggstyrning/synka-releases";
        let name = "Synka_0.2.0.0_x64.msix";
        let good = format!("https://github.com/{channel}/releases/download/v0.2.0/{name}");
        let release = serde_json::json!({ "assets": [
            { "name": "Other_0.2.0.0_x64.msix", "browser_download_url": "https://attacker.example/file.msix" },
            { "name": name, "browser_download_url": good }
        ] });
        assert_eq!(
            release_asset_url(&release, channel, &version),
            Some(good.clone())
        );

        for url in [
            format!("https://attacker.example/{channel}/releases/download/v0.2.0/{name}"),
            format!("http://github.com/{channel}/releases/download/v0.2.0/{name}"),
            format!("https://github.com/{channel}/releases/download/v0.3.0/{name}"),
            format!("{good}?download=1"),
        ] {
            let candidate =
                serde_json::json!({ "assets": [{ "name": name, "browser_download_url": url }] });
            assert_eq!(release_asset_url(&candidate, channel, &version), None);
        }
    }
}
