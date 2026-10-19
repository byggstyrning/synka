# Shared by build, delivery and release-resume validation. An outer MSIX signature
# does not give its embedded notification-area executable an Authenticode identity.

function Assert-ReleaseFileSignature {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][Security.Cryptography.X509Certificates.X509Certificate2]$Certificate
    )

    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    # Require Windows to validate the signature and trust chain. UnknownError
    # cannot establish integrity and must never stand in for an untrusted root.
    if ($signature.Status -ne 'Valid' -or -not $signature.SignerCertificate) {
        throw "$Path is not validly signed (status: $($signature.Status), $($signature.StatusMessage))."
    }
    if ($signature.SignerCertificate.Thumbprint -ne $Certificate.Thumbprint) {
        throw "$Path is not signed by the committed release certificate ($($Certificate.Thumbprint))."
    }
    if (-not $signature.TimeStamperCertificate) {
        throw "$Path has no signature timestamp. Sign it with an RFC 3161 timestamp before packaging."
    }
    return $signature
}

function Assert-PackageEngine {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$PackagePath,
        [Parameter(Mandatory = $true)][Security.Cryptography.X509Certificates.X509Certificate2]$Certificate,
        [string]$ExpectedEnginePath
    )

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $scratch = Join-Path ([IO.Path]::GetTempPath()) ("synka-engine-signature-" + [guid]::NewGuid().ToString('N'))
    $engine = Join-Path $scratch 'synka-engine.exe'
    [void][IO.Directory]::CreateDirectory($scratch)
    try {
        $archive = [IO.Compression.ZipFile]::OpenRead($PackagePath)
        try {
            $entries = @($archive.Entries | Where-Object {
                $_.FullName.Replace('\', '/').Equals('engine/synka-engine.exe', [StringComparison]::OrdinalIgnoreCase)
            })
            if ($entries.Count -ne 1) {
                throw "$PackagePath must contain exactly one engine/synka-engine.exe; found $($entries.Count)."
            }
            $source = $entries[0].Open()
            try {
                $destination = [IO.File]::Open($engine, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
                try { $source.CopyTo($destination) }
                finally { $destination.Dispose() }
            }
            finally { $source.Dispose() }
        }
        finally { $archive.Dispose() }

        $hash = (Get-FileHash -LiteralPath $engine -Algorithm SHA256).Hash
        if ($ExpectedEnginePath -and $hash -ne (Get-FileHash -LiteralPath $ExpectedEnginePath -Algorithm SHA256).Hash) {
            throw "The embedded engine in $PackagePath does not match $ExpectedEnginePath (SHA256)."
        }
        $signature = Assert-ReleaseFileSignature -Path $engine -Certificate $Certificate
        return [pscustomobject]@{
            sha256 = $hash
            length = (Get-Item -LiteralPath $engine).Length
            signer = $signature.SignerCertificate.Thumbprint
            timestampSigner = $signature.TimeStamperCertificate.Thumbprint
        }
    }
    finally {
        # Only our fixed extracted file is removed; never recursively delete archive paths.
        try { [IO.File]::Delete($engine) } catch { }
        try { [IO.Directory]::Delete($scratch) } catch { }
    }
}
