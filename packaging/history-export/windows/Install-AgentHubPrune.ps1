# Install the agent-hub drop prune on a Windows drop host. Idempotent; run elevated.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File Install-AgentHubPrune.ps1 [-Source <Invoke-AgentHubPrune.ps1>]
#
# Result: local user agenthubprune (no groups), granted "Log on as a batch job" only. Each run
# sets a fresh random password, hands it to Task Scheduler (which keeps it as an LSA secret
# for the task's password logon) and discards it; no file or output ever holds it. S4U logon
# was the first choice but Register-ScheduledTask refuses it on dtm-p1gen7 with
# E_ACCESSDENIED, as SYSTEM too, cause not found (2026-10-29). The scheduled task
# agent-hub-prune runs C:\ProgramData\agent-hub-prune\Invoke-AgentHubPrune.ps1 daily as
# that user. The user may list, read and delete inside the drop's incoming folder and
# write the prune log; it cannot change the script it runs. The drop account
# (agenthubdrop) is untouched and still cannot delete. Design and runbook: ../README.md.
#
# -TaskName, -PruneArguments and -NoTrigger register a second, untriggered task for the
# self-test in ../README.md ("Retention"), which runs the real account against a scratch root.
param(
    [string]$Source = (Join-Path $PSScriptRoot 'Invoke-AgentHubPrune.ps1'),
    [int]$Keep = 14,
    [string]$At = '03:30',
    [string]$TaskName = 'agent-hub-prune',
    [string]$PruneArguments = '',
    [switch]$NoTrigger
)
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$user = 'agenthubprune'
$root = 'C:\codedev\agent-hub-artifacts\incoming'
$logDir = 'C:\codedev\agent-hub-artifacts\prune-log'
$binDir = 'C:\ProgramData\agent-hub-prune'
$script = Join-Path $binDir 'Invoke-AgentHubPrune.ps1'

if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "prune script $Source not found" }
if (-not (Test-Path -LiteralPath $root -PathType Container)) { throw "drop root $root missing; run Install-AgentHubDrop.ps1 first" }

# Built straight into a SecureString, so no plaintext copy exists until the one call that
# hands it to Task Scheduler. The fixed tail meets complexity policy.
$alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789'
$bytes = New-Object byte[] 48
[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
$sec = New-Object Security.SecureString
foreach ($b in $bytes) { $sec.AppendChar($alphabet[$b % $alphabet.Length]) }
foreach ($c in 'aA1!'.ToCharArray()) { $sec.AppendChar($c) }
$sec.MakeReadOnly()
Remove-Variable bytes
if (Get-LocalUser -Name $user -ErrorAction SilentlyContinue) {
    Set-LocalUser -Name $user -Password $sec
} else {
    New-LocalUser -Name $user -Password $sec -PasswordNeverExpires -UserMayNotChangePassword -AccountNeverExpires `
        -Description 'agent-hub drop prune (scheduled task)' | Out-Null
}
$sid = (Get-LocalUser -Name $user).SID
"user: $user sid=$($sid.Value)"

# A task's password logon is a batch logon, which needs SeBatchLogonRight. LsaAddAccountRights adds it for this SID only and
# leaves every other account's rights as they are (no secedit round trip).
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;
public static class AgentHubLsa {
    [StructLayout(LayoutKind.Sequential)]
    struct LsaUnicodeString { public ushort Length; public ushort MaximumLength; public IntPtr Buffer; }
    [StructLayout(LayoutKind.Sequential)]
    struct LsaObjectAttributes { public int Length; public IntPtr RootDirectory; public IntPtr ObjectName; public int Attributes; public IntPtr SecurityDescriptor; public IntPtr SecurityQualityOfService; }
    [DllImport("advapi32.dll")] static extern uint LsaOpenPolicy(IntPtr system, ref LsaObjectAttributes attrs, int access, out IntPtr handle);
    [DllImport("advapi32.dll")] static extern uint LsaAddAccountRights(IntPtr handle, byte[] sid, LsaUnicodeString[] rights, int count);
    [DllImport("advapi32.dll")] static extern uint LsaClose(IntPtr handle);
    [DllImport("advapi32.dll")] static extern int LsaNtStatusToWinError(uint status);
    public static void AddRight(string sidString, string right) {
        var sid = new SecurityIdentifier(sidString);
        var raw = new byte[sid.BinaryLength];
        sid.GetBinaryForm(raw, 0);
        var attrs = new LsaObjectAttributes();
        IntPtr handle;
        // POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES
        uint status = LsaOpenPolicy(IntPtr.Zero, ref attrs, 0x00000010 | 0x00000800, out handle);
        if (status != 0) throw new Win32Exception(LsaNtStatusToWinError(status));
        var name = new LsaUnicodeString();
        name.Buffer = Marshal.StringToHGlobalUni(right);
        name.Length = (ushort)(right.Length * 2);
        name.MaximumLength = (ushort)((right.Length + 1) * 2);
        try {
            status = LsaAddAccountRights(handle, raw, new[] { name }, 1);
            if (status != 0) throw new Win32Exception(LsaNtStatusToWinError(status));
        } finally {
            Marshal.FreeHGlobal(name.Buffer);
            LsaClose(handle);
        }
    }
}
'@
[AgentHubLsa]::AddRight($sid.Value, 'SeBatchLogonRight')
'right: SeBatchLogonRight granted'

$F = [Security.AccessControl.FileSystemRights]; $I = [Security.AccessControl.InheritanceFlags]
$P = [Security.AccessControl.PropagationFlags]; $A = [Security.AccessControl.AccessControlType]::Allow
$both = $I::ContainerInherit -bor $I::ObjectInherit
$rx = $F::ReadAndExecute -bor $F::Synchronize
function Rule($who, $rights, $inh, $prop) {
    $s = if ($who -is [Security.Principal.SecurityIdentifier]) { $who } elseif ($who -like 'S-*') { New-Object Security.Principal.SecurityIdentifier($who) } else { (New-Object Security.Principal.NTAccount($who)).Translate([Security.Principal.SecurityIdentifier]) }
    New-Object Security.AccessControl.FileSystemAccessRule($s, $rights, $inh, $prop, $A)
}
function Set-ExactAcl($path, $rules) {
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true, $false)
    $acl.SetOwner((New-Object Security.Principal.NTAccount('BUILTIN\Administrators')))
    foreach ($r in $rules) { $acl.AddAccessRule($r) }
    Set-Acl -LiteralPath $path -AclObject $acl
}
$admins = @((Rule 'S-1-5-18' $F::FullControl $both $P::None), (Rule 'S-1-5-32-544' $F::FullControl $both $P::None))

# The prune's rights on incoming. Install-AgentHubDrop.ps1 rebuilds the incoming DACL from
# scratch and re-adds these same two ACEs when this user exists; keep the two in step.
$pruneDirRights = $rx -bor $F::DeleteSubdirectoriesAndFiles
$acl = Get-Acl -LiteralPath $root
$acl.AddAccessRule((Rule $sid $pruneDirRights $I::ContainerInherit $P::None))
$acl.AddAccessRule((Rule $sid $rx $I::ObjectInherit $P::InheritOnly))
Set-Acl -LiteralPath $root -AclObject $acl
if (Get-ChildItem -Force $root) { & icacls "$root\*" /reset /T /C /Q | Out-Null }

New-Item -ItemType Directory -Force -Path $logDir, $binDir | Out-Null
$modify = $F::Modify -bor $F::Synchronize
Set-ExactAcl $logDir ($admins + @((Rule $sid $modify $both $P::None)))
Set-ExactAcl $binDir ($admins + @((Rule $sid $rx $both $P::None)))
Copy-Item -LiteralPath $Source -Destination $script -Force
& icacls $script /reset | Out-Null
"script: $script sha256=" + (Get-FileHash -Algorithm SHA256 -LiteralPath $script).Hash.ToLowerInvariant()

$ps = 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe'
$task = @{
    TaskName    = $TaskName
    Action      = New-ScheduledTaskAction -Execute $ps -Argument ("-NoProfile -NonInteractive -ExecutionPolicy Bypass " +
                  "-File `"$script`" -Keep $Keep $PruneArguments").Trim()
    User        = "$env:COMPUTERNAME\$user"
    RunLevel    = 'Limited'
    Settings    = New-ScheduledTaskSettingsSet -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
                  -ExecutionTimeLimit (New-TimeSpan -Hours 1) -MultipleInstances IgnoreNew
    Description = 'Prune agent-hub drop bundles: keep newest N plus referenced; hash-verify and log each removal'
}
if (-not $NoTrigger) { $task.Trigger = New-ScheduledTaskTrigger -Daily -At $At }
try {
    $cred = New-Object Management.Automation.PSCredential($task.User, $sec)
    Register-ScheduledTask @task -Password $cred.GetNetworkCredential().Password -Force | Out-Null
    # The password just changed, so every other task of this account (the main task when this
    # run registered a self-test task, or the reverse) gets the new one too.
    Get-ScheduledTask | Where-Object { $_.Principal.UserId -like "*\$user" -or $_.Principal.UserId -eq $user } |
        Where-Object { $_.TaskName -ne $TaskName } | ForEach-Object {
            Set-ScheduledTask -TaskName $_.TaskName -TaskPath $_.TaskPath -User $task.User `
                -Password $cred.GetNetworkCredential().Password | Out-Null
            "task: refreshed the credential of $($_.TaskName)"
        }
} finally {
    Remove-Variable cred -ErrorAction SilentlyContinue
    $sec.Dispose()
}
# Register-ScheduledTask can report access denied as a non-terminating error and return, so
# read the task back instead of trusting the call (it fooled the S4U diagnostics).
$t = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
if ($t.Principal.UserId -notlike "*$user" -or $t.Principal.LogonType -ne 'Password') {
    throw "task $TaskName registered with an unexpected principal: $($t.Principal.UserId) $($t.Principal.LogonType)"
}
"task: $TaskName as $($t.Principal.UserId) ($($t.Principal.LogonType)), keep=$Keep, " +
    $(if ($NoTrigger) { 'no trigger' } else { "daily at $At" })
& icacls $root
