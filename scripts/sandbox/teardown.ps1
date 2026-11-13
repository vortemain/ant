#Requires -RunAsAdministrator
# Remove everything scripts/sandbox/setup.ps1 created. Idempotent.
$ErrorActionPreference = 'Continue'
$UserName = 'AntSandboxOffline'
Remove-NetFirewallRule -Name 'AntSandbox-BlockOutbound' -ErrorAction SilentlyContinue
Remove-NetFirewallRule -Name 'AntSandbox-BlockInbound' -ErrorAction SilentlyContinue
Remove-LocalUser -Name $UserName -ErrorAction SilentlyContinue
Remove-LocalGroup -Name 'AntSandboxUsers' -ErrorAction SilentlyContinue
Remove-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList' `
    -Name $UserName -ErrorAction SilentlyContinue
Remove-Item -Path (Join-Path $env:LOCALAPPDATA 'Ant\sandbox') -Recurse -Force -ErrorAction SilentlyContinue
Write-Output 'teardown ok (the account profile folder under C:\Users, if one was created, can be deleted manually)'
