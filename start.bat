@echo off
setlocal DisableDelayedExpansion
rem Everything below the marker is embedded PowerShell; no helper scripts are required.
set "TRINITY_LAUNCHER=%~f0"
set "TRINITY_MODE=ui"
set "TRINITY_UI_PID="
set "TRINITY_RUN_ID="
if /i "%~1"=="--headless" set "TRINITY_MODE=headless"
if /i "%~1"=="--stop" set "TRINITY_MODE=stop"
set "TRINITY_PS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
if exist "%SystemRoot%\Sysnative\WindowsPowerShell\v1.0\powershell.exe" set "TRINITY_PS=%SystemRoot%\Sysnative\WindowsPowerShell\v1.0\powershell.exe"
if not "%TRINITY_MODE%"=="ui" goto console
start "" "%TRINITY_PS%" -NoLogo -NoProfile -ExecutionPolicy Bypass -STA -WindowStyle Hidden -Command "try { [AppContext]::SetSwitch('Switch.System.IO.UseLegacyPathHandling',$false); [AppContext]::SetSwitch('Switch.System.IO.BlockLongPaths',$false); $parts = [IO.File]::ReadAllText($env:TRINITY_LAUNCHER) -split '(?m)^#==TRINITY_POWERSHELL==\r?$', 2; if ($parts.Count -ne 2) { throw 'Launcher payload is missing.' }; & ([scriptblock]::Create($parts[1])) } catch { Add-Type -AssemblyName System.Windows.Forms; [Windows.Forms.MessageBox]::Show($_.Exception.Message,'Trinity could not open this folder','OK','Error') | Out-Null; exit 1 }"
exit /b
:console
"%TRINITY_PS%" -NoLogo -NoProfile -ExecutionPolicy Bypass -STA -Command "try { [AppContext]::SetSwitch('Switch.System.IO.UseLegacyPathHandling',$false); [AppContext]::SetSwitch('Switch.System.IO.BlockLongPaths',$false); $parts = [IO.File]::ReadAllText($env:TRINITY_LAUNCHER) -split '(?m)^#==TRINITY_POWERSHELL==\r?$', 2; if ($parts.Count -ne 2) { throw 'Launcher payload is missing.' }; & ([scriptblock]::Create($parts[1])) } catch { Write-Error $_; exit 1 }"
exit /b %errorlevel%
#==TRINITY_POWERSHELL==

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Self = [IO.Path]::GetFullPath($env:TRINITY_LAUNCHER)
$Root = [IO.Path]::GetDirectoryName($Self)
$SourceRoot = $Root
Set-Location -LiteralPath $Root
[Environment]::CurrentDirectory = $Root
$Runtime = Join-Path $Root '.trinity'
$Logs = Join-Path $Root 'logs'
$Utf8 = New-Object Text.UTF8Encoding($false)
[IO.Directory]::CreateDirectory($Runtime) | Out-Null
[IO.Directory]::CreateDirectory($Logs) | Out-Null
$LogPath = Join-Path $Logs 'launcher.log'
$StopPath = Join-Path $Runtime 'stop.request'
$StatusPath = Join-Path $Runtime 'status.json'
if (-not $env:TRINITY_RUN_ID) { $env:TRINITY_RUN_ID = [Guid]::NewGuid().ToString('N') }
$script:Status = [ordered]@{ runId = $env:TRINITY_RUN_ID; stage = 'Preparing'; message = 'Checking local setup'; progress = 0; ready = $false; error = $false; stopped = $false; gamePort = 0; webPort = 0 }
$script:Stopping = $false

function Write-Log([string]$Message) {
    $Message = $Message -replace '\x1B\[[0-9;]*[a-zA-Z]', ''
    [IO.File]::AppendAllText($LogPath, $Message + [Environment]::NewLine, $Utf8)
    if ($env:TRINITY_MODE -ne 'worker') { Write-Host $Message }
}
function Set-Status([string]$Stage, [string]$Message, [int]$Progress) {
    $script:Status['downloadPercent'] = $null
    $script:Status['downloadName'] = ''
    $script:Status['phasePercent'] = $null
    $script:Status['phaseLabel'] = ''
    $script:Status.stage = $Stage
    $script:Status.message = $Message
    $script:Status.progress = $Progress
    [IO.File]::WriteAllText($StatusPath, ($script:Status | ConvertTo-Json -Compress), $Utf8)
    Write-Log ('[{0}] {1}' -f $Stage, $Message)
}
function Assert-Running {
    if ($script:Stopping) { return }
    if (Test-Path -LiteralPath $StopPath) {
        $request = [IO.File]::ReadAllText($StopPath).Trim()
        if ($request -eq $env:TRINITY_RUN_ID -or $request -eq 'stop') { throw [OperationCanceledException]::new('Stop requested.') }
    }
    if ($env:TRINITY_UI_PID -and -not (Get-Process -Id ([int]$env:TRINITY_UI_PID) -ErrorAction SilentlyContinue)) {
        throw [OperationCanceledException]::new('Launcher window closed.')
    }
}
function Quote-Argument([string]$Value) {
    '"' + ([regex]::Replace([regex]::Replace($Value, '(\\*)"', '$1$1\"'), '(\\+)$', '$1$1')) + '"'
}
function Initialize-NativePathAliasType {
    if ('Trinity.NativePathAlias' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace Trinity {
    public static class NativePathAlias {
        [DllImport("kernel32.dll", EntryPoint = "DefineDosDeviceW", CharSet = CharSet.Unicode, SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool DefineDosDevice(uint flags, string device, string target);

        [DllImport("kernel32.dll", EntryPoint = "QueryDosDeviceW", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern uint QueryDosDevice(string device, StringBuilder target, int capacity);

        public static string Query(string device) {
            var target = new StringBuilder(32768);
            if (QueryDosDevice(device, target, target.Capacity) != 0)
                return target.ToString();
            int error = Marshal.GetLastWin32Error();
            if (error == 2) return null;
            throw new Win32Exception(error, "Cannot inspect drive " + device);
        }

        public static void Create(string device, string target) {
            // DDD_RAW_TARGET_PATH | DDD_NO_BROADCAST_SYSTEM.
            if (!DefineDosDevice(0x00000009, device, target))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot create temporary drive " + device);
        }

        public static void RemoveExact(string device, string target) {
            // RAW_TARGET_PATH | REMOVE_DEFINITION | EXACT_MATCH_ON_REMOVE | NO_BROADCAST_SYSTEM.
            if (!DefineDosDevice(0x0000000f, device, target)) {
                int error = Marshal.GetLastWin32Error();
                if (error != 2)
                    throw new Win32Exception(error, "Cannot remove temporary drive " + device);
            }
        }
    }
}
'@
}
function New-NativePathAlias {
    param([Parameter(Mandatory = $true)][string]$Path)
    Initialize-NativePathAliasType
    $source = [IO.Path]::GetFullPath($Path)
    if ($source.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {
        $target = '\??\UNC\' + $source.Substring(8)
    } elseif ($source.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) {
        $target = '\??\' + $source.Substring(4)
    } elseif ($source.StartsWith('\\', [StringComparison]::Ordinal)) {
        $target = '\??\UNC\' + $source.Substring(2)
    } else {
        $target = '\??\' + $source
    }
    if ($source.Length -gt [IO.Path]::GetPathRoot($source).Length) { $target = $target.TrimEnd('\') }

    # Reuse exact existing aliases first, so a surviving private database retains
    # the same path after an interrupted launch. Existing mappings are never removed.
    foreach ($reuse in @($true, $false)) {
        for ($letter = 90; $letter -ge 68; $letter--) {
            $drive = ([char]$letter).ToString() + ':'
            $mutex = [Threading.Mutex]::new($false, ('Local\Trinity.NativePathAlias.' + [char]$letter))
            $held = $false; $created = $false; $keep = $false
            try {
                try { $held = $mutex.WaitOne(0) }
                catch [Threading.AbandonedMutexException] { $held = $true }
                if (-not $held) { continue }
                $existing = [Trinity.NativePathAlias]::Query($drive)
                if ($reuse) {
                    if (-not [string]::Equals($existing, $target, [StringComparison]::OrdinalIgnoreCase)) { continue }
                } else {
                    if ($null -ne $existing) { continue }
                    [Trinity.NativePathAlias]::Create($drive, $target)
                    $created = $true
                }
                if (-not [string]::Equals([Trinity.NativePathAlias]::Query($drive), $target, [StringComparison]::OrdinalIgnoreCase)) {
                    throw "Temporary drive $drive changed while preparing Trinity. Run start.bat again."
                }
                # Register the new OS drive with this PowerShell session too.
                # Sessions started before the mapping may not have mounted it yet.
                $driveName = ([char]$letter).ToString()
                if (-not (Get-PSDrive -Name $driveName -PSProvider FileSystem -ErrorAction SilentlyContinue)) {
                    New-PSDrive -Name $driveName -PSProvider FileSystem -Root ($drive + '\') -Scope Script | Out-Null
                }
                $alias = [pscustomobject]@{
                    Root = $drive + '\'; Drive = $drive; Target = $target
                    SourcePath = $source; Mutex = $mutex; Created = $created; Removed = $false
                }
                $keep = $true
                return $alias
            } finally {
                if (-not $keep) {
                    try { if ($created) { [Trinity.NativePathAlias]::RemoveExact($drive, $target) } }
                    finally {
                        if ($held) { $mutex.ReleaseMutex() }
                        $mutex.Dispose()
                    }
                }
            }
        }
    }
    throw 'No free drive letter is available for Trinity. Free one drive letter from D: through Z: and run start.bat again.'
}
function Remove-NativePathAlias {
    param($Alias)
    if ($null -eq $Alias -or $Alias.Removed) { return }
    try {
        if ($Alias.Created) { [Trinity.NativePathAlias]::RemoveExact($Alias.Drive, $Alias.Target) }
        $Alias.Removed = $true
    } finally {
        if ($null -ne $Alias.Mutex) {
            try { $Alias.Mutex.ReleaseMutex() }
            finally { $Alias.Mutex.Dispose(); $Alias.Mutex = $null }
        }
    }
}
function New-NativeProcess([string]$File, [string[]]$Arguments) {
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = $File
    $startInfo.Arguments = ($Arguments | ForEach-Object { Quote-Argument $_ }) -join ' '
    $startInfo.WorkingDirectory = $Root
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $p = New-Object Diagnostics.Process
    $p.StartInfo = $startInfo
    [void]$p.Start()
    return @{ Process = $p; Out = $p.StandardOutput.ReadLineAsync(); Err = $p.StandardError.ReadLineAsync(); Lines = (New-Object 'Collections.Generic.List[string]') }
}
function Read-NativeOutput($Job, [switch]$Capture) {
    foreach ($key in @('Out', 'Err')) {
        $limit = 200
        while ($null -ne $Job[$key] -and $Job[$key].IsCompleted -and $limit-- -gt 0) {
            $line = $Job[$key].GetAwaiter().GetResult()
            if ($null -eq $line) { $Job[$key] = $null; break }
            if ($Capture -and $key -eq 'Out') { $Job.Lines.Add($line) }
            else { Write-Log $line }
            if ($key -eq 'Out') { $Job[$key] = $Job.Process.StandardOutput.ReadLineAsync() }
            else { $Job[$key] = $Job.Process.StandardError.ReadLineAsync() }
        }
    }
}
function Stop-NativeProcess([Diagnostics.Process]$NativeProcess) {
    # Keep the original process handle open while targeting its PID, so it cannot
    # be recycled for an unrelated process. /T also stops cargo/rustup children.
    if ($NativeProcess.HasExited) { return }
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = Join-Path $env:SystemRoot 'System32\taskkill.exe'
    $startInfo.Arguments = '/PID ' + $NativeProcess.Id + ' /T /F'
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $killer = New-Object Diagnostics.Process
    $killer.StartInfo = $startInfo
    try {
        [void]$killer.Start()
        $stdout = $killer.StandardOutput.ReadToEndAsync()
        $stderr = $killer.StandardError.ReadToEndAsync()
        if (-not $killer.WaitForExit(15000)) {
            $killer.Kill()
            Write-Log 'Process-tree shutdown timed out; stopping the owned parent process.'
        } elseif ($killer.ExitCode -ne 0 -and -not $NativeProcess.HasExited) {
            Write-Log ('Process-tree shutdown reported: ' + $stderr.GetAwaiter().GetResult().Trim())
        }
    } catch { Write-Log ('Process-tree shutdown reported: ' + $_.Exception.Message) }
    finally { $killer.Dispose() }
    if (-not $NativeProcess.HasExited) { $NativeProcess.Kill() }
    [void]$NativeProcess.WaitForExit(10000)
}
function Invoke-Native([string]$File, [string[]]$Arguments, [switch]$Capture, [int]$Timeout = 1800) {
    $job = New-NativeProcess $File $Arguments
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        $exitedAt = $null
        while ($true) {
            Assert-Running
            Read-NativeOutput $job -Capture:$Capture
            if ($job.Process.HasExited) {
                if ($null -eq $exitedAt) { $exitedAt = $watch.Elapsed.TotalMilliseconds }
                # Daemons started by pg_ctl can inherit a pipe handle. Never wait
                # for their EOF after the actual command has already completed.
                if (($null -eq $job.Out -and $null -eq $job.Err) -or ($watch.Elapsed.TotalMilliseconds - $exitedAt -gt 250)) { break }
            }
            if ($watch.Elapsed.TotalSeconds -gt $Timeout) { throw "Timed out running $(Split-Path -Leaf $File). See logs/launcher.log." }
            Start-Sleep -Milliseconds 40
        }
        if ($job.Process.ExitCode -ne 0) { throw "$(Split-Path -Leaf $File) exited with code $($job.Process.ExitCode). See logs/launcher.log." }
        if ($Capture) { return ($job.Lines -join "`n").Trim() }
    } finally {
        Stop-NativeProcess $job.Process
        $job.Process.Dispose()
    }
}
function Initialize-DownloadProgressType {
    if ('Trinity.LiveDownload' -as [type]) { return }
    # Progress callbacks run on .NET threads, without a PowerShell runspace.
    # Keep their counters in C# and poll them from the worker's own thread.
    Add-Type -TypeDefinition @'
using System;
using System.Net;
using System.Threading;
using System.Threading.Tasks;
namespace Trinity {
    public sealed class LiveDownload : IDisposable {
        private readonly WebClient client = new WebClient();
        private long received;
        private long total = -1;
        private int percent = -1;
        public long Received { get { return Interlocked.Read(ref received); } }
        public long Total { get { return Interlocked.Read(ref total); } }
        public int Percent { get { return Interlocked.CompareExchange(ref percent, 0, 0); } }
        public LiveDownload() { client.DownloadProgressChanged += OnProgress; }
        private void OnProgress(object sender, DownloadProgressChangedEventArgs e) {
            Interlocked.Exchange(ref received, e.BytesReceived);
            Interlocked.Exchange(ref total, e.TotalBytesToReceive);
            Interlocked.Exchange(ref percent, e.TotalBytesToReceive > 0 ? e.ProgressPercentage : -1);
        }
        public Task Start(string url, string path) { return client.DownloadFileTaskAsync(new Uri(url), path); }
        public void Cancel() { client.CancelAsync(); }
        public void Dispose() { client.Dispose(); }
    }
}
'@
}
function Set-PhaseProgress([string]$Label, [string]$Message, [double]$Percent) {
    $script:Status['downloadPercent'] = $null
    $script:Status['phaseLabel'] = $Label
    $script:Status['phasePercent'] = [Math]::Max(0, [Math]::Min(100, $Percent))
    $script:Status.message = $Message
    [IO.File]::WriteAllText($StatusPath, ($script:Status | ConvertTo-Json -Compress), $Utf8)
}
function Get-FileHashWithProgress([string]$Path, [string]$DisplayName) {
    Write-Log ('Verifying ' + $DisplayName)
    Set-PhaseProgress 'VERIFYING FILE' ('Verifying {0} - 0%' -f $DisplayName) 0
    $stream = [IO.File]::OpenRead($Path)
    $hash = [Security.Cryptography.SHA256]::Create()
    $buffer = New-Object byte[] (1MB)
    $processed = [long]0
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        $total = $stream.Length
        while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            Assert-Running
            [void]$hash.TransformBlock($buffer, 0, $count, $buffer, 0)
            $processed += $count
            if ($watch.ElapsedMilliseconds -ge 150 -or $processed -eq $total) {
                $percent = if ($total -gt 0) { [Math]::Floor(100.0 * $processed / $total) } else { 100 }
                Set-PhaseProgress 'VERIFYING FILE' ('Verifying {0} - {1}% ({2:N1} / {3:N1} MB)' -f $DisplayName,$percent,($processed / 1MB),($total / 1MB)) $percent
                $watch.Restart()
            }
        }
        [void]$hash.TransformFinalBlock([byte[]]@(), 0, 0)
        Set-PhaseProgress 'VERIFYING FILE' ('Verified {0} - 100%' -f $DisplayName) 100
        return ([BitConverter]::ToString($hash.Hash)).Replace('-', '').ToLowerInvariant()
    } finally { $stream.Dispose(); $hash.Dispose() }
}
function Get-Download([string]$Url, [string]$Name, [string]$Sha256) {
    if ([string]::IsNullOrWhiteSpace($Name) -or [IO.Path]::GetFileName($Name) -ne $Name) { throw 'Invalid download filename.' }
    $directory = Join-Path $Runtime 'downloads'
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    $destination = Join-Path $directory $Name
    if ([IO.File]::Exists($destination) -and (Get-FileHashWithProgress $destination $Name) -eq $Sha256) { return $destination }
    Initialize-DownloadProgressType
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $partial = $destination + '.part'
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        Assert-Running
        Write-Log "Downloading $Name (attempt $attempt/3)"
        $client = [Trinity.LiveDownload]::new()
        $script:Status['downloadName'] = $Name
        $script:Status['downloadPercent'] = 0
        $script:Status['phasePercent'] = $null
        $script:Status.message = 'Downloading {0} - 0%' -f $Name
        [IO.File]::WriteAllText($StatusPath, ($script:Status | ConvertTo-Json -Compress), $Utf8)
        try {
            $task = $client.Start($Url, $partial)
            while (-not $task.IsCompleted) {
                Assert-Running
                $received = $client.Received; $total = $client.Total; $percent = $client.Percent
                $script:Status['downloadReceived'] = $received
                $script:Status['downloadTotal'] = $total
                if ($total -gt 0) {
                    $script:Status['downloadPercent'] = [Math]::Max(0, [Math]::Min(100, $percent))
                    $script:Status.message = 'Downloading {0} - {1}% ({2:N1} / {3:N1} MB)' -f $Name,$script:Status.downloadPercent,($received / 1MB),($total / 1MB)
                } else {
                    $script:Status['downloadPercent'] = $null
                    $script:Status.message = 'Downloading {0} - {1:N1} MB received (waiting for total size)' -f $Name,($received / 1MB)
                }
                [IO.File]::WriteAllText($StatusPath, ($script:Status | ConvertTo-Json -Compress), $Utf8)
                Start-Sleep -Milliseconds 250
            }
            # WebClient's runtime Task can return a null completion object.
            # Suppress it so callers receive exactly one string: the file path.
            [void]$task.GetAwaiter().GetResult()
            if ((Get-FileHashWithProgress $partial $Name) -ne $Sha256) { throw "Checksum mismatch for $Name. Download refused." }
            if ([IO.File]::Exists($destination)) { [IO.File]::Replace($partial, $destination, [NullString]::Value) }
            else { [IO.File]::Move($partial, $destination) }
            return [string]$destination
        } catch [OperationCanceledException] { throw }
        catch { if ($attempt -eq 3) { throw }; Write-Log $_.Exception.Message }
        finally { $client.Cancel(); $client.Dispose() }
    }
}
function Expand-PortableZip([string]$Archive, [string]$Destination) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Directory]::CreateDirectory($Destination) | Out-Null
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    $base = [IO.Path]::GetFullPath($Destination).TrimEnd('\') + '\'
    try {
        $entries = @($zip.Entries | Where-Object { $_.FullName -match '^pgsql/(bin|lib|share)/' -and $_.Name -ne '' })
        $completed = 0
        $watch = [Diagnostics.Stopwatch]::StartNew()
        Set-PhaseProgress 'EXTRACTING POSTGRESQL' 'Download verified. Extracting PostgreSQL - 0%' 0
        foreach ($entry in $entries) {
            Assert-Running
            # pgAdmin is not needed and contains paths too long for PowerShell 5.1.
            if ($entry.FullName -notmatch '^pgsql/(bin|lib|share)/') { continue }
            $output = [IO.Path]::GetFullPath((Join-Path $Destination $entry.FullName))
            if (-not $output.StartsWith($base, [StringComparison]::OrdinalIgnoreCase)) { throw 'Archive contains an invalid path.' }
            if ($entry.Name -eq '') { [IO.Directory]::CreateDirectory($output) | Out-Null; continue }
            [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($output)) | Out-Null
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $output, $true)
            $completed++
            if ($watch.ElapsedMilliseconds -ge 150 -or $completed -eq $entries.Count) {
                $percent = [Math]::Floor(100.0 * $completed / $entries.Count)
                Set-PhaseProgress 'EXTRACTING POSTGRESQL' ('Extracting PostgreSQL - {0}% ({1} / {2} files)' -f $percent,$completed,$entries.Count) $percent
                $watch.Restart()
            }
        }
    } finally { $zip.Dispose() }
}
function Test-FreePort([int]$Port) {
    $listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, $Port)
    $listener.ExclusiveAddressUse = $true
    try { $listener.Start(); return $true } catch { return $false } finally { $listener.Stop() }
}
function Test-Listening([int]$Port) {
    $tcp = New-Object Net.Sockets.TcpClient
    try { $task = $tcp.ConnectAsync('127.0.0.1', $Port); return ($task.Wait(200) -and $tcp.Connected) } catch { return $false } finally { $tcp.Dispose() }
}
function Test-OwnedPostgres([string]$DataDirectory, [string]$Executable) {
    $pidFile = Join-Path $DataDirectory 'postmaster.pid'
    if (-not (Test-Path -LiteralPath $pidFile)) { return $false }
    $pidLines = [IO.File]::ReadAllLines($pidFile)
    $postgresId = 0
    $startSeconds = [long]0
    if ($pidLines.Length -lt 3 -or -not [int]::TryParse($pidLines[0], [ref]$postgresId) -or $postgresId -le 0) {
        throw 'Cannot verify PostgreSQL ownership: invalid postmaster.pid. No process was stopped.'
    }
    $pgProcess = Get-Process -Id $postgresId -ErrorAction SilentlyContinue
    if (-not $pgProcess) { return $false } # PostgreSQL can recover its stale PID file.
    try {
        $recordedData = [IO.Path]::GetFullPath($pidLines[1]).TrimEnd('\')
        $expectedData = [IO.Path]::GetFullPath($DataDirectory).TrimEnd('\')
        $expectedExecutable = [IO.Path]::GetFullPath($Executable)
        if (-not [string]::Equals($recordedData, $expectedData, [StringComparison]::OrdinalIgnoreCase) -or
            -not [string]::Equals($pgProcess.Path, $expectedExecutable, [StringComparison]::OrdinalIgnoreCase) -or
            -not [long]::TryParse($pidLines[2], [ref]$startSeconds)) {
            throw 'Cannot verify PostgreSQL ownership: the live PID does not match this database. No process was stopped.'
        }
        $epoch = [DateTime]::SpecifyKind([DateTime]'1970-01-01', [DateTimeKind]::Utc)
        $recordedStart = $epoch.AddSeconds($startSeconds)
        if ([Math]::Abs(($pgProcess.StartTime.ToUniversalTime() - $recordedStart).TotalSeconds) -gt 3) {
            throw 'Cannot verify PostgreSQL ownership: the recorded PID has been reused. No process was stopped.'
        }
        return $true
    } finally { $pgProcess.Dispose() }
}
function Install-PostgresRuntime([string]$Bin) {
    $runtimeDlls = @('concrt140.dll','msvcp140.dll','msvcp140_1.dll','msvcp140_2.dll','msvcp140_atomic_wait.dll','msvcp140_codecvt_ids.dll','vcruntime140.dll','vcruntime140_1.dll')
    if (Test-Path -LiteralPath (Join-Path $Bin 'vclibs-installed.ok')) { return }
    $archive = Get-Download 'https://download.microsoft.com/download/4/7/c/47c6134b-d61f-4024-83bd-b9c9ea951c25/Microsoft.VCLibs.x64.14.00.Desktop.appx' 'Microsoft.VCLibs.x64.14.00.Desktop.appx' 'b56a9101f706f9d95f815f5b7fa6efbac972e86573d378b96a07cff5540c5961'
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        foreach ($name in $runtimeDlls) {
            $entry = $zip.GetEntry($name)
            if (-not $entry) { throw "Microsoft runtime archive is missing $name." }
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $Bin $name), $true)
        }
    } finally { $zip.Dispose() }
    [IO.File]::WriteAllText((Join-Path $Bin 'vclibs-installed.ok'), '14.0.33321.0', $Utf8)
}

function Start-TrinityWorker {
    $lock = $null; $server = $null; $databaseOwned = $false; $exitCode = 0
    $gamePort = 26000; $webPort = 18080; $databasePort = 5432
    $checkoutAlias = $null
    $originalTemp = $env:TEMP; $originalTmp = $env:TMP
    $previousOriginalTemp = $env:TRINITY_ORIGINAL_TEMP; $previousOriginalTmp = $env:TRINITY_ORIGINAL_TMP
    $pgBin = Join-Path $Runtime 'postgres-16.15\pgsql\bin'
    $data = Join-Path $Runtime 'data'
    try {
        try { $lock = [IO.File]::Open((Join-Path $Runtime 'launcher.lock'), 'OpenOrCreate', 'ReadWrite', 'None') }
        catch { Write-Host 'Trinity is already running from this folder. Use its launcher window.'; return 2 }
        # Native PostgreSQL subprocesses and compiler tools receive short ASCII
        # paths even when the checkout lives under Unicode names or a UNC share.
        $checkoutAlias = New-NativePathAlias -Path $SourceRoot
        $script:Root = $checkoutAlias.Root
        $script:Runtime = Join-Path $Root '.trinity'
        $script:Logs = Join-Path $Root 'logs'
        $script:LogPath = Join-Path $Logs 'launcher.log'
        $script:StopPath = Join-Path $Runtime 'stop.request'
        $script:StatusPath = Join-Path $Runtime 'status.json'
        # Keep worker-local values as well: the embedded payload is invoked as a
        # scriptblock, whose parent locals can otherwise shadow script scope.
        $Root = $checkoutAlias.Root
        $Runtime = Join-Path $Root '.trinity'; $Logs = Join-Path $Root 'logs'
        $LogPath = Join-Path $Logs 'launcher.log'
        $StopPath = Join-Path $Runtime 'stop.request'; $StatusPath = Join-Path $Runtime 'status.json'
        $script:Runtime = $Runtime; $script:Logs = $Logs; $script:LogPath = $LogPath
        $script:StopPath = $StopPath; $script:StatusPath = $StatusPath
        Set-Location -LiteralPath $Root
        [Environment]::CurrentDirectory = $Root
        $temporaryDirectory = Join-Path $Runtime 'tmp'
        [IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
        $env:TRINITY_ORIGINAL_TEMP = $originalTemp; $env:TRINITY_ORIGINAL_TMP = $originalTmp
        $env:TEMP = $temporaryDirectory; $env:TMP = $temporaryDirectory
        $pgBin = Join-Path $Runtime 'postgres-16.15\pgsql\bin'
        $data = Join-Path $Runtime 'data'
        if (Test-Path -LiteralPath $StopPath) {
            $request = [IO.File]::ReadAllText($StopPath).Trim()
            $started = [Diagnostics.Process]::GetCurrentProcess().StartTime.ToUniversalTime()
            if ($request -ne $env:TRINITY_RUN_ID -and (Get-Item -LiteralPath $StopPath).LastWriteTimeUtc -lt $started) { Remove-Item -LiteralPath $StopPath }
        }
        [IO.File]::WriteAllText($LogPath, ('Trinity launcher - ' + (Get-Date -Format s) + "`r`n"), $Utf8)
        Set-Status 'Preparing' 'Checking Windows and preparing your local server' 2
        if (-not [Environment]::Is64BitOperatingSystem -or -not [Environment]::Is64BitProcess) { throw 'Trinity requires 64-bit Windows and 64-bit Windows PowerShell.' }
        foreach ($required in @('Cargo.lock','Cargo.toml','trinity.yml','db\00_shared.sql.gz','crates\eve-server\Cargo.toml')) {
            if (-not (Test-Path -LiteralPath (Join-Path $Root $required))) { throw "Missing $required. Keep start.bat in the extracted Trinity repository." }
        }
        $settingsPath = Join-Path $Runtime 'settings.json'
        $savedClientPath = ''
        if (Test-Path -LiteralPath $settingsPath) {
            $savedSettings = Get-Content -Raw -LiteralPath $settingsPath | ConvertFrom-Json
            if ($savedSettings.PSObject.Properties['clientPath']) { $savedClientPath = [string]$savedSettings.clientPath }
        }
        # Retain the selected client only; discard older automatic port overrides.
        $settings = [pscustomobject]@{ clientPath = $savedClientPath }
        [IO.File]::WriteAllText($settingsPath, ($settings | ConvertTo-Json), $Utf8)
        if (-not (Test-FreePort $gamePort)) { throw 'Game port 26000 is already in use. Close the other server before starting Trinity.' }
        if (-not (Test-FreePort $webPort)) { throw 'Trinity dashboard port 18080 is already in use. Close the application using it before starting Trinity.' }
        $script:Status.gamePort = $gamePort; $script:Status.webPort = $webPort

        Set-Status 'Toolchain' 'Preparing portable Rust 1.97 (first run downloads the compiler)' 8
        $env:CARGO_HOME = Join-Path $Runtime 'cargo'
        $env:RUSTUP_HOME = Join-Path $Runtime 'rustup'
        $env:RUSTUP_TOOLCHAIN = '1.97.0-x86_64-pc-windows-gnu'
        $env:CARGO_TARGET_DIR = Join-Path $Root 'target'
        $env:CARGO_BUILD_TARGET = 'x86_64-pc-windows-gnu'
        $env:CARGO_TERM_COLOR = 'never'
        $toolDir = Join-Path $Runtime 'tools'
        [IO.Directory]::CreateDirectory($toolDir) | Out-Null
        $rustup = Join-Path $toolDir 'rustup.exe'
        if (-not (Test-Path -LiteralPath $rustup)) {
            $bootstrap = Get-Download 'https://static.rust-lang.org/rustup/archive/1.28.2/x86_64-pc-windows-gnu/rustup-init.exe' 'rustup-init.exe' 'ccbfd951d8024856043b3a0c3903a59f39937bce8d3074768b0d3da55f21e817'
            if ([string]::IsNullOrWhiteSpace($bootstrap) -or -not [IO.File]::Exists($bootstrap)) { throw 'The Rust download did not return a valid file path. Run start.bat again to retry.' }
            [IO.File]::Copy($bootstrap, $rustup, $true)
        }
        $toolchainReady = Join-Path $Runtime 'rust-toolchain-installed.ok'
        if (-not (Test-Path -LiteralPath $toolchainReady) -or -not (Test-Path -LiteralPath (Join-Path $env:RUSTUP_HOME 'toolchains\1.97.0-x86_64-pc-windows-gnu\bin\rustc.exe'))) {
            # Pin the minimal profile archives from Rust's official 1.97.0 manifest.
            # Prefill rustup's verified cache so every large download uses the live
            # byte-based progress display, even though rustup runs without a console.
            $components = @(
                @{ name = 'rustc'; hash = 'c5b717977488eb02b56fa89c3629a7b7ebdaf1c6aecc3a6897cb924aa128c138' }
                @{ name = 'cargo'; hash = '8ef34ae3c90879860103fdb00a0120940f20f3c77b8647372c54aa3a6de21b3d' }
                @{ name = 'rust-std'; hash = '98e6ef703b003b1f7d75c3249a831f4ac07f0deb41b0e1dfe41054847ac2816f' }
                @{ name = 'rust-mingw'; hash = '5d0bb89c6652df6dd759247c3c5c92d8d681f0fe1d7650bc5db9f31f179844ee' }
            )
            $rustupDownloads = Join-Path $env:RUSTUP_HOME 'downloads'
            [IO.Directory]::CreateDirectory($rustupDownloads) | Out-Null
            $componentIndex = 0
            foreach ($component in $components) {
                Assert-Running
                $componentIndex++
                Set-Status 'Downloading Rust' ("Compiler component {0} of {1}: {2}" -f $componentIndex,$components.Count,$component.name) (8 + 2 * $componentIndex)
                $cachedArchive = Join-Path $rustupDownloads $component.hash
                if ([IO.File]::Exists($cachedArchive) -and (Get-FileHash -LiteralPath $cachedArchive -Algorithm SHA256).Hash -eq $component.hash) { continue }
                $archiveName = $component.name + '-1.97.0-x86_64-pc-windows-gnu.tar.xz'
                $componentArchive = Get-Download ('https://static.rust-lang.org/dist/2026-07-09/' + $archiveName) $archiveName $component.hash
                [IO.File]::Copy($componentArchive, $cachedArchive, $true)
            }
            Set-Status 'Installing Rust' 'Downloads complete. Installing the portable compiler...' 18
            Invoke-Native $rustup @('toolchain','install','1.97.0-x86_64-pc-windows-gnu','--profile','minimal','--no-self-update')
            [IO.File]::WriteAllText($toolchainReady, '1.97.0-x86_64-pc-windows-gnu', $Utf8)
        }
        Set-Status 'PostgreSQL' 'Preparing portable PostgreSQL 16.15' 20
        $pgDir = Join-Path $Runtime 'postgres-16.15'
        if (-not (Test-Path -LiteralPath (Join-Path $pgDir 'installed.ok'))) {
            $archive = Get-Download 'https://get.enterprisedb.com/postgresql/postgresql-16.15-1-windows-x64-binaries.zip' 'postgresql-16.15-1-windows-x64-binaries.zip' '25e6fcdfb8caec38691bf461125e7564508760666f7b8e5dc6a5f0818f58f81e'
            Set-Status 'Extracting PostgreSQL' 'Download verified. Preparing the database tools...' 22
            Expand-PortableZip $archive $pgDir
            [IO.File]::WriteAllText((Join-Path $pgDir 'installed.ok'), '16.15', $Utf8)
        }
        # Install app-local Microsoft runtime DLLs here; no machine-wide installer.
        Install-PostgresRuntime $pgBin
        Invoke-Native (Join-Path $pgBin 'postgres.exe') @('--version')
        Set-Status 'Building' 'Compiling Trinity; later launches reuse the build cache' 35
        Invoke-Native $rustup @('run','1.97.0-x86_64-pc-windows-gnu','cargo','build','--release','--locked','-p','eve-server','--bin','eve-server','--target','x86_64-pc-windows-gnu')

        Set-Status 'Database' 'Preparing your private PostgreSQL database' 50
        $passwordPath = Join-Path $Runtime 'database-password.txt'
        if (-not (Test-Path -LiteralPath $passwordPath)) {
            if (Test-Path -LiteralPath (Join-Path $data 'PG_VERSION')) { throw 'Database password file is missing. Restore .trinity/database-password.txt from your backup.' }
            $random = New-Object byte[] 32
            $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
            try { $rng.GetBytes($random) } finally { $rng.Dispose() }
            [IO.File]::WriteAllText($passwordPath, ([BitConverter]::ToString($random).Replace('-','').ToLowerInvariant()), $Utf8)
        }
        $password = [IO.File]::ReadAllText($passwordPath).Trim()
        # Existing PostgreSQL client/service settings must not redirect or alter
        # the private cluster. These changes affect only this worker and its children.
        foreach ($variable in @(Get-ChildItem Env: | Where-Object { $_.Name -like 'PG*' })) {
            [Environment]::SetEnvironmentVariable($variable.Name, $null, 'Process')
        }
        $env:PGPASSWORD = $password
        $env:PGCLIENTENCODING = 'UTF8'
        $env:PGCONNECT_TIMEOUT = '5'
        if (-not (Test-Path -LiteralPath (Join-Path $data 'PG_VERSION'))) {
            if (Test-Path -LiteralPath $data) { throw 'Incomplete data folder found. Preserve it and move it aside before retrying.' }
            $staging = Join-Path $Runtime ('data-init-' + [Guid]::NewGuid().ToString('N'))
            Invoke-Native (Join-Path $pgBin 'initdb.exe') @('-D',$staging,'-U','trinity','--pwfile',$passwordPath,'--encoding=UTF8','--locale=C','--auth-host=scram-sha-256','--auth-local=scram-sha-256')
            # Both absolute paths are fixed children of this launcher's private runtime.
            [IO.Directory]::Move($staging, $data)
        }
        if ([IO.File]::ReadAllText((Join-Path $data 'PG_VERSION')).Trim() -ne '16') { throw 'This launcher requires a PostgreSQL 16 data directory.' }
        $pgCtl = Join-Path $pgBin 'pg_ctl.exe'
        if (Test-OwnedPostgres $data (Join-Path $pgBin 'postgres.exe')) {
            Invoke-Native $pgCtl @('status','-D',$data)
            $databaseOwned = $true
            Invoke-Native $pgCtl @('stop','-D',$data,'-m','fast','-w','-t','60') -Timeout 90
            $databaseOwned = $false
        }
        if (-not (Test-FreePort $databasePort)) { throw 'PostgreSQL port 5432 is already in use. Close the other database server before starting Trinity.' }
        $databaseOwned = $true
        $pgOptions = '-h 127.0.0.1 -p ' + $databasePort
        Invoke-Native $pgCtl @('start','-D',$data,'-l',(Join-Path $Logs 'postgres.log'),'-o',$pgOptions,'-w','-t','60') -Timeout 90
        $psql = Join-Path $pgBin 'psql.exe'
        $pgArgs = @('-X','-h','127.0.0.1','-p',[string]$databasePort,'-U','trinity','-v','ON_ERROR_STOP=1')
        $exists = Invoke-Native $psql ($pgArgs + @('-d','postgres','-Atc',"SELECT 1 FROM pg_database WHERE datname='trinity'")) -Capture
        if ($exists -ne '1') { Invoke-Native (Join-Path $pgBin 'createdb.exe') @('-h','127.0.0.1','-p',[string]$databasePort,'-U','trinity','-E','UTF8','-T','template0','trinity') }
        $dbArgs = $pgArgs + @('-d','trinity')
        Invoke-Native $psql ($dbArgs + @('-q','-c','CREATE TABLE IF NOT EXISTS public.trinity_launcher_imports (name text PRIMARY KEY, imported_at timestamptz NOT NULL DEFAULT now());'))
        $done = Invoke-Native $psql ($dbArgs + @('-Atc','SELECT name FROM public.trinity_launcher_imports ORDER BY name;')) -Capture
        $completed = @{}
        foreach ($name in ($done -split "`n")) { $completed[$name.Trim()] = $true }
        $dumps = @(Get-ChildItem -LiteralPath (Join-Path $Root 'db') -Filter '*.sql.gz' | Sort-Object -Property Name)
        if ($dumps.Count -lt 1) { throw 'No database dumps found.' }
        $index = 0
        foreach ($dump in $dumps) {
            Assert-Running
            $index++
            if ($completed.ContainsKey($dump.Name)) { continue }
            Set-Status 'Importing universe' ("{0}/{1} - {2}" -f $index,$dumps.Count,$dump.Name) (50 + [int](35 * $index / $dumps.Count))
            $sqlPath = Join-Path $Runtime 'import.sql'
            $inputFile = [IO.File]::OpenRead($dump.FullName)
            $gzip = New-Object IO.Compression.GzipStream($inputFile, [IO.Compression.CompressionMode]::Decompress)
            $outputFile = [IO.File]::Create($sqlPath)
            try { $gzip.CopyTo($outputFile) } finally { $outputFile.Dispose(); $gzip.Dispose(); $inputFile.Dispose() }
            $safeName = $dump.Name.Replace("'", "''")
            [IO.File]::AppendAllText($sqlPath, "`nINSERT INTO public.trinity_launcher_imports(name) VALUES ('$safeName');`n", $Utf8)
            # Each dump and its checkpoint commit together, so interrupted setup resumes safely.
            Invoke-Native $psql ($dbArgs + @('-q','-o','NUL','--single-transaction','-f',$sqlPath)) -Timeout 900
        }
        $sqlPath = Join-Path $Runtime 'import.sql'
        if (Test-Path -LiteralPath $sqlPath) { Remove-Item -LiteralPath $sqlPath }

        Set-Status 'Client' 'Finding and preparing the Trinity game client' 87
        Prepare-TrinityClient $settings

        Set-Status 'Starting' 'Loading the universe and starting the game server' 92
        $env:RMR_CONFIG = Join-Path $Root 'trinity.yml'
        $env:RMR_STRINGS = Join-Path $Root 'crates\eve-marshal\data\strings.txt'
        $env:RMR_DATABASE = 'postgres://trinity:' + $password + '@127.0.0.1:' + $databasePort + '/trinity'
        $env:RMR_BIND = '127.0.0.1:' + $gamePort
        $env:RMR_WEB = '127.0.0.1:' + $webPort
        $env:RMR_LOG_FILE = Join-Path $Logs 'server.log'
        $env:RMR_DUMP_DIR = Join-Path $Logs 'dumps'
        [IO.Directory]::CreateDirectory($env:RMR_DUMP_DIR) | Out-Null
        $binaryDir = Join-Path $Root 'target\x86_64-pc-windows-gnu\release'
        $server = New-NativeProcess (Join-Path $binaryDir 'eve-server.exe') @()
        $watch = [Diagnostics.Stopwatch]::StartNew()
        while (-not (Test-Listening $gamePort) -or -not (Test-Listening $webPort)) {
            Assert-Running; Read-NativeOutput $server
            if ($server.Process.HasExited) { Read-NativeOutput $server; throw "Server exited with code $($server.Process.ExitCode). See logs/launcher.log." }
            if ($watch.Elapsed.TotalSeconds -gt 180) { throw 'Server startup timed out. See logs/server.log.' }
            Start-Sleep -Milliseconds 200
        }
        Assert-Running
        if ($server.Process.HasExited) { throw "The server stopped before setup finished. See logs/server.log." }
        [IO.File]::WriteAllText((Join-Path $Runtime 'setup-complete'), (Get-Date -Format s), $Utf8)
        $script:Status.ready = $true
        Set-Status 'Online' ("Client: 127.0.0.1:{0}   |   Dashboard: http://127.0.0.1:{1}" -f $gamePort,$webPort) 100
        Assert-Running
        Start-TrinityClient -ClientPath $script:Status.clientPath
        while (-not $server.Process.HasExited) { Assert-Running; Read-NativeOutput $server; Start-Sleep -Milliseconds 100 }
        Read-NativeOutput $server
        throw "Server exited unexpectedly with code $($server.Process.ExitCode). See logs/server.log."
    } catch [OperationCanceledException] { Write-Log $_.Exception.Message }
    catch {
        $exitCode = 1
        $script:Status.error = $true
        Set-Status 'Needs attention' $_.Exception.Message $script:Status.progress
        Write-Log $_.ScriptStackTrace
    } finally {
        if ($lock) {
            try {
                $script:Stopping = $true
                $script:Status.ready = $false
                if ($server) {
                    Stop-NativeProcess $server.Process
                    Read-NativeOutput $server
                    $server.Process.Dispose()
                }
                if ($databaseOwned) {
                    Write-Log 'Stopping this launcher''s PostgreSQL database...'
                    try {
                        if (Test-OwnedPostgres $data (Join-Path $pgBin 'postgres.exe')) {
                            Invoke-Native (Join-Path $pgBin 'pg_ctl.exe') @('stop','-D',$data,'-m','fast','-w','-t','60') -Timeout 90
                        }
                        $databaseOwned = $false
                    }
                    catch { $exitCode = 1; $script:Status.error = $true; Set-Status 'Needs attention' ('Database shutdown failed: ' + $_.Exception.Message) 100 }
                }
                $env:PGPASSWORD = $null; $env:RMR_DATABASE = $null
                $script:Status.stopped = $true
                if (-not $script:Status.error) { Set-Status 'Stopped' 'Server stopped. Your universe is saved for the next launch.' 100 }
                else { [IO.File]::WriteAllText($StatusPath, ($script:Status | ConvertTo-Json -Compress), $Utf8) }
            } finally {
                # Keep the path available if PostgreSQL could not be stopped.
                # A later launcher can reuse it and recover the private cluster.
                if ($databaseOwned -and $checkoutAlias) { $checkoutAlias.Created = $false }
                try {
                    try {
                        Set-Location -LiteralPath $SourceRoot
                        [Environment]::CurrentDirectory = $SourceRoot
                    } catch {
                        Set-Location -LiteralPath $env:SystemRoot
                        [Environment]::CurrentDirectory = $env:SystemRoot
                    }
                    $env:TEMP = $originalTemp; $env:TMP = $originalTmp
                    $env:TRINITY_ORIGINAL_TEMP = $previousOriginalTemp; $env:TRINITY_ORIGINAL_TMP = $previousOriginalTmp
                    # UI status uses the original files; remove only the exact
                    # mapping we created, after all native tools have finished.
                    Remove-NativePathAlias $checkoutAlias
                } catch {
                    $exitCode = 1
                    Write-Host ('Temporary drive cleanup needs attention: ' + $_.Exception.Message)
                } finally {
                    $Root = $SourceRoot
                    $Runtime = Join-Path $Root '.trinity'; $Logs = Join-Path $Root 'logs'
                    $LogPath = Join-Path $Logs 'launcher.log'
                    $StopPath = Join-Path $Runtime 'stop.request'; $StatusPath = Join-Path $Runtime 'status.json'
                    $script:Root = $SourceRoot
                    $script:Runtime = Join-Path $Root '.trinity'
                    $script:Logs = Join-Path $Root 'logs'
                    $script:LogPath = Join-Path $Logs 'launcher.log'
                    $script:StopPath = Join-Path $Runtime 'stop.request'
                    $script:StatusPath = Join-Path $Runtime 'status.json'
                    $lock.Dispose()
                }
            }
        }
    }
    return $exitCode
}

# Embedded in start.bat. Trinity 4.10 build 45017 only.
function Get-TrinityClientInfo {
    param([string]$ClientPath)
    if ([string]::IsNullOrWhiteSpace($ClientPath)) { return $null }
    try {
        $path = [IO.Path]::GetFullPath($ClientPath.Trim().Trim('"'))
        if ([IO.File]::Exists($path)) { $path = [IO.Path]::GetDirectoryName($path) }
        if ([IO.Path]::GetFileName($path) -eq 'bin') { $path = [IO.Path]::GetDirectoryName($path) }
        foreach ($name in @('common.ini', 'boot.ini', 'eve.exe', 'bin\ExeFile.exe', 'bin\blue.dll', 'script\compiled.code')) {
            if (-not [IO.File]::Exists((Join-Path $path $name))) { return $null }
        }
        $common = [IO.File]::ReadAllText((Join-Path $path 'common.ini'))
        if ($common -notmatch '(?im)^\s*version\s*=\s*4\.10\s*$' -or
            $common -notmatch '(?im)^\s*build\s*=\s*45017\s*$') { return $null }
        return [pscustomobject]@{ Path = $path; Build = '45017'; Version = '4.10' }
    } catch { return $null }
}

function Find-TrinityClient {
    param([string]$Root, [string]$SavedPath)
    $saved = Get-TrinityClientInfo $SavedPath
    if ($saved) { return $saved.Path }
    # Search only the checkout, its parent and nearby grandchildren. Never crawl
    # the entire disk, follow junctions, or descend through resources/caches.
    $parent = [IO.Directory]::GetParent($Root)
    $starts = @($Root, (Join-Path $Root 'Client'))
    if ($parent) { $starts += @((Join-Path $parent.FullName 'Client'), $parent.FullName) }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $queue = [Collections.Generic.Queue[object]]::new()
    foreach ($start in $starts) { $queue.Enqueue(@($start, 0)) }
    $visited = 0
    while ($queue.Count -gt 0 -and $visited -lt 300) {
        $entry = $queue.Dequeue(); $path = [string]$entry[0]; $depth = [int]$entry[1]
        if (-not $seen.Add($path) -or -not [IO.Directory]::Exists($path)) { continue }
        $visited++
        $info = Get-TrinityClientInfo $path
        if ($info) { return $info.Path }
        if ($depth -ge 2) { continue }
        foreach ($directory in @(Get-ChildItem -LiteralPath $path -Directory -ErrorAction SilentlyContinue | Sort-Object Name)) {
            if ($directory.Name -match '^(\.|target$|node_modules$|res$|cache$|bulkdata$|logs$|lib$|bin$|script$)' -or
                ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint)) { continue }
            $queue.Enqueue(@($directory.FullName, ($depth + 1)))
        }
    }
    return $null
}

function Get-TrinityBytesHash {
    param([byte[]]$Bytes)
    $hash = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($hash.ComputeHash($Bytes))).Replace('-', '').ToLowerInvariant() }
    finally { $hash.Dispose() }
}

function Write-TrinityClientFile {
    param([string]$Path, [byte[]]$Bytes, [string]$BackupDirectory)
    $existing = [IO.File]::ReadAllBytes($Path)
    $beforeHash = Get-TrinityBytesHash $existing
    if ($beforeHash -eq (Get-TrinityBytesHash $Bytes)) { return }
    # Originals are immutable. A pre-existing different backup gets a hash-named
    # sibling, rather than being overwritten by a later setup run.
    $backup = $Path + '.trinity-original'
    if ($BackupDirectory) {
        [IO.Directory]::CreateDirectory($BackupDirectory) | Out-Null
        $backup = Join-Path $BackupDirectory ([IO.Path]::GetFileName($Path) + '.trinity-original')
    }
    if ([IO.File]::Exists($backup) -and (Get-TrinityBytesHash ([IO.File]::ReadAllBytes($backup))) -ne $beforeHash) {
        $backup += '-' + $beforeHash.Substring(0, 12)
    }
    if (-not [IO.File]::Exists($backup)) { [IO.File]::WriteAllBytes($backup, $existing) }
    $temporary = $Path + '.trinity-pending'
    [IO.File]::WriteAllBytes($temporary, $Bytes)
    try { [IO.File]::Replace($temporary, $Path, [NullString]::Value) }
    catch {
        if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) }
        throw "Cannot update $Path. Close the EVE client and keep it in a writable folder. $($_.Exception.Message)"
    }
}

function Set-TrinityLocalPreferences {
    param([string]$ClientPath)
    # Trinity 45017 predates the 1.1 change to root/settings. /LUA:OFF maps
    # this build's settings to root/cache/Settings. Never edit shared AppData.
    $directory = Join-Path $ClientPath 'cache\Settings'
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    $path = Join-Path $directory 'prefs.ini'
    $text = if ([IO.File]::Exists($path)) { [IO.File]::ReadAllText($path) } else { '' }
    # The legacy IniFile parser ignores section headers. Replace every duplicate
    # of our endpoint keys, including keys outside [main], retaining other prefs.
    $lines = [Collections.Generic.List[string]]::new()
    foreach ($line in ($text -split '\r?\n')) {
        if ($line -notmatch '^\s*(port|server)\s*=') { $lines.Add($line) }
    }
    $lines.Add('server=127.0.0.1'); $lines.Add('port=26000')
    $bytes = [Text.Encoding]::UTF8.GetBytes((($lines -join "`r`n").Trim("`r", "`n")) + "`r`n")
    if ([IO.File]::Exists($path)) { Write-TrinityClientFile $path $bytes }
    else { [IO.File]::WriteAllBytes($path, $bytes) }
}

function Set-TrinityIniValues {
    param([string]$Path, [System.Collections.IDictionary]$Values)
    $text = [IO.File]::ReadAllText($Path)
    $newline = if ($text.Contains("`r`n")) { "`r`n" } else { "`n" }
    $lines = [Collections.Generic.List[string]]::new()
    $seen = @{}; $inMain = $false; $hadMain = $false
    foreach ($line in ($text -split '\r?\n')) {
        if ($line -match '^\s*\[([^\]]+)\]\s*$') {
            if ($inMain) {
                foreach ($key in $Values.Keys) { if (-not $seen[$key]) { $lines.Add("$key=$($Values[$key])"); $seen[$key] = $true } }
            }
            $inMain = $Matches[1] -eq 'main'
            if ($inMain) { $hadMain = $true }
        }
        if ($inMain -and $line -match '^\s*([^=;#]+?)\s*=') {
            $key = $Matches[1].Trim()
            if ($Values.Contains($key)) {
                if (-not $seen[$key]) { $lines.Add("$key=$($Values[$key])"); $seen[$key] = $true }
                continue
            }
        }
        $lines.Add($line)
    }
    if (-not $hadMain) { $lines.Add('[main]') }
    foreach ($key in $Values.Keys) { if (-not $seen[$key]) { $lines.Add("$key=$($Values[$key])") } }
    $updated = ($lines -join $newline).TrimEnd("`r", "`n") + $newline
    Write-TrinityClientFile -Path $Path -Bytes ([Text.Encoding]::UTF8.GetBytes($updated))
}

function Initialize-TrinityClient {
    param([string]$ClientPath, [string]$ScratchPath)
    $info = Get-TrinityClientInfo $ClientPath
    if (-not $info) { throw 'Select the EVE Trinity 4.10 build 45017 client folder containing eve.exe.' }
    $ClientPath = $info.Path
    $bluePath = Join-Path $ClientPath 'bin\blue.dll'
    $bytes = [IO.File]::ReadAllBytes($bluePath)
    $sourceHash = '416f8d8b1e3fd1edcd67651fab73564b3fe9e63a360da0d6a54bc4d266858230'
    $targetHash = 'fc2ad683d9c2f0e7fb9260d986818b8be885f689ab2925b7f3d7795dd423426b'
    $actualHash = Get-TrinityBytesHash $bytes
    if ($actualHash -ne $sourceHash -and $actualHash -ne $targetHash) {
        throw "This client's blue.dll is not the verified Trinity 45017 original or patch. No client files were changed. SHA256: $actualHash"
    }

    # 45017's file-verification success branch, independently checked against
    # CryptVerifySignatureA / CryptDestroyHash and the 'bad signature for file'
    # failure block. The established blue_patcher uses this NOP+JMP technique:
    # https://github.com/stschake/blue_patcher/blob/master/Source/blue_patcher/Patcher.cs
    # Exact whole-file hashes guard this recipe; other client builds are refused.
    if ($actualHash -eq $sourceHash) {
        if ($bytes.Length -ne 849152 -or $bytes[0x25ca0] -ne 0x0f -or $bytes[0x25ca1] -ne 0x85) {
            throw 'The Trinity patch precondition failed. No client files were changed.'
        }
        $bytes[0x25ca0] = 0x90; $bytes[0x25ca1] = 0xe9
        if ((Get-TrinityBytesHash $bytes) -ne $targetHash) { throw 'Trinity patch verification failed.' }
    }

    # The 2007 client ships its own signed D3DX cabinet inside this redistributable.
    # Extract the one DLL beside ExeFile.exe; do not execute/install the redistributable.
    # Blue relocates files absent from its bin whitelist. Keep app-local runtime
    # DLLs outside bin and expose this folder only in the client's process PATH.
    $clientRuntime = Join-Path $ClientPath '.trinity-runtime'
    $directX = Join-Path $clientRuntime 'd3dx9_35.dll'
    $directXHash = '2198022938156b790e9cfb0f7997494b66a11a1ad49b395be58251d635b66b26'
    if (-not [IO.File]::Exists($directX)) {
        $redistPath = Join-Path $ClientPath 'bin\RedistD3DXOnly.exe'
        if (-not [IO.File]::Exists($redistPath)) { throw 'This client is incomplete: bin\RedistD3DXOnly.exe is missing.' }
        $redist = [IO.File]::ReadAllBytes($redistPath)
        if ((Get-TrinityBytesHash $redist) -ne '1e1c90972653d2673ddc14e2c63cef8b19600ad31f27a54b4a7220de57a543ef') {
            throw 'The bundled Trinity DirectX redistributable is not the verified 45017 file.'
        }
        [IO.Directory]::CreateDirectory($ScratchPath) | Out-Null
        $cabinet = Join-Path $ScratchPath 'd3dx9_35_x86.cab'
        $cabBytes = [byte[]]::new(1702192)
        [Array]::Copy($redist, 0x82aa, $cabBytes, 0, $cabBytes.Length)
        if ((Get-TrinityBytesHash $cabBytes) -ne '2f6133f583d4979a892280dedddc25581ebec5f801dbccc3174dce42023b6c25') { throw 'DirectX cabinet verification failed.' }
        [IO.File]::WriteAllBytes($cabinet, $cabBytes)
        $output = & "$env:SystemRoot\System32\expand.exe" '-F:d3dx9_35.dll' $cabinet $ScratchPath 2>&1
        if ($LASTEXITCODE -ne 0) { throw "Unable to extract the client DirectX library: $output" }
        $extracted = [IO.File]::ReadAllBytes((Join-Path $ScratchPath 'd3dx9_35.dll'))
        if ((Get-TrinityBytesHash $extracted) -ne $directXHash) { throw 'DirectX library verification failed.' }
        [IO.Directory]::CreateDirectory($clientRuntime) | Out-Null
        [IO.File]::WriteAllBytes($directX, $extracted)
    }
    foreach ($runtimeFile in @('Microsoft.VC80.CRT.manifest', 'msvcr80.dll', 'msvcp80.dll')) {
        if (-not [IO.File]::Exists((Join-Path $ClientPath "bin\Microsoft.VC80.CRT\$runtimeFile"))) {
            throw "This client is incomplete: bin\Microsoft.VC80.CRT\$runtimeFile is missing."
        }
    }
    $backupDirectory = Join-Path $ClientPath '.trinity-backups\bin'
    # Reconstruct the exact original if an older launcher left its backup in bin
    # and Blue relocated it. Both the patched and reconstructed hashes are fixed.
    $originalBlue = [byte[]]$bytes.Clone()
    $originalBlue[0x25ca0] = 0x0f; $originalBlue[0x25ca1] = 0x85
    if ((Get-TrinityBytesHash $originalBlue) -ne $sourceHash) { throw 'Original client backup verification failed.' }
    [IO.Directory]::CreateDirectory($backupDirectory) | Out-Null
    $blueBackup = Join-Path $backupDirectory 'blue.dll.trinity-original'
    if (-not [IO.File]::Exists($blueBackup)) { [IO.File]::WriteAllBytes($blueBackup, $originalBlue) }
    elseif ((Get-TrinityBytesHash ([IO.File]::ReadAllBytes($blueBackup))) -ne $sourceHash) { throw 'The preserved original blue.dll has changed. Restore it before continuing.' }
    Write-TrinityClientFile -Path $bluePath -Bytes $bytes -BackupDirectory $backupDirectory
    Set-TrinityIniValues -Path (Join-Path $ClientPath 'common.ini') -Values ([ordered]@{ cryptoPack = 'Placebo' })
    Set-TrinityIniValues -Path (Join-Path $ClientPath 'boot.ini') -Values ([ordered]@{ server = '127.0.0.1'; port = '26000' })
    Set-TrinityLocalPreferences -ClientPath $ClientPath
    return [pscustomobject]@{ Path = $ClientPath; Build = '45017'; BlueHash = $targetHash }
}

function Start-TrinityClient {
    param([string]$ClientPath)
    # Keep this function self-contained: UI callbacks invoke a captured scriptblock.
    $exe = Join-Path $ClientPath 'bin\ExeFile.exe'
    if (-not [IO.File]::Exists($exe)) { throw 'The selected Trinity client is no longer present.' }
    # boot.ini and isolated prefs carry the port. /LUA:OFF prevents another EVE
    # install's shared legacy prefs.ini (often port 26000) from overriding it.
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $exe; $start.WorkingDirectory = $ClientPath
    $start.Arguments = '/LUA:OFF /server:127.0.0.1'
    $start.UseShellExecute = $false
    $start.EnvironmentVariables['PATH'] = (Join-Path $ClientPath '.trinity-runtime') + ';' + $env:PATH
    # The game may stay open after the server's temporary drive is released.
    if ($env:TRINITY_ORIGINAL_TEMP) { $start.EnvironmentVariables['TEMP'] = $env:TRINITY_ORIGINAL_TEMP }
    if ($env:TRINITY_ORIGINAL_TMP) { $start.EnvironmentVariables['TMP'] = $env:TRINITY_ORIGINAL_TMP }
    [Diagnostics.Process]::Start($start) | Out-Null
}

function Prepare-TrinityClient($Settings) {
    $saved = if ($Settings.PSObject.Properties['clientPath']) { [string]$Settings.clientPath } else { '' }
    # Search beside the actual checkout, not the temporary native-tool drive.
    $clientPath = Find-TrinityClient -Root $SourceRoot -SavedPath $saved
    while (-not $clientPath) {
        Assert-Running
        if ($env:TRINITY_MODE -eq 'worker') {
            $selection = Join-Path $Runtime 'client-selection.txt'
            if (Test-Path -LiteralPath $selection) { Remove-Item -LiteralPath $selection }
            $script:Status['needsClient'] = $true
            $script:Status['clientRequestId'] = [Guid]::NewGuid().ToString('N')
            Set-Status 'Choose your client' 'Select EVE Trinity 4.10 build 45017, the folder containing eve.exe.' 87
            while (-not (Test-Path -LiteralPath $selection)) { Assert-Running; Start-Sleep -Milliseconds 200 }
            $selected = [IO.File]::ReadAllText($selection).Trim()
            $script:Status['needsClient'] = $false
            if (-not $selected) { throw [OperationCanceledException]::new('Client selection cancelled. Run start.bat when your client is ready.') }
        } else {
            $selected = Read-Host 'Path to EVE Trinity 4.10 build 45017 (folder containing eve.exe)'
            if (-not $selected) { throw [OperationCanceledException]::new('Client selection cancelled.') }
        }
        $info = Get-TrinityClientInfo $selected
        if ($info) { $clientPath = $info.Path }
        else { Write-Log 'That folder is not a complete EVE Trinity 4.10 build 45017 client. Please select another folder.' }
    }
    Set-Status 'Preparing client' 'Backing up, verifying and configuring your Trinity client' 89
    $prepared = Initialize-TrinityClient -ClientPath $clientPath -ScratchPath (Join-Path $Runtime 'client-runtime')
    $Settings | Add-Member -NotePropertyName clientPath -NotePropertyValue $prepared.Path -Force
    [IO.File]::WriteAllText((Join-Path $Runtime 'settings.json'), ($Settings | ConvertTo-Json), $Utf8)
    $script:Status['clientPath'] = $prepared.Path
    Write-Log ('Client ready: ' + $prepared.Path + ' (Trinity 45017; verified blue.dll; 127.0.0.1:26000)')
}

function Start-LauncherUi {
    # Copy these into the function scope so event-handler closures retain them.
    $Root = [string]$Root
    $Runtime = [string]$Runtime
    $Logs = [string]$Logs
    $Self = [string]$Self
    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -AssemblyName System.Drawing
    [System.Windows.Forms.Application]::EnableVisualStyles()
    if (-not ('Trinity.LauncherSkyForm' -as [type])) {
        Add-Type -ReferencedAssemblies @('System.dll', 'System.Windows.Forms.dll', 'System.Drawing.dll') -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Windows.Forms;

namespace Trinity {
    internal static class LauncherSky {
        public static void Paint(Graphics graphics, Rectangle bounds, double time, int seed, int count) {
            if (bounds.Width < 1 || bounds.Height < 1) return;
            graphics.SmoothingMode = SmoothingMode.AntiAlias;
            using (var background = new LinearGradientBrush(bounds,
                Color.FromArgb(8, 15, 27), Color.FromArgb(12, 23, 37), 28f))
                graphics.FillRectangle(background, bounds);
            var random = new Random(seed);
            for (int i = 0; i < count; i++) {
                float x = (float)random.NextDouble() * bounds.Width;
                float y = (float)random.NextDouble() * bounds.Height;
                double phase = random.NextDouble() * Math.PI * 2;
                double speed = 0.35 + random.NextDouble() * 0.45;
                float radius = i % 11 == 0 ? 1.5f : 0.7f;
                int alpha = (int)(35 + 105 * (0.5 + 0.5 * Math.Sin(time * speed + phase)));
                using (var star = new SolidBrush(Color.FromArgb(alpha, 165, 216, 230)))
                    graphics.FillEllipse(star, x, y, radius * 2, radius * 2);
                if (i % 19 == 0) {
                    using (var halo = new SolidBrush(Color.FromArgb(alpha / 7, 100, 220, 225)))
                        graphics.FillEllipse(halo, x - 3, y - 3, 8, 8);
                }
            }
        }
    }

    public sealed class LauncherSkyForm : Form {
        private readonly Timer animation;
        private readonly DateTime started = DateTime.UtcNow;
        public LauncherSkyForm() {
            SetStyle(ControlStyles.AllPaintingInWmPaint | ControlStyles.OptimizedDoubleBuffer |
                     ControlStyles.ResizeRedraw, true);
            animation = new Timer();
            animation.Interval = 100;
            animation.Tick += delegate { if (Visible && WindowState != FormWindowState.Minimized) Invalidate(); };
            animation.Start();
        }
        protected override void OnPaintBackground(PaintEventArgs e) {
            LauncherSky.Paint(e.Graphics, ClientRectangle, (DateTime.UtcNow - started).TotalSeconds, 45017, 140);
        }
        protected override void Dispose(bool disposing) {
            if (disposing) { animation.Stop(); animation.Dispose(); }
            base.Dispose(disposing);
        }
    }

    public sealed class LauncherSkyHeader : Panel {
        private readonly Timer animation;
        private readonly DateTime started = DateTime.UtcNow;
        public LauncherSkyHeader() {
            SetStyle(ControlStyles.UserPaint | ControlStyles.AllPaintingInWmPaint |
                     ControlStyles.OptimizedDoubleBuffer | ControlStyles.ResizeRedraw, true);
            animation = new Timer();
            animation.Interval = 100;
            animation.Tick += delegate { if (Visible) Invalidate(true); };
            animation.Start();
        }
        protected override void OnPaintBackground(PaintEventArgs e) {
            LauncherSky.Paint(e.Graphics, ClientRectangle, (DateTime.UtcNow - started).TotalSeconds, 182, 48);
        }
        protected override void Dispose(bool disposing) {
            if (disposing) { animation.Stop(); animation.Dispose(); }
            base.Dispose(disposing);
        }
    }

    public sealed class LauncherProgress : Control {
        private float value;
        public float Value {
            get { return value; }
            set { this.value = Math.Max(0, Math.Min(100, value)); Invalidate(); }
        }
        public LauncherProgress() {
            SetStyle(ControlStyles.UserPaint | ControlStyles.AllPaintingInWmPaint |
                     ControlStyles.OptimizedDoubleBuffer | ControlStyles.ResizeRedraw, true);
            BackColor = Color.FromArgb(18, 29, 43);
            Height = 10;
        }
        protected override void OnPaint(PaintEventArgs e) {
            if (Width < 1 || Height < 1) return;
            e.Graphics.SmoothingMode = SmoothingMode.AntiAlias;
            using (var track = new SolidBrush(Color.FromArgb(35, 51, 67)))
                e.Graphics.FillRectangle(track, ClientRectangle);
            float fill = Width * value / 100f;
            if (fill > 0) {
                using (var accent = new LinearGradientBrush(ClientRectangle,
                    Color.FromArgb(40, 151, 170), Color.FromArgb(112, 237, 213), 0f))
                    e.Graphics.FillRectangle(accent, 0, 0, fill, Height);
                using (var edge = new Pen(Color.FromArgb(120, 209, 255, 246)))
                    e.Graphics.DrawLine(edge, 0, 0, fill, 0);
            }
        }
    }
}
'@
    }

    $firstRun = -not [IO.File]::Exists((Join-Path $Runtime 'setup-complete'))
    $background = [System.Drawing.Color]::FromArgb(8, 15, 27)
    $panelColor = [System.Drawing.Color]::FromArgb(18, 29, 43)
    $muted = [System.Drawing.Color]::FromArgb(161, 180, 198)
    $bright = [System.Drawing.Color]::FromArgb(236, 244, 250)
    $cyan = [System.Drawing.Color]::FromArgb(112, 237, 213)
    $font = New-Object System.Drawing.Font('Segoe UI', 12.5)
    $form = New-Object Trinity.LauncherSkyForm
    $form.Text = 'Trinity | Preparing your universe'
    $form.StartPosition = 'CenterScreen'
    $workArea = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
    $form.ClientSize = New-Object System.Drawing.Size(([Math]::Min(1240, $workArea.Width - 64)), ([Math]::Min(900, $workArea.Height - 88)))
    $form.MinimumSize = New-Object System.Drawing.Size(([Math]::Min(1040, $workArea.Width - 64)), ([Math]::Min(770, $workArea.Height - 64)))
    $form.AutoScaleMode = 'Dpi'
    $form.BackColor = $background
    $form.ForeColor = $bright
    $form.Font = $font
    $form.Padding = New-Object System.Windows.Forms.Padding(28)

    # Small displays can scroll the complete layout instead of clipping controls.
    $viewport = New-Object System.Windows.Forms.Panel
    $viewport.Dock = 'Fill'
    $viewport.AutoScroll = $true
    $viewport.AutoScrollMinSize = New-Object System.Drawing.Size(930, 730)
    $viewport.BackColor = [System.Drawing.Color]::Transparent
    $form.Controls.Add($viewport)
    $layout = New-Object System.Windows.Forms.TableLayoutPanel
    $layout.Dock = 'Fill'
    $layout.MinimumSize = New-Object System.Drawing.Size(930, 730)
    $layout.ColumnCount = 1
    $layout.RowCount = 6
    $layout.BackColor = [System.Drawing.Color]::Transparent
    $layout.ColumnStyles.Add((New-Object System.Windows.Forms.ColumnStyle('Percent', 100))) | Out-Null
    foreach ($height in @(112, 250, 42)) {
        $layout.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Absolute', $height))) | Out-Null
    }
    $layout.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Percent', 100))) | Out-Null
    $layout.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Absolute', 78))) | Out-Null
    $layout.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Absolute', 36))) | Out-Null
    $viewport.Controls.Add($layout)

    $header = New-Object Trinity.LauncherSkyHeader
    $header.Dock = 'Fill'
    $header.Margin = New-Object System.Windows.Forms.Padding(0, 0, 0, 12)
    $brand = New-Object System.Windows.Forms.Label
    $brand.Text = 'T R I N I T Y'
    $brand.Font = New-Object System.Drawing.Font('Segoe UI Light', 34)
    $brand.ForeColor = $bright
    $brand.BackColor = [System.Drawing.Color]::Transparent
    $brand.AutoSize = $true
    $brand.Location = New-Object System.Drawing.Point(0, 1)
    $subtitle = New-Object System.Windows.Forms.Label
    $subtitle.Text = 'YOUR OWN CORNER OF NEW EDEN'
    $subtitle.Font = New-Object System.Drawing.Font('Segoe UI', 11)
    $subtitle.ForeColor = $muted
    $subtitle.BackColor = [System.Drawing.Color]::Transparent
    $subtitle.AutoSize = $true
    $subtitle.Location = New-Object System.Drawing.Point(4, 68)
    $editionLabel = New-Object System.Windows.Forms.Label
    $editionLabel.Text = 'TRINITY 4.10  /  45017'
    $editionLabel.Font = New-Object System.Drawing.Font('Segoe UI', 11)
    $editionLabel.ForeColor = $cyan
    $editionLabel.BackColor = [System.Drawing.Color]::Transparent
    $editionLabel.TextAlign = 'MiddleRight'
    $editionLabel.Dock = 'Right'
    $editionLabel.Width = 265
    $header.Controls.AddRange(@($brand, $subtitle, $editionLabel))
    $layout.Controls.Add($header, 0, 0)

    $statusPanel = New-Object System.Windows.Forms.TableLayoutPanel
    $statusPanel.Dock = 'Fill'
    $statusPanel.BackColor = $panelColor
    $statusPanel.Padding = New-Object System.Windows.Forms.Padding(24, 18, 24, 16)
    $statusPanel.Margin = New-Object System.Windows.Forms.Padding(0)
    $statusPanel.RowCount = 6
    $statusPanel.ColumnCount = 1
    $statusPanel.ColumnStyles.Add((New-Object System.Windows.Forms.ColumnStyle('Percent', 100))) | Out-Null
    foreach ($height in @(24, 45, 57, 33, 10)) {
        $statusPanel.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Absolute', $height))) | Out-Null
    }
    $statusPanel.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Percent', 100))) | Out-Null
    $statusEyebrow = New-Object System.Windows.Forms.Label
    $statusEyebrow.Text = if ($firstRun) { 'FIRST LAUNCH' } else { 'WELCOME BACK' }
    $statusEyebrow.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 10.5)
    $statusEyebrow.ForeColor = $cyan
    $statusEyebrow.Dock = 'Fill'
    $statusEyebrow.Margin = New-Object System.Windows.Forms.Padding(0)
    $stageLabel = New-Object System.Windows.Forms.Label
    $stageLabel.Text = 'Preparing your universe'
    $stageLabel.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 23)
    $stageLabel.Dock = 'Fill'
    $stageLabel.AutoEllipsis = $true
    $stageLabel.Margin = New-Object System.Windows.Forms.Padding(0)
    $messageLabel = New-Object System.Windows.Forms.Label
    $messageLabel.Text = if ($firstRun) { 'We will download the tools, build your server and prepare its database automatically.' } else { 'Checking your saved setup and getting everything ready for launch.' }
    $messageLabel.ForeColor = $muted
    $messageLabel.Dock = 'Fill'
    $messageLabel.AutoEllipsis = $true
    $messageLabel.Margin = New-Object System.Windows.Forms.Padding(0, 5, 0, 0)
    $progressHeading = New-Object System.Windows.Forms.TableLayoutPanel
    $progressHeading.Dock = 'Fill'
    $progressHeading.ColumnCount = 2
    $progressHeading.RowCount = 1
    $progressHeading.ColumnStyles.Add((New-Object System.Windows.Forms.ColumnStyle('Percent', 100))) | Out-Null
    $progressHeading.ColumnStyles.Add((New-Object System.Windows.Forms.ColumnStyle('Absolute', 180))) | Out-Null
    $progressHeading.Margin = New-Object System.Windows.Forms.Padding(0)
    $progressCaption = New-Object System.Windows.Forms.Label
    $progressCaption.Text = 'OVERALL SETUP'
    $progressCaption.Dock = 'Fill'
    $progressCaption.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 10.5)
    $progressCaption.ForeColor = $muted
    $progressCaption.TextAlign = 'MiddleLeft'
    $progressCaption.AutoEllipsis = $true
    $progressCaption.Margin = New-Object System.Windows.Forms.Padding(0)
    $progressPercent = New-Object System.Windows.Forms.Label
    $progressPercent.Text = '0%'
    $progressPercent.Dock = 'Fill'
    $progressPercent.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 15)
    $progressPercent.ForeColor = $cyan
    $progressPercent.TextAlign = 'MiddleRight'
    $progressPercent.Margin = New-Object System.Windows.Forms.Padding(0)
    $progressHeading.Controls.Add($progressCaption, 0, 0)
    $progressHeading.Controls.Add($progressPercent, 1, 0)
    $progressTrack = New-Object Trinity.LauncherProgress
    $progressTrack.Dock = 'Fill'
    $progressTrack.Margin = New-Object System.Windows.Forms.Padding(0)
    $setupHint = New-Object System.Windows.Forms.Label
    $setupHint.Text = if ($firstRun) { 'First setup can take a few minutes while tools download and the database is prepared.' } else { 'Your saved universe and installed tools will be reused.' }
    $setupHint.Font = New-Object System.Drawing.Font('Segoe UI', 11.5)
    $setupHint.ForeColor = $muted
    $setupHint.Dock = 'Fill'
    $setupHint.Margin = New-Object System.Windows.Forms.Padding(0, 12, 0, 0)
    $setupHint.AutoEllipsis = $true
    $statusPanel.Controls.Add($statusEyebrow, 0, 0)
    $statusPanel.Controls.Add($stageLabel, 0, 1)
    $statusPanel.Controls.Add($messageLabel, 0, 2)
    $statusPanel.Controls.Add($progressHeading, 0, 3)
    $statusPanel.Controls.Add($progressTrack, 0, 4)
    $statusPanel.Controls.Add($setupHint, 0, 5)
    $layout.Controls.Add($statusPanel, 0, 1)

    $logHeading = New-Object System.Windows.Forms.Label
    $logHeading.Dock = 'Fill'
    $logHeading.Text = 'ACTIVITY  /  LIVE SERVER OUTPUT'
    $logHeading.ForeColor = $muted
    $logHeading.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 10.5)
    $logHeading.BackColor = [System.Drawing.Color]::Transparent
    $logHeading.TextAlign = 'MiddleLeft'
    $logHeading.Margin = New-Object System.Windows.Forms.Padding(0)
    $layout.Controls.Add($logHeading, 0, 2)

    $consoleFrame = New-Object System.Windows.Forms.Panel
    $consoleFrame.Dock = 'Fill'
    $consoleFrame.BackColor = [System.Drawing.Color]::FromArgb(8, 15, 24)
    $consoleFrame.Padding = New-Object System.Windows.Forms.Padding(16, 12, 16, 12)
    $consoleFrame.Margin = New-Object System.Windows.Forms.Padding(0)
    $consoleFrame.MinimumSize = New-Object System.Drawing.Size(0, 155)
    $console = New-Object System.Windows.Forms.RichTextBox
    $console.Dock = 'Fill'
    $console.BackColor = $consoleFrame.BackColor
    $console.ForeColor = [System.Drawing.Color]::FromArgb(188, 209, 222)
    $console.Font = New-Object System.Drawing.Font('Consolas', 11.5)
    $console.BorderStyle = 'None'
    $console.ReadOnly = $true
    $console.DetectUrls = $false
    $console.WordWrap = $true
    $console.HideSelection = $false
    $console.Margin = New-Object System.Windows.Forms.Padding(0)
    $console.Text = "Welcome to your universe.`r`nPreparing Trinity...`r`n"
    $consoleFrame.Controls.Add($console)
    $layout.Controls.Add($consoleFrame, 0, 3)

    $footer = New-Object System.Windows.Forms.TableLayoutPanel
    $footer.Dock = 'Fill'
    $footer.Margin = New-Object System.Windows.Forms.Padding(0, 20, 0, 0)
    $footer.BackColor = [System.Drawing.Color]::Transparent
    $footer.ColumnCount = 4
    $footer.RowCount = 1
    foreach ($width in @(30, 25, 20, 25)) {
        $footer.ColumnStyles.Add((New-Object System.Windows.Forms.ColumnStyle('Percent', $width))) | Out-Null
    }
    $connectionLabel = New-Object System.Windows.Forms.Label
    $connectionLabel.Dock = 'Fill'
    $connectionLabel.ForeColor = $muted
    $connectionLabel.TextAlign = 'MiddleLeft'
    $connectionLabel.BackColor = [System.Drawing.Color]::Transparent
    $connectionLabel.Font = New-Object System.Drawing.Font('Segoe UI', 11)
    $connectionLabel.Text = 'LOCAL UNIVERSE  /  Setup and game data stay on your computer'
    $connectionLabel.AutoEllipsis = $true
    $connectionLabel.Margin = New-Object System.Windows.Forms.Padding(0)

    $clientButton = New-Object System.Windows.Forms.Button
    $dashboardButton = New-Object System.Windows.Forms.Button
    $logsButton = New-Object System.Windows.Forms.Button
    $stopButton = New-Object System.Windows.Forms.Button
    foreach ($button in @($clientButton, $dashboardButton, $logsButton, $stopButton)) {
        $button.Dock = 'Fill'
        $button.FlatStyle = 'Flat'
        $button.FlatAppearance.BorderSize = 1
        $button.FlatAppearance.BorderColor = [System.Drawing.Color]::FromArgb(43, 65, 81)
        $button.FlatAppearance.MouseOverBackColor = [System.Drawing.Color]::FromArgb(31, 49, 65)
        $button.FlatAppearance.MouseDownBackColor = [System.Drawing.Color]::FromArgb(39, 65, 81)
        $button.BackColor = $panelColor
        $button.ForeColor = $bright
        $button.Cursor = [System.Windows.Forms.Cursors]::Hand
        $button.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 12.5)
        $button.Margin = New-Object System.Windows.Forms.Padding(0, 0, 12, 0)
        $button.UseVisualStyleBackColor = $false
    }
    $clientButton.Text = 'Preparing...'
    $clientButton.Enabled = $false
    $clientButton.BackColor = [System.Drawing.Color]::FromArgb(36, 43, 52)
    $clientButton.ForeColor = [System.Drawing.Color]::FromArgb(137, 148, 161)
    $clientButton.FlatAppearance.BorderSize = 1
    $clientButton.FlatAppearance.MouseOverBackColor = [System.Drawing.Color]::FromArgb(150, 248, 229)
    $clientButton.FlatAppearance.MouseDownBackColor = [System.Drawing.Color]::FromArgb(79, 202, 182)
    $dashboardButton.Text = 'Preparing...'
    $dashboardButton.Enabled = $false
    $dashboardButton.BackColor = [System.Drawing.Color]::FromArgb(36, 43, 52)
    $dashboardButton.ForeColor = [System.Drawing.Color]::FromArgb(137, 148, 161)
    $logsButton.Text = 'Open logs'
    $stopButton.Text = 'Stop server'
    $stopButton.ForeColor = [System.Drawing.Color]::FromArgb(255, 174, 156)
    $stopButton.Margin = New-Object System.Windows.Forms.Padding(0)
    $footer.Controls.Add($clientButton, 0, 0)
    $footer.Controls.Add($dashboardButton, 1, 0)
    $footer.Controls.Add($logsButton, 2, 0)
    $footer.Controls.Add($stopButton, 3, 0)
    $layout.Controls.Add($footer, 0, 4)
    $layout.Controls.Add($connectionLabel, 0, 5)
    $tooltip = New-Object System.Windows.Forms.ToolTip
    $launchClient = $null
    if (Get-Command Start-TrinityClient -ErrorAction SilentlyContinue) {
        $launchClient = ${function:Start-TrinityClient}
    }

    $state = @{
        Process = $null
        Ready = $false
        StopRequested = $false
        CloseRequested = $false
        Exited = $false
        LastLogLength = -1L
        LastLogWrite = [DateTime]::MinValue
        LastStatus = ''
        WebPort = 18080
        ClientPath = ''
        PickerOpen = $false
        LastClientRequest = ''
        FirstRun = $firstRun
        Started = [DateTime]::UtcNow
        RunId = $env:TRINITY_RUN_ID
        StopPath = (Join-Path $Runtime 'stop.request')
        StatusPath = (Join-Path $Runtime 'status.json')
        LogPath = (Join-Path $Logs 'launcher.log')
    }

    $setReadyState = {
        param([bool]$Ready, [string]$DisabledText = 'Preparing...')
        $state.Ready = $Ready -and -not $state.StopRequested -and -not $state.Exited
        $clientButton.Enabled = $state.Ready -and [bool]$state.ClientPath
        $dashboardButton.Enabled = $state.Ready
        if ($state.Ready) {
            $clientButton.Text = 'Launch EVE'
            $clientButton.BackColor = $cyan
            $clientButton.ForeColor = $background
            $clientButton.FlatAppearance.BorderSize = 0
            $clientButton.Cursor = [System.Windows.Forms.Cursors]::Hand
            $dashboardButton.Text = 'Dashboard'
            $dashboardButton.BackColor = $panelColor
            $dashboardButton.ForeColor = $bright
            $dashboardButton.Cursor = [System.Windows.Forms.Cursors]::Hand
        } else {
            foreach ($button in @($clientButton, $dashboardButton)) {
                $button.Text = $DisabledText
                $button.BackColor = [System.Drawing.Color]::FromArgb(36, 43, 52)
                $button.ForeColor = [System.Drawing.Color]::FromArgb(137, 148, 161)
                $button.FlatAppearance.BorderSize = 1
                $button.Cursor = [System.Windows.Forms.Cursors]::Default
            }
        }
    }.GetNewClosure()

    $requestStop = {
        if ($state.StopRequested -or $state.Exited) { return }
        & $setReadyState $false 'Stopping...'
        try {
            [System.IO.File]::WriteAllText($state.StopPath, $state.RunId, [System.Text.Encoding]::ASCII)
            $state.StopRequested = $true
            $stageLabel.Text = 'Stopping Trinity'
            $form.Text = 'Trinity | Stopping'
            $statusEyebrow.Text = 'SAFE SHUTDOWN'
            $messageLabel.Text = 'Finishing the current operation and shutting down the database. Please keep this window open.'
            $setupHint.Text = 'Your universe will be kept for the next launch.'
            $stopButton.Enabled = $false
            $stopButton.Text = 'Stopping...'
            $dashboardButton.Enabled = $false
            $clientButton.Enabled = $false
        } catch {
            $messageLabel.Text = 'Could not request shutdown: ' + $_.Exception.Message
        }
    }.GetNewClosure()

    $clientButton.Add_Click({
        if (-not $state.Ready -or $state.StopRequested -or $state.Exited) { return }
        if ($null -eq $state.Process -or $state.Process.HasExited) { & $setReadyState $false 'Server offline'; return }
        try {
            if ($null -eq $launchClient) { throw 'The client launcher is unavailable. Restart start.bat.' }
            & $launchClient -ClientPath $state.ClientPath | Out-Null
        } catch { $messageLabel.Text = 'Could not launch EVE: ' + $_.Exception.Message }
    }.GetNewClosure())
    $dashboardButton.Add_Click({
        if (-not $state.Ready -or $state.StopRequested -or $state.Exited) { return }
        if ($null -eq $state.Process -or $state.Process.HasExited) { & $setReadyState $false 'Server offline'; return }
        try { Start-Process ('http://127.0.0.1:{0}/' -f $state.WebPort) | Out-Null }
        catch { $messageLabel.Text = 'Could not open your browser: ' + $_.Exception.Message }
    }.GetNewClosure())
    $logsButton.Add_Click({
        try { Start-Process explorer.exe -ArgumentList ('"{0}"' -f $Logs) | Out-Null }
        catch { $messageLabel.Text = 'Logs are saved in ' + $Logs }
    }.GetNewClosure())
    $stopButton.Add_Click({
        if ($state.Exited) { $form.Close() } else { & $requestStop }
    }.GetNewClosure())
    $form.Add_FormClosing({
        param($sender, $eventArgs)
        if (-not $state.Exited -and $null -ne $state.Process) {
            $eventArgs.Cancel = $true
            $state.CloseRequested = $true
            & $requestStop
        }
    }.GetNewClosure())

    $timer = New-Object System.Windows.Forms.Timer
    $timer.Interval = 250
    $timer.Add_Tick({
        # Open with sharing: worker output and atomic status updates must never block the UI.
        try {
            if ([System.IO.File]::Exists($state.StatusPath)) {
                $stream = New-Object System.IO.FileStream($state.StatusPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
                try {
                    $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8)
                    try { $raw = $reader.ReadToEnd() } finally { $reader.Dispose() }
                } finally { $stream.Dispose() }
                if ($raw -and $raw -ne $state.LastStatus) {
                    $status = $raw | ConvertFrom-Json
                    if (-not $status.PSObject.Properties['runId'] -or [string]$status.runId -ne [string]$state.RunId) {
                        & $setReadyState $false
                        $raw = $null
                    }
                }
                if ($raw -and $raw -ne $state.LastStatus) {
                    $state.LastStatus = $raw
                    if (-not $state.StopRequested) {
                        if ($status.stage) { $stageLabel.Text = [string]$status.stage }
                        if ($status.message) { $messageLabel.Text = [string]$status.message }
                    }
                    $overall = [Math]::Max(0, [Math]::Min(100, [double]$status.progress))
                    $downloadActive = $status.PSObject.Properties['downloadPercent'] -and
                        $null -ne $status.downloadPercent -and [double]$status.downloadPercent -ge 0 -and
                        -not $status.ready -and -not $status.error -and -not $state.StopRequested
                    $phaseActive = $status.PSObject.Properties['phasePercent'] -and
                        $null -ne $status.phasePercent -and [double]$status.phasePercent -ge 0 -and
                        -not $status.ready -and -not $status.error -and -not $state.StopRequested
                    if ($downloadActive) {
                        $filePercent = [Math]::Max(0, [Math]::Min(100, [double]$status.downloadPercent))
                        $progressTrack.Value = [single]$filePercent
                        $progressCaption.Text = 'FILE DOWNLOAD   /   OVERALL SETUP ' + ('{0:0}%' -f $overall)
                        $progressPercent.Text = '{0:0}%' -f $filePercent
                    } elseif ($phaseActive) {
                        $phasePercent = [Math]::Max(0, [Math]::Min(100, [double]$status.phasePercent))
                        $phaseTitle = 'PREPARING'
                        if ($status.PSObject.Properties['phaseLabel'] -and $status.phaseLabel) { $phaseTitle = [string]$status.phaseLabel }
                        $progressTrack.Value = [single]$phasePercent
                        $progressCaption.Text = $phaseTitle + '   /   OVERALL SETUP ' + ('{0:0}%' -f $overall)
                        $progressPercent.Text = '{0:0}%' -f $phasePercent
                    } else {
                        $progressTrack.Value = [single]$overall
                        $progressCaption.Text = 'OVERALL SETUP'
                        $progressPercent.Text = '{0:0}%' -f $overall
                    }
                    if ($status.message) { $tooltip.SetToolTip($messageLabel, [string]$status.message) }
                    if ($status.webPort) { $state.WebPort = [int]$status.webPort }
                    if ($status.PSObject.Properties['clientPath'] -and $status.clientPath) {
                        $state.ClientPath = [string]$status.clientPath
                        $tooltip.SetToolTip($clientButton, $state.ClientPath)
                        $tooltip.SetToolTip($connectionLabel, $state.ClientPath)
                    }
                    $verifiedReady = [bool]$status.ready -and -not [bool]$status.error -and -not [bool]$status.stopped -and
                        -not $state.StopRequested -and -not $state.Exited -and [bool]$state.ClientPath -and
                        $null -ne $state.Process -and -not $state.Process.HasExited
                    $disabledText = if ($state.StopRequested) { 'Stopping...' } elseif ($status.error -or $status.stopped) { 'Server offline' } else { 'Preparing...' }
                    & $setReadyState $verifiedReady $disabledText
                    if ($state.Ready) {
                        $stageLabel.ForeColor = $cyan
                        $statusEyebrow.Text = 'SETUP COMPLETE'
                        $stageLabel.Text = 'READY TO LAUNCH'
                        $messageLabel.Text = 'All setup is complete. Your server is online. Click Launch EVE below to play.'
                        $tooltip.SetToolTip($messageLabel, $messageLabel.Text)
                        $progressCaption.Text = 'READY TO LAUNCH'
                        $progressPercent.Text = 'READY'
                        $progressTrack.Value = 100
                        $setupHint.Text = 'No more downloads or setup steps remain.'
                        $connectionLabel.Text = 'ONLINE  /  127.0.0.1:' + [string]$status.gamePort
                        if ($state.ClientPath) { $connectionLabel.Text += '    |    Client: ' + [IO.Path]::GetFileName($state.ClientPath.TrimEnd('\')) }
                        $form.Text = 'Trinity | READY TO LAUNCH'
                    }
                    if ($status.error) {
                        & $setReadyState $false 'Server offline'
                        $form.Text = 'Trinity | Needs attention'
                        $statusEyebrow.Text = 'A LITTLE ATTENTION NEEDED'
                        $stageLabel.ForeColor = [System.Drawing.Color]::FromArgb(255, 151, 130)
                        $messageLabel.Text = [string]$status.message
                        $setupHint.Text = 'The activity log below has the details. Your saved data is kept.'
                        $connectionLabel.Text = 'Review the log below for details'
                    }
                    if ($status.PSObject.Properties['needsClient'] -and $status.needsClient -and -not $state.PickerOpen -and -not $state.StopRequested) {
                        $requestId = 'initial'
                        if ($status.PSObject.Properties['clientRequestId']) { $requestId = [string]$status.clientRequestId }
                        if ($requestId -ne $state.LastClientRequest) {
                            $state.LastClientRequest = $requestId
                            $state.PickerOpen = $true
                            $picker = New-Object System.Windows.Forms.FolderBrowserDialog
                            try {
                                $picker.Description = 'Select EVE Trinity 4.10 (build 45017) folder containing eve.exe'
                                $picker.ShowNewFolderButton = $false
                                $picker.SelectedPath = $Root
                                $chosenPath = ''
                                if ($picker.ShowDialog($form) -eq [System.Windows.Forms.DialogResult]::OK) {
                                    $chosenPath = $picker.SelectedPath
                                }
                                $choicePath = Join-Path $Runtime 'client-selection.txt'
                                [System.IO.File]::WriteAllText($choicePath, $chosenPath, (New-Object System.Text.UTF8Encoding($false)))
                            } finally {
                                $picker.Dispose()
                                $state.PickerOpen = $false
                            }
                        }
                    }
                }
            }
        } catch { } # A partially written status is retried on the next tick.

        try {
            if ([System.IO.File]::Exists($state.LogPath)) {
                $fileInfo = New-Object System.IO.FileInfo($state.LogPath)
                if ($fileInfo.Length -ne $state.LastLogLength -or $fileInfo.LastWriteTimeUtc -ne $state.LastLogWrite) {
                    $stream = New-Object System.IO.FileStream($state.LogPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
                    try {
                        $offset = [Math]::Max(0L, $stream.Length - 65536L)
                        $stream.Position = $offset
                        $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8)
                        try {
                            if ($offset -gt 0) { $null = $reader.ReadLine() }
                            $tail = $reader.ReadToEnd()
                        } finally { $reader.Dispose() }
                    } finally { $stream.Dispose() }
                    $console.Text = $tail
                    $console.SelectionStart = $console.TextLength
                    $console.ScrollToCaret()
                    $state.LastLogLength = $fileInfo.Length
                    $state.LastLogWrite = $fileInfo.LastWriteTimeUtc
                }
            }
        } catch { }

        if ($null -ne $state.Process -and -not $state.Exited) {
            $state.Process.Refresh()
            if ($state.Process.HasExited) {
                $state.Exited = $true
                & $setReadyState $false 'Server offline'
                $dashboardButton.Enabled = $false
                $clientButton.Enabled = $false
                $stopButton.Enabled = $true
                $stopButton.Text = 'Close'
                $connectionLabel.Text = 'OFFLINE  /  Your universe is saved'
                $form.Text = 'Trinity | Stopped'
                $statusEyebrow.Text = 'UNTIL NEXT TIME'
                $progressCaption.Text = 'SESSION FINISHED'
                $progressPercent.Text = 'OFFLINE'
                $setupHint.Text = 'Close this window when you are ready.'
                if ($state.Process.ExitCode -eq 0) {
                    $stageLabel.Text = 'Trinity stopped'
                    $messageLabel.Text = 'Your database is kept for next time. Run start.bat whenever you are ready.'
                } else {
                    $statusEyebrow.Text = 'A LITTLE ATTENTION NEEDED'
                    $stageLabel.Text = 'Trinity needs attention'
                    $stageLabel.ForeColor = [System.Drawing.Color]::FromArgb(255, 151, 130)
                    if (-not $state.LastStatus) { $messageLabel.Text = 'Setup exited unexpectedly. Open the logs for details, then run start.bat again.' }
                }
                if ($state.CloseRequested) { $form.Close() }
            }
        }
    }.GetNewClosure())

    $form.Add_Shown({
        try {
            # Bring the launcher forward once; later client launches may take focus.
            $form.BringToFront()
            $form.Activate()
            $null = [System.IO.Directory]::CreateDirectory($Runtime)
            $null = [System.IO.Directory]::CreateDirectory($Logs)
            $lockPath = Join-Path $Runtime 'launcher.lock'
            try {
                $probe = [System.IO.File]::Open($lockPath, [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
                $probe.Dispose()
            } catch {
                throw 'Another Trinity launcher is already running from this folder. Use its window to manage the server.'
            }

            # The worker owns stale-state cleanup after obtaining its exclusive lock.
            # Environment variables avoid injecting a checkout path into executable code.
            $bootstrap = '[AppContext]::SetSwitch("Switch.System.IO.UseLegacyPathHandling",$false); [AppContext]::SetSwitch("Switch.System.IO.BlockLongPaths",$false); $self = $env:TRINITY_LAUNCHER; $source = [IO.File]::ReadAllText($self); $parts = $source -split ''(?m)^#==TRINITY_POWERSHELL==\r?$'', 2; if ($parts.Count -ne 2) { throw "Launcher payload is missing" }; & ([ScriptBlock]::Create($parts[1]))'
            $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($bootstrap))
            $oldMode = $env:TRINITY_MODE
            $oldLauncher = $env:TRINITY_LAUNCHER
            $oldUiPid = $env:TRINITY_UI_PID
            try {
                $env:TRINITY_MODE = 'worker'
                $env:TRINITY_LAUNCHER = $Self
                $env:TRINITY_UI_PID = [string]$PID
                $powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
                $state.Process = Start-Process -FilePath $powershell -ArgumentList @('-NoLogo', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded) -WorkingDirectory $Root -WindowStyle Hidden -PassThru
            } finally {
                $env:TRINITY_MODE = $oldMode
                $env:TRINITY_LAUNCHER = $oldLauncher
                $env:TRINITY_UI_PID = $oldUiPid
            }
            $timer.Start()
        } catch {
            $state.Exited = $true
            & $setReadyState $false 'Server offline'
            $statusEyebrow.Text = 'A LITTLE ATTENTION NEEDED'
            $stageLabel.Text = 'Unable to start Trinity'
            $stageLabel.ForeColor = [System.Drawing.Color]::FromArgb(255, 151, 130)
            $messageLabel.Text = $_.Exception.Message
            $console.Text = $_.Exception.ToString()
            $stopButton.Text = 'Close'
            $connectionLabel.Text = 'OFFLINE'
        }
    }.GetNewClosure())

    try { [System.Windows.Forms.Application]::Run($form) }
    finally {
        $timer.Stop()
        $timer.Dispose()
        $tooltip.Dispose()
        if ($null -ne $state.Process) { $state.Process.Dispose() }
        $form.Dispose()
        $font.Dispose()
    }
}

if ($env:TRINITY_MODE -eq 'stop') {
    [IO.File]::WriteAllText($StopPath, 'stop', $Utf8)
    Write-Host 'Stop requested. The launcher will save and close its database.'
    exit 0
}
if ($env:TRINITY_MODE -eq 'ui') {
    try { Start-LauncherUi }
    catch {
        Add-Type -AssemblyName System.Windows.Forms
        [Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Trinity could not start', 'OK', 'Error') | Out-Null
    }
} else {
    $result = Start-TrinityWorker
    exit $result
}
