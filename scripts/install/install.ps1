# Ant installer for Windows (D-043).
#
#   irm https://raw.githubusercontent.com/vortemain/ant/main/scripts/install/install.ps1 | iex
#
# Downloads ant.exe for this machine from GitHub Releases, verifies its
# SHA-256 against the release's SHA256SUMS, installs it into
# %LOCALAPPDATA%\Ant\bin, and adds that folder to your user PATH.
# No administrator rights are needed.
#
# Settings (environment):
#   ANT_VERSION         install this version (e.g. 0.2.0) instead of the latest
#   ANT_INSTALL_DIR     install folder (default: %LOCALAPPDATA%\Ant\bin)
#   ANT_NO_MODIFY_PATH  set to 1 to leave PATH alone
#   ANT_DOWNLOAD_BASE   mirror laid out like https://github.com/vortemain/ant/releases

# Everything runs in a child scope, so `irm | iex` does not leave settings
# or variables behind in your PowerShell session.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is far slower with a progress bar

    function Fail([string]$message) {
        Write-Host "ant install: $message" -ForegroundColor Red
        throw "ant install failed"
    }

    $base = if ($env:ANT_DOWNLOAD_BASE) { $env:ANT_DOWNLOAD_BASE.TrimEnd('/') } else { 'https://github.com/vortemain/ant/releases' }
    $installDir = if ($env:ANT_INSTALL_DIR) { $env:ANT_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Ant\bin' }

    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { $target = 'x86_64-pc-windows-msvc' }
        'ARM64' { $target = 'aarch64-pc-windows-msvc' }
        default { Fail "unsupported CPU architecture: $arch" }
    }
    $asset = "ant-$target.exe"
    $urlBase = if ($env:ANT_VERSION) { "$base/download/v$($env:ANT_VERSION.TrimStart('v'))" } else { "$base/latest/download" }

    # TLS 1.2 for Windows PowerShell 5.1 on older systems.
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("ant-install-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null
    try {
        Write-Host "Downloading Ant for $target..."
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$urlBase/$asset" -OutFile (Join-Path $tmp $asset)
            Invoke-WebRequest -UseBasicParsing -Uri "$urlBase/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
        } catch {
            Fail "could not download from $urlBase ($($_.Exception.Message))"
        }

        $expected = $null
        foreach ($line in Get-Content (Join-Path $tmp 'SHA256SUMS')) {
            $parts = $line -split '\s+', 2
            if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $asset) { $expected = $parts[0].ToLowerInvariant() }
        }
        if (-not $expected) { Fail "SHA256SUMS has no entry for $asset" }
        $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash.ToLowerInvariant()
        if ($expected -ne $actual) { Fail "checksum mismatch for $asset (expected $expected, got $actual); nothing was installed" }

        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        $destination = Join-Path $installDir 'ant.exe'
        if (Test-Path $destination) {
            # A running ant.exe cannot be overwritten, but it can be renamed.
            $old = "$destination.old"
            Remove-Item $old -Force -ErrorAction SilentlyContinue
            Move-Item $destination $old -Force
        }
        Move-Item (Join-Path $tmp $asset) $destination -Force
        $version = & $destination --version
        Write-Host "Installed $version to $destination"

        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $entries = @($userPath -split ';' | Where-Object { $_ })
        if (($entries -notcontains $installDir) -and ($env:ANT_NO_MODIFY_PATH -ne '1')) {
            [Environment]::SetEnvironmentVariable('Path', (($entries + $installDir) -join ';'), 'User')
            $env:Path = "$env:Path;$installDir"
            Write-Host "Added $installDir to your user PATH (open a new terminal for other windows to see it)."
        }
        Write-Host ""
        Write-Host "Get started:"
        Write-Host "  cd your-project; ant build"
        Write-Host "Update later with: ant upgrade"
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}
