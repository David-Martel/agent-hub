# Install the agent-hub SFTP drop on a Windows (Win32-OpenSSH) host. Idempotent.
# Used for dtm-p1gen7 (primary) and dtm-p1gen9 (mirror); owner approval 2026-10-29 14:26 EDT
# (bus stream 1793298483900-0, "option A"). Run elevated, e.g. over ssh with -EncodedCommand.
#
# Result: local user agenthubdrop (random password never stored, key auth only), confined by
# sshd_config "Match User agenthubdrop" to ForceCommand internal-sftp + ChrootDirectory
# C:\codedev\agent-hub-artifacts\incoming. The key file lives in ProgramData (no profile needed),
# owned by Administrators. The incoming DACL lets the drop list, create folders and create files,
# but files only grant it read: overwrite, delete, rename and re-ACL are denied (OWNER RIGHTS = RX).
# Controls proven on dtm-p1gen7 and dtm-p1gen9: shell refused, chroot escape denied, overwrite and
# delete denied, readback byte-equal (p1gen7 also: rename denied, port forward prohibited).
# Design, controls and runbook: ../README.md. The only key material here is the public key.
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$user = 'agenthubdrop'
$pub = 'restrict ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDtalSTc5mGYf75FclQYxJzkfPVjOfEN4GY3V8BDOJF4 agent-hub-drop@asuspro13'
$root = 'C:\codedev\agent-hub-artifacts\incoming'
$keys = 'C:\ProgramData\ssh\agenthubdrop_authorized_keys'
$cfg = 'C:\ProgramData\ssh\sshd_config'
$backup = 'C:\ProgramData\ssh\sshd_config.pre-agenthubdrop'
$sshd = 'C:\Windows\System32\OpenSSH\sshd.exe'

if (-not (Get-LocalUser -Name $user -ErrorAction SilentlyContinue)) {
    # Built straight into a SecureString, so no plaintext copy of the password ever exists.
    # Nobody needs it: the account logs in by key only. The fixed tail meets complexity policy.
    $alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789'
    $bytes = New-Object byte[] 48
    [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $sec = New-Object Security.SecureString
    foreach ($b in $bytes) { $sec.AppendChar($alphabet[$b % $alphabet.Length]) }
    foreach ($c in 'aA1!'.ToCharArray()) { $sec.AppendChar($c) }
    $sec.MakeReadOnly()
    New-LocalUser -Name $user -Password $sec -PasswordNeverExpires -UserMayNotChangePassword -AccountNeverExpires `
        -Description 'agent-hub SFTP drop (chroot incoming, key only)' | Out-Null
    $sec.Dispose()
    Remove-Variable sec, bytes
}
Add-LocalGroupMember -Group Users -Member $user -ErrorAction SilentlyContinue
"user: $user enabled=" + (Get-LocalUser $user).Enabled

New-Item -ItemType Directory -Force -Path $root | Out-Null
$F = [Security.AccessControl.FileSystemRights]; $I = [Security.AccessControl.InheritanceFlags]
$P = [Security.AccessControl.PropagationFlags]; $A = [Security.AccessControl.AccessControlType]::Allow
function Rule($who, $rights, $inh, $prop) {
    $sid = if ($who -like 'S-*') { New-Object Security.Principal.SecurityIdentifier($who) } else { (New-Object Security.Principal.NTAccount($who)).Translate([Security.Principal.SecurityIdentifier]) }
    New-Object Security.AccessControl.FileSystemAccessRule($sid, $rights, $inh, $prop, $A)
}
$both = $I::ContainerInherit -bor $I::ObjectInherit
$rx = $F::ReadAndExecute -bor $F::Synchronize
$dirWrite = $rx -bor $F::CreateFiles -bor $F::CreateDirectories -bor $F::WriteAttributes -bor $F::WriteExtendedAttributes
$acl = New-Object Security.AccessControl.DirectorySecurity
$acl.SetAccessRuleProtection($true, $false)
$acl.SetOwner((New-Object Security.Principal.NTAccount('BUILTIN\Administrators')))
$acl.AddAccessRule((Rule 'S-1-5-18' $F::FullControl $both $P::None))
$acl.AddAccessRule((Rule 'S-1-5-32-544' $F::FullControl $both $P::None))
$acl.AddAccessRule((Rule "$env:COMPUTERNAME\$env:USERNAME" $F::FullControl $both $P::None))
$acl.AddAccessRule((Rule 'S-1-5-32-545' $rx $both $P::None))
$acl.AddAccessRule((Rule "$env:COMPUTERNAME\$user" $dirWrite $I::ContainerInherit $P::None))
$acl.AddAccessRule((Rule "$env:COMPUTERNAME\$user" $rx $I::ObjectInherit $P::InheritOnly))
$acl.AddAccessRule((Rule 'S-1-3-4' $rx $both $P::None))
Set-Acl -LiteralPath $root -AclObject $acl
if (Get-ChildItem -Force $root) { & icacls "$root\*" /reset /T /C /Q | Out-Null }
& icacls $root

[IO.File]::WriteAllText($keys, $pub + "`n", (New-Object Text.UTF8Encoding($false)))
& icacls $keys /setowner Administrators | Out-Null
& icacls $keys /inheritance:r /grant:r 'SYSTEM:F' 'Administrators:F' "${user}:R" | Out-Null
if ($LASTEXITCODE) { throw "icacls keys failed $LASTEXITCODE" }

$text = [IO.File]::ReadAllText($cfg)
$restart = $false
if ($text -notmatch '(?m)^Match User agenthubdrop') {
    if (-not (Test-Path $backup)) { Copy-Item $cfg $backup }
    $block = "Match User agenthubdrop`r`n       AuthorizedKeysFile __PROGRAMDATA__/ssh/agenthubdrop_authorized_keys`r`n       ChrootDirectory C:\codedev\agent-hub-artifacts\incoming`r`n       ForceCommand internal-sftp`r`n       PasswordAuthentication no`r`n       PermitTTY no`r`n       AllowTcpForwarding no`r`n       AllowAgentForwarding no`r`n       GatewayPorts no`r`n"
    $candidate = Join-Path $env:TEMP 'sshd_config.candidate'
    [IO.File]::WriteAllText($candidate, $text.TrimEnd() + "`r`n`r`n" + $block, (New-Object Text.UTF8Encoding($false)))
    & $sshd -t -f $candidate
    if ($LASTEXITCODE) { throw "sshd -t rejected candidate ($LASTEXITCODE); live config untouched" }
    Copy-Item $candidate $cfg -Force
    Remove-Item $candidate
    $restart = $true
    "sshd_config: Match block appended and validated"
} else { "sshd_config: Match block already present" }
& $sshd -t
if ($LASTEXITCODE) { throw "live sshd_config fails -t" }

if ($restart) {
    # A Restart-Service run inside this ssh session would kill the session before the start.
    $tn = 'agenthubdrop-sshd-restart'
    $act = New-ScheduledTaskAction -Execute 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' -Argument '-NoProfile -NonInteractive -Command "Restart-Service -Name sshd -Force"'
    Register-ScheduledTask -TaskName $tn -Action $act -Principal (New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest) -Force | Out-Null
    Start-ScheduledTask -TaskName $tn
    "sshd restart task started (remove it afterwards: Unregister-ScheduledTask $tn)"
}
