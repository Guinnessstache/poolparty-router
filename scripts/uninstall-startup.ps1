# Stops and removes the PoolParty scheduled tasks created by install-startup.ps1.
foreach ($name in 'PoolParty Router', 'PoolParty 4070 Ti model') {
    if (Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $name -Confirm:$false
        Write-Host "Removed: $name"
    }
}
# Make sure no hidden copy is left running.
Get-Process poolparty-router -ErrorAction SilentlyContinue | Stop-Process
