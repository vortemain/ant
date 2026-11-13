# Ant network-isolation setup (decision D-009 / D-013). Run ONCE per machine
# from an elevated PowerShell started by the SAME Windows user who runs Ant
# (the stored credential is DPAPI-protected for that user).
#
# What it changes (all reversible with teardown.ps1):
#   - local group  AntSandboxUsers
#   - local user   AntSandboxOffline (random password, never expires, hidden
#                  from the sign-in screen, member of Users + AntSandboxUsers)
#   - firewall     AntSandbox-BlockOutbound / AntSandbox-BlockInbound: block
#                  all protocols, all profiles, scoped ONLY to that user's SID
#   - files        %LOCALAPPDATA%\Ant\sandbox\offline.cred (DPAPI, current user)
#                  %LOCALAPPDATA%\Ant\sandbox\setup.json  (versioned marker)
# The password is generated here, stored only as a DPAPI blob, and never
# printed or logged.
param([switch]$DryRun)
$ErrorActionPreference = 'Stop'

$GroupName = 'AntSandboxUsers'
$UserName = 'AntSandboxOffline'
$RuleOut = 'AntSandbox-BlockOutbound'
$RuleIn = 'AntSandbox-BlockInbound'
$StateDir = Join-Path $env:LOCALAPPDATA 'Ant\sandbox'
$CredFile = Join-Path $StateDir 'offline.cred'
$Marker = Join-Path $StateDir 'setup.json'
$HideKey = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList'

if ($DryRun) {
    Write-Output "dry-run: would create/refresh local group '$GroupName'"
    Write-Output "dry-run: would create/refresh local user '$UserName' (random password, hidden from sign-in)"
    Write-Output "dry-run: would add firewall rules '$RuleOut' and '$RuleIn' (block, all protocols, all profiles) scoped to that user only"
    Write-Output "dry-run: would write DPAPI credential '$CredFile' and marker '$Marker'"
    exit 0
}

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell (Run as administrator) as the same user who runs Ant.'
}

$fw = New-Object -ComObject HNetCfg.FwPolicy2
if ($fw.LocalPolicyModifyState -ne 0) {
    throw "Local firewall rules are overridden by policy (LocalPolicyModifyState=$($fw.LocalPolicyModifyState)); network isolation cannot be guaranteed."
}
$disabled = Get-NetFirewallProfile | Where-Object { -not $_.Enabled }
if ($disabled) {
    throw "Firewall disabled for profile(s): $($disabled.Name -join ', '); enable the firewall first."
}

# Random 32-char password from a CSPRNG; never leaves this process except as a
# DPAPI blob.
$alphabet = [char[]]'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789!@#%^*-_=+'
$bytes = New-Object byte[] 64
[System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
$plain = -join ($bytes[0..31] | ForEach-Object { $alphabet[$_ % $alphabet.Length] })
$secure = ConvertTo-SecureString $plain -AsPlainText -Force
Remove-Variable plain

if (-not (Get-LocalGroup -Name $GroupName -ErrorAction SilentlyContinue)) {
    New-LocalGroup -Name $GroupName -Description 'Ant build sandbox accounts' | Out-Null
}
$user = Get-LocalUser -Name $UserName -ErrorAction SilentlyContinue
if ($user) {
    Set-LocalUser -Name $UserName -Password $secure -PasswordNeverExpires $true -AccountNeverExpires
} else {
    $user = New-LocalUser -Name $UserName -Password $secure -PasswordNeverExpires -AccountNeverExpires `
        -UserMayNotChangePassword -Description 'Ant build sandbox (network-blocked)'
}
$sid = (Get-LocalUser -Name $UserName).SID.Value
foreach ($g in @($GroupName, (Get-LocalGroup -SID 'S-1-5-32-545').Name)) {
    if (-not (Get-LocalGroupMember -Group $g -Member $UserName -ErrorAction SilentlyContinue)) {
        Add-LocalGroupMember -Group $g -Member $UserName
    }
}
if (-not (Test-Path $HideKey)) { New-Item -Path $HideKey -Force | Out-Null }
New-ItemProperty -Path $HideKey -Name $UserName -Value 0 -PropertyType DWord -Force | Out-Null

$sddl = "O:LSD:(A;;CC;;;$sid)"
foreach ($rule in @(@{ Name = $RuleOut; Dir = 'Outbound' }, @{ Name = $RuleIn; Dir = 'Inbound' })) {
    Remove-NetFirewallRule -Name $rule.Name -ErrorAction SilentlyContinue
    New-NetFirewallRule -Name $rule.Name -DisplayName "Ant sandbox: block $($rule.Dir.ToLower())" `
        -Description 'Blocks all network traffic for the Ant build sandbox account only.' `
        -Direction $rule.Dir -Action Block -Profile Any -Protocol Any -LocalUser $sddl -Enabled True | Out-Null
}

New-Item -ItemType Directory -Path $StateDir -Force | Out-Null
ConvertFrom-SecureString $secure | Set-Content -Path $CredFile -Encoding ASCII -NoNewline
$installer = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
[ordered]@{
    version = 1
    user = $UserName
    user_sid = $sid
    group = $GroupName
    rules = @($RuleOut, $RuleIn)
    installed_for_sid = $installer
    created = (Get-Date).ToUniversalTime().ToString('o')
} | ConvertTo-Json | Set-Content -Path $Marker -Encoding UTF8

# Read back: fail loudly if anything did not apply as intended.
foreach ($name in @($RuleOut, $RuleIn)) {
    $r = Get-NetFirewallRule -Name $name
    $filter = $r | Get-NetFirewallSecurityFilter
    if ($r.Action -ne 'Block' -or $r.Enabled -ne 'True' -or $filter.LocalUser -notlike "*$sid*") {
        throw "Firewall rule $name did not read back as a block scoped to $sid."
    }
}
Write-Output "setup ok: user=$UserName sid=$sid rules=$RuleOut,$RuleIn marker=$Marker"
