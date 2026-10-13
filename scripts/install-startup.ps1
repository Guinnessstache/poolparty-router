# Starts the PoolParty router (and optionally the 4070 Ti model) hidden at logon.
#   powershell -ExecutionPolicy Bypass -File scripts\install-startup.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\install-startup.ps1 -WithModel
# Remove with scripts\uninstall-startup.ps1.

param([switch]$WithModel)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot -Parent
$exe  = Join-Path $root 'target\release\poolparty-router.exe'
$cfg  = Join-Path $root 'router.toml'

if (-not (Test-Path $exe)) {
    Write-Host 'Building the router first...'
    Push-Location $root; cargo build --release; Pop-Location
    if (-not (Test-Path $exe)) { throw "Build failed: $exe not found." }
}
if (-not (Test-Path $cfg)) { throw "Create $cfg first (run start-router.cmd once and edit it)." }

# Without a log file, nothing the hidden router prints can be seen.
if (-not (Select-String -Path $cfg -Pattern '^\s*log_file\s*=' -Quiet)) {
    Write-Host 'Tip: add  log_file = "router.log"  under [router] in router.toml to keep logs.'
}

# S4U runs hidden with no stored password. Some Windows setups only allow
# registering it from an elevated PowerShell; if so, we fall back to a normal
# logon task, which shows a console window you can minimize.
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType S4U -RunLevel Limited
$fallback  = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited

function Register-PP($name, $action) {
    try {
        Register-ScheduledTask -TaskName $name -Action $action -Trigger $trigger `
            -Principal $principal -Settings $settings -Force -ErrorAction Stop | Out-Null
    } catch {
        Write-Host "Hidden mode needs an admin PowerShell; installing '$name' with a visible window instead."
        Register-ScheduledTask -TaskName $name -Action $action -Trigger $trigger `
            -Principal $fallback -Settings $settings -Force | Out-Null
    }
    Start-ScheduledTask -TaskName $name
}
$trigger   = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings  = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
               -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 5 -RestartInterval (New-TimeSpan -Minutes 1) `
               -MultipleInstances IgnoreNew

$action = New-ScheduledTaskAction -Execute $exe -Argument "`"$cfg`"" -WorkingDirectory $root
Register-PP 'PoolParty Router' $action
Write-Host 'PoolParty Router: installed and started. Dashboard: http://127.0.0.1:4000/'

if ($WithModel) {
    $bat = Join-Path $PSScriptRoot 'jayspc-model.cmd'
    $action = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument "/c `"$bat`"" -WorkingDirectory $root
    Register-PP 'PoolParty 4070 Ti model' $action
    Write-Host 'PoolParty 4070 Ti model: installed and started (scripts\jayspc-model.cmd).'
}
