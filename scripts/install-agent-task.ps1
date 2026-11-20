<#
.SYNOPSIS
  Start the agent silently at every logon of the current user (Task Scheduler).

.DESCRIPTION
  There are deliberately no defaults for the task name: pick something unremarkable.
  The executable can be renamed freely before installing.

.EXAMPLE
  .\install-agent-task.ps1 -ExePath "$env:LOCALAPPDATA\Programs\x\helper.exe" -TaskName "Display Color Sync"
#>
param(
    [Parameter(Mandatory)] [string] $ExePath,
    [Parameter(Mandatory)] [string] $TaskName,
    [string] $AgentHome
)

$ErrorActionPreference = 'Stop'
$ExePath = (Resolve-Path $ExePath).Path

$arguments = 'run'
if ($AgentHome) { $arguments = "--home `"$AgentHome`" run" }

$user = "$env:USERDOMAIN\$env:USERNAME"
$action = New-ScheduledTaskAction -Execute $ExePath -Argument $arguments
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
# Note: a restart after a crash starts a new session, so the check-in is needed again.
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings | Out-Null

Write-Host "Installed task '$TaskName'. It starts at your next logon."
Write-Host "Start it now with: Start-ScheduledTask -TaskName '$TaskName'"
