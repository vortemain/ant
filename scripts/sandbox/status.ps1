# Non-elevated verification of the Ant network-isolation setup. Prints
# key=value lines and exits 0 only when every check passes (fail closed).
$ErrorActionPreference = 'Stop'
$fail = @()
$StateDir = Join-Path $env:LOCALAPPDATA 'Ant\sandbox'
$Marker = Join-Path $StateDir 'setup.json'
$CredFile = Join-Path $StateDir 'offline.cred'

if (-not (Test-Path $Marker)) { Write-Output 'check=marker ok=false'; Write-Output 'ready=false'; exit 1 }
$m = Get-Content $Marker -Raw | ConvertFrom-Json
Write-Output "user=$($m.user)"
Write-Output "user_sid=$($m.user_sid)"
$me = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
if ($m.version -ne 1) { $fail += 'marker-version' }
if ($m.installed_for_sid -ne $me) { $fail += 'installed-for-another-user' }

$u = Get-LocalUser -Name $m.user -ErrorAction SilentlyContinue
if (-not $u -or $u.SID.Value -ne $m.user_sid -or -not $u.Enabled) { $fail += 'user' }
try { $null = Get-Content $CredFile -Raw | ConvertTo-SecureString } catch { $fail += 'credential' }

try {
    if ((New-Object -ComObject HNetCfg.FwPolicy2).LocalPolicyModifyState -ne 0) { $fail += 'firewall-policy-override' }
} catch { $fail += 'firewall-policy-unreadable' }
if (Get-NetFirewallProfile | Where-Object { -not $_.Enabled }) { $fail += 'firewall-profile-disabled' }
foreach ($name in $m.rules) {
    $r = Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue
    if (-not $r) { $fail += "rule-missing:$name"; continue }
    $filter = $r | Get-NetFirewallSecurityFilter
    if ($r.Action -ne 'Block' -or $r.Enabled -ne 'True' -or $filter.LocalUser -notlike "*$($m.user_sid)*") {
        $fail += "rule-wrong:$name"
    }
}
foreach ($f in $fail) { Write-Output "failed=$f" }
if ($fail.Count -gt 0) { Write-Output 'ready=false'; exit 1 }
Write-Output 'ready=true'
exit 0
