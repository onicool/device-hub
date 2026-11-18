<#
.SYNOPSIS
  Logicool G HUB を完全に削除し、Device HUB を常駐 (ログイン時自動起動) に置き換える。

.DESCRIPTION
  1. (任意) G HUB の設定 DB をデスクトップへ退避
  2. G HUB のプロセス・更新サービスを停止
  3. 公式アンインストーラー (lghub_updater.exe --uninstall) を実行
  4. 残りのフォルダ・自動起動エントリ・サービスを掃除
  5. Device HUB を %LOCALAPPDATA%\DeviceHub\ に配置し、ログイン時に --background で自動起動

  実行前に Device HUB で PRO Wireless が認識され、DPI などが書き込めることを確認しておくこと。
  (マウスの設定は本体のオンボードメモリに残るので、G HUB を消しても DPI 等はそのまま。)

.PARAMETER DeviceHubExe
  配置する device-hub.exe (既定: リポジトリの target\release\device-hub.exe)。

.PARAMETER SkipBackup
  G HUB 設定の退避をしない。

.PARAMETER Yes
  確認を省略する。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\remove-ghub.ps1
#>
[CmdletBinding()]
param(
    [string]$DeviceHubExe = (Join-Path $PSScriptRoot '..\target\release\device-hub.exe'),
    [switch]$SkipBackup,
    [switch]$Yes,
    # 昇格前のユーザーのフォルダ (自己昇格時に内部で渡す)
    [string]$UserLocalAppData = $env:LOCALAPPDATA,
    [string]$UserAppData = $env:APPDATA
)

$ErrorActionPreference = 'Continue'

# ---- 管理者で再起動 (Program Files とサービスの削除に必要)
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Write-Host '管理者権限で再実行します…' -ForegroundColor Yellow
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"",
        '-DeviceHubExe', "`"$DeviceHubExe`"",
        '-UserLocalAppData', "`"$env:LOCALAPPDATA`"", '-UserAppData', "`"$env:APPDATA`"")
    if ($SkipBackup) { $argList += '-SkipBackup' }
    if ($Yes) { $argList += '-Yes' }
    Start-Process powershell -Verb RunAs -ArgumentList $argList
    exit
}

# 昇格後も「元のユーザー」のプロファイルを使う (別アカウントで昇格した場合に備える)
$LocalAppData = $UserLocalAppData
$AppData = $UserAppData

function Confirm-Step([string]$msg) {
    if ($Yes) { return $true }
    $r = Read-Host "$msg [y/N]"
    return $r -match '^(y|yes)$'
}

Write-Host '=== Logicool G HUB の削除と Device HUB への置き換え ===' -ForegroundColor Cyan

$exe = Resolve-Path $DeviceHubExe -ErrorAction SilentlyContinue
if (-not $exe) {
    Write-Host "device-hub.exe が見つかりません: $DeviceHubExe" -ForegroundColor Red
    Write-Host '先に `cargo build --release -p device-hub` でビルドしてください。'
    exit 1
}
if (-not (Confirm-Step 'Device HUB で PRO Wireless の設定ができることを確認済みですか? 続行すると G HUB を削除します')) {
    exit 0
}

# ---- 1. 設定の退避
$ghubLocal = Join-Path $LocalAppData 'LGHUB'
if (-not $SkipBackup -and (Test-Path $ghubLocal)) {
    $dest = Join-Path ([Environment]::GetFolderPath('Desktop')) ("LGHUB-backup-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
    New-Item -ItemType Directory -Path $dest -Force | Out-Null
    Get-ChildItem $ghubLocal -Filter 'settings*' -ErrorAction SilentlyContinue |
        Copy-Item -Destination $dest -Force
    Write-Host "G HUB の設定を退避しました: $dest"
}

# ---- 2. プロセスとサービスの停止
$procs = 'lghub', 'lghub_agent', 'lghub_updater', 'lghub_system_tray', 'lghub_software_manager', 'lghub_crashpad_handler'
foreach ($p in $procs) {
    Get-Process -Name $p -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
}
$svc = Get-Service -Name 'LGHUBUpdaterService' -ErrorAction SilentlyContinue
if ($svc) {
    Stop-Service $svc -Force -ErrorAction SilentlyContinue
    Set-Service $svc -StartupType Disabled -ErrorAction SilentlyContinue
}
Write-Host 'G HUB のプロセスとサービスを停止しました'

# ---- 3. 公式アンインストーラー
$ghubDir = Join-Path $env:ProgramFiles 'LGHUB'
$uninst = Join-Path $ghubDir 'lghub_updater.exe'
if (Test-Path $uninst) {
    Write-Host '公式アンインストーラーを起動します (画面の指示に従ってください)…'
    Start-Process $uninst -ArgumentList '--uninstall' -Wait
    Start-Sleep -Seconds 3
    foreach ($p in $procs) {
        Get-Process -Name $p -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    }
} else {
    Write-Host '公式アンインストーラーが見つかりません (既に削除済み?) — 残りを掃除します'
}

# ---- 4. 残りの掃除
if ($svc) { & sc.exe delete LGHUBUpdaterService | Out-Null }
$leftovers = @(
    $ghubDir,
    (Join-Path $env:ProgramData 'LGHUB'),
    $ghubLocal,
    (Join-Path $AppData 'LGHUB'),
    (Join-Path $AppData 'G HUB')
)
foreach ($d in $leftovers) {
    if (Test-Path $d) {
        Remove-Item $d -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path $d) { Write-Host "  削除できませんでした (再起動後に再実行): $d" -ForegroundColor Yellow }
        else { Write-Host "  削除: $d" }
    }
}
foreach ($root in 'HKCU:', 'HKLM:') {
    $run = "$root\Software\Microsoft\Windows\CurrentVersion\Run"
    $props = Get-ItemProperty $run -ErrorAction SilentlyContinue
    if ($props) {
        $props.PSObject.Properties | Where-Object { $_.Name -match 'LGHUB|G HUB' } | ForEach-Object {
            Remove-ItemProperty -Path $run -Name $_.Name -ErrorAction SilentlyContinue
            Write-Host "  自動起動を削除: $root ... $($_.Name)"
        }
    }
}
# スタートメニューのショートカット
Get-ChildItem "$env:ProgramData\Microsoft\Windows\Start Menu\Programs" -Filter 'Logitech G HUB*' -Recurse -ErrorAction SilentlyContinue |
    Remove-Item -Force -Recurse -ErrorAction SilentlyContinue

# G HUB が入れた仮想デバイスドライバ (残っていれば一覧を出し、確認して削除)
$drivers = & pnputil.exe /enum-drivers 2>$null | Out-String
$blocks = $drivers -split "(\r?\n){2,}" | Where-Object { $_ -match 'Logitech' -and $_ -match 'logi_(joy|generic|virtual)|lghub' }
foreach ($b in $blocks) {
    if ($b -match 'Published Name\s*:\s*(oem\d+\.inf)|公開名\s*:\s*(oem\d+\.inf)') {
        $inf = if ($Matches[1]) { $Matches[1] } else { $Matches[2] }
        Write-Host "G HUB のドライバが残っています:`n$b"
        if (Confirm-Step "  $inf を削除しますか?") {
            & pnputil.exe /delete-driver $inf /uninstall /force
        }
    }
}
Write-Host 'G HUB の削除が完了しました' -ForegroundColor Green

# ---- 5. Device HUB を常駐させる
$installDir = Join-Path $LocalAppData 'DeviceHub'
New-Item -ItemType Directory -Path $installDir -Force | Out-Null
Get-Process -Name 'device-hub' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
$target = Join-Path $installDir 'device-hub.exe'
Copy-Item $exe $target -Force
# 昇格した PowerShell の HKCU は同じユーザーなら同一。Device HUB の設定画面の
# 「Windows ログイン時に起動」と同じ値を書く。
$run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
Set-ItemProperty -Path $run -Name 'DeviceHub' -Value "`"$target`" --background"
Write-Host "Device HUB をログイン時に自動起動するよう登録しました: $target"

# 今すぐ常駐起動 (管理者ではなく通常権限で起動するため explorer 経由)
Start-Process explorer.exe -ArgumentList "`"$target`""
Write-Host '完了。タスクトレイの Device HUB アイコンから設定を開けます。' -ForegroundColor Green
Write-Host '(マウスを G HUB 時代と同じ動きにするには、Device HUB で DPI/ボタンを設定してください)'
