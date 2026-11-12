[CmdletBinding()]
param(
    [ValidateSet('all', 'x64', 'i386', 'arm64')]
    [string]$Arch = 'all',
    [string]$Version,
    [string]$RustVersion = '1.99.1',
    [string]$WixVersion = '7.0.1',
    [switch]$AcceptWixEula
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Invoke-Checked {
    param([string]$Executable, [string[]]$Arguments)
    & $Executable @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Command failed ($LASTEXITCODE): $Executable $($Arguments -join ' ')" }
}

function Get-PeMachine {
    param([string]$Path)
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $reader = [System.IO.BinaryReader]::new($stream)
        try {
            if ($reader.ReadUInt16() -ne 0x5A4D) { throw "Invalid MZ header: $Path" }
            $stream.Position = 0x3C
            $peOffset = $reader.ReadInt32()
            if ($peOffset -lt 64 -or $peOffset -gt ($stream.Length - 6)) { throw "Invalid PE offset: $Path" }
            $stream.Position = $peOffset
            if ($reader.ReadUInt32() -ne 0x00004550) { throw "Invalid PE signature: $Path" }
            return $reader.ReadUInt16()
        } finally { $reader.Dispose() }
    } finally { $stream.Dispose() }
}

function Assert-PeMachine {
    param([string]$Path, [int]$Machine)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Missing executable: $Path" }
    $actual = Get-PeMachine $Path
    if ($actual -ne $Machine) { throw ('PE architecture mismatch: {0}; expected 0x{1:X4}, got 0x{2:X4}' -f $Path, $Machine, $actual) }
}

function Set-MsvcEnvironment {
    param([string]$BatchFile, [string]$Target, [hashtable]$OriginalEnvironment)
    foreach ($entry in @(Get-ChildItem Env:)) {
        if (-not $OriginalEnvironment.ContainsKey($entry.Name)) {
            [Environment]::SetEnvironmentVariable($entry.Name, $null, 'Process')
        }
    }
    foreach ($name in $OriginalEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $OriginalEnvironment[$name], 'Process')
    }
    $command = '"' + $BatchFile + '" ' + $Target + ' >nul && set'
    $lines = & cmd.exe /d /s /c $command
    if ($LASTEXITCODE -ne 0) { throw "MSVC environment initialization failed: $Target" }
    foreach ($line in $lines) {
        if ($line -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], 'Process')
        }
    }
}

function ConvertTo-XmlAttribute {
    param([string]$Text)
    return [System.Security.SecurityElement]::Escape($Text)
}

function New-Icon {
    param([string]$Source, [string]$Destination)
    Add-Type -AssemblyName System.Drawing
    $image = [System.Drawing.Image]::FromFile($Source)
    $bitmap = [System.Drawing.Bitmap]::new(256, 256)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $memory = [System.IO.MemoryStream]::new()
    try {
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.DrawImage($image, 0, 0, 256, 256)
        $bitmap.Save($memory, [System.Drawing.Imaging.ImageFormat]::Png)
        $png = $memory.ToArray()
        $file = [System.IO.File]::Create($Destination)
        try {
            $writer = [System.IO.BinaryWriter]::new($file)
            try {
                $writer.Write([uint16]0)
                $writer.Write([uint16]1)
                $writer.Write([uint16]1)
                $writer.Write([byte]0)
                $writer.Write([byte]0)
                $writer.Write([byte]0)
                $writer.Write([byte]0)
                $writer.Write([uint16]1)
                $writer.Write([uint16]32)
                $writer.Write([uint32]$png.Length)
                $writer.Write([uint32]22)
                $writer.Write($png)
            } finally { $writer.Dispose() }
        } finally { $file.Dispose() }
    } finally {
        $memory.Dispose()
        $graphics.Dispose()
        $bitmap.Dispose()
        $image.Dispose()
    }
}

function New-Logo {
    param([string]$Source, [string]$Destination)
    Add-Type -AssemblyName System.Drawing
    $image = [System.Drawing.Image]::FromFile($Source)
    $bitmap = [System.Drawing.Bitmap]::new(128, 128)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.DrawImage($image, 0, 0, 128, 128)
        $bitmap.Save($Destination, [System.Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $graphics.Dispose()
        $bitmap.Dispose()
        $image.Dispose()
    }
}

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
foreach ($file in @('Cargo.toml', 'Cargo.lock', 'LICENSE', 'assets/overink.png', 'assets/fonts/OFL.txt')) {
    if (-not (Test-Path -LiteralPath (Join-Path $root $file) -PathType Leaf)) {
        throw "Run this script from the OverInk source tree; missing $file"
    }
}

if (-not $Version) {
    $cargoToml = [System.IO.File]::ReadAllText((Join-Path $root 'Cargo.toml'))
    $versionMatch = [regex]::Match($cargoToml, '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"')
    if (-not $versionMatch.Success) { throw 'Cannot determine Cargo package version' }
    $Version = $versionMatch.Groups[1].Value
}
if ($Version -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$') {
    throw 'Version must be numeric major.minor.patch, e.g. 0.1.0'
}
$parts = $Version.Split('.')
if ([int64]$parts[0] -gt 255 -or [int64]$parts[1] -gt 255 -or [int64]$parts[2] -gt 65535) {
    throw 'Windows Installer version fields must be <= 255.255.65535'
}

foreach ($commandName in @('rustup', 'cargo', 'dotnet', 'cmake')) {
    if (-not (Get-Command $commandName -ErrorAction SilentlyContinue)) { throw "Required build tool not found: $commandName" }
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
if (-not (Test-Path $vswhere)) { throw 'Visual Studio Installer (vswhere.exe) not found' }
$vsPath = & $vswhere -latest -products '*' -property installationPath
if ($LASTEXITCODE -ne 0 -or -not $vsPath) { throw 'Visual Studio 2022 or Build Tools is required' }
$vcvars = Join-Path $vsPath 'VC/Auxiliary/Build/vcvarsall.bat'
if (-not (Test-Path $vcvars)) { throw 'MSVC C++ build tools are missing' }

$ninjaPath = Join-Path $vsPath 'Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja'
if (Test-Path (Join-Path $ninjaPath 'ninja.exe')) { $env:PATH = "$ninjaPath;$env:PATH" }
if (-not (Get-Command ninja.exe -ErrorAction SilentlyContinue)) { throw 'Ninja is required; install the Visual Studio C++ CMake tools' }

$work = Join-Path $root '.overink-windows-build'
$dist = Join-Path $root 'dist'
$toolDir = Join-Path $work 'tools'
New-Item -ItemType Directory -Force -Path $work, $dist, $toolDir | Out-Null
if ($WixVersion -notmatch '^7\.\d+\.\d+$') { throw 'WiX 7 requires a version such as 7.0.1' }
$dotnetRuntimes = @(& dotnet --list-runtimes)
if ($LASTEXITCODE -ne 0 -or -not ($dotnetRuntimes -match '^Microsoft\.NETCore\.App 8\.')) {
    throw 'WiX 7 command-line tool requires the .NET 8 runtime. Install it before packaging.'
}
$wix = Join-Path $toolDir 'wix.exe'
if (Test-Path -LiteralPath $wix) {
    $installedTools = @(& dotnet tool list --tool-path $toolDir)
    if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect the local WiX tool installation' }
    $installedWixVersion = $null
    foreach ($line in $installedTools) {
        if ($line -match '^\s*wix\s+(\S+)\s+') { $installedWixVersion = $Matches[1]; break }
    }
    if ($installedWixVersion -ne $WixVersion) {
        Invoke-Checked 'dotnet' @('tool', 'update', 'wix', '--tool-path', $toolDir, '--version', $WixVersion)
    }
} else {
    Invoke-Checked 'dotnet' @('tool', 'install', 'wix', '--tool-path', $toolDir, '--version', $WixVersion)
}
if ($AcceptWixEula) {
    Invoke-Checked $wix @('eula', 'accept', 'wix7')
} elseif (-not (Test-Path -LiteralPath (Join-Path $env:USERPROFILE '.wix/wix7-osmf-eula.txt'))) {
    throw 'WiX 7 requires explicit OSMF EULA acceptance. Review https://www.firegiant.com/wix/osmf/ and rerun with -AcceptWixEula, or run wix eula accept wix7 first.'
}
$actualWixVersion = (& $wix --version).Trim()
if ($LASTEXITCODE -ne 0) { throw "Cannot determine WiX version: $actualWixVersion" }
$actualWixBaseVersion = ($actualWixVersion -split '\+', 2)[0]
if ($actualWixBaseVersion -ne $WixVersion) { throw "WiX version mismatch: expected $WixVersion, got $actualWixVersion" }
foreach ($extension in @('WixToolset.BootstrapperApplications.wixext', 'WixToolset.Util.wixext')) {
    Invoke-Checked $wix @('extension', 'add', '-g', "$extension/$WixVersion")
}
Invoke-Checked 'rustup' @('toolchain', 'install', $RustVersion, '--profile', 'minimal')

$icon = Join-Path $work 'OverInk.ico'
New-Icon (Join-Path $root 'assets/overink.png') $icon
$logo = Join-Path $work 'OverInk-logo.png'
New-Logo (Join-Path $root 'assets/overink.png') $logo
$themeTemplate = @'
<?xml version="1.0" encoding="utf-8"?>
<Theme xmlns="http://wixtoolset.org/schemas/v4/thmutil">
  <Font Id="0" Height="-14" Weight="500" Foreground="292222" Background="ffffff">Segoe UI</Font>
  <Font Id="1" Height="-28" Weight="650" Foreground="252020">Segoe UI</Font>
  <Font Id="2" Height="-14" Weight="450" Foreground="777070">Segoe UI</Font>
  <Font Id="3" Height="-15" Weight="600" Foreground="966863">Segoe UI</Font>
  <Font Id="4" Height="-16" Weight="450" Foreground="363030">Segoe UI</Font>
  <Window Width="640" Height="430" HexStyle="100a0000" FontId="0" Caption="#(loc.Caption)">
    <ImageControl X="32" Y="27" Width="64" Height="64" ImageFile="logo.png" Visible="yes" />
    <Label X="112" Y="29" Width="-32" Height="44" FontId="1" DisablePrefix="yes">OverInk</Label>
    <Label X="114" Y="77" Width="-32" Height="29" FontId="3" DisablePrefix="yes">#(loc.Tagline)</Label>
    <Page Name="Loading">
      <Label X="32" Y="146" Width="-32" Height="42" FontId="4">#(loc.Preparing)</Label>
      <Label Name="CheckingForUpdatesLabel" X="32" Y="195" Width="-32" Height="30" FontId="2" Visible="no" />
    </Page>
    <Page Name="Help">
      <Label X="32" Y="141" Width="-32" Height="42" FontId="1">#(loc.HelpHeader)</Label>
      <Label X="32" Y="203" Width="-32" Height="64" FontId="4">#(loc.HelpMessage)</Label>
      <Button Name="HelpCloseButton" X="-30" Y="-28" Width="110" Height="36" TabStop="yes" FontId="0">
        <Text>#(loc.Close)</Text><CloseWindowAction />
      </Button>
    </Page>
    <Page Name="Install">
      <Label X="32" Y="141" Width="-32" Height="46" FontId="1">#(loc.InstallHeader)</Label>
      <Label X="32" Y="201" Width="-32" Height="65" FontId="4">#(loc.InstallMessage)</Label>
      <Label Name="InstallVersion" X="32" Y="274" Width="-32" Height="28" FontId="2" VisibleCondition="WixStdBAShowVersion">#(loc.VersionText)</Label>
      <Label X="32" Y="307" Width="-32" Height="44" FontId="2" VisibleCondition="PreviousInstallFolder">#(loc.UpgradeFolder)</Label>
      <Button Name="OptionsButton" X="32" Y="-28" Width="124" Height="36" TabStop="yes" FontId="0" VisibleCondition="NOT PreviousInstallFolder">
        <Text>#(loc.Options)</Text><ChangePageAction Page="Options" />
      </Button>
      <Button Name="InstallButton" X="-158" Y="-28" Width="120" Height="36" TabStop="yes" FontId="0" ElevationShieldCondition='(WixBundleAuthoredScope = 1 AND NOT WixBundleElevated) OR ((WixBundleAuthoredScope = 2 OR WixBundleAuthoredScope = 3) AND WixStdBAScope = "PerMachine")'>#(loc.Install)</Button>
      <Button Name="InstallCancelButton" X="-30" Y="-28" Width="110" Height="36" TabStop="yes" FontId="0">
        <Text>#(loc.Close)</Text><CloseWindowAction />
      </Button>
    </Page>
    <Page Name="Options">
      <Label X="32" Y="130" Width="-32" Height="48" FontId="1">#(loc.OptionsHeader)</Label>
      <Label X="32" Y="183" Width="-32" Height="27" FontId="2">#(loc.InstallLocation)</Label>
      <Editbox Name="InstallFolder" X="32" Y="215" Width="432" Height="36" TabStop="yes" FontId="0" FileSystemAutoComplete="yes" />
      <Button Name="BrowseButton" X="478" Y="215" Width="130" Height="36" TabStop="yes" FontId="0">
        <Text>#(loc.Browse)</Text><BrowseDirectoryAction VariableName="InstallFolder" />
      </Button>
      <Checkbox Name="CreateDesktopShortcut" X="32" Y="273" Width="-32" Height="32" TabStop="yes" FontId="4">#(loc.DesktopShortcut)</Checkbox>
      <Checkbox Name="CreateStartMenuShortcut" X="32" Y="312" Width="-32" Height="32" TabStop="yes" FontId="4">#(loc.StartShortcut)</Checkbox>
      <Button Name="OptionsOkButton" X="-158" Y="-28" Width="120" Height="36" TabStop="yes" FontId="0">
        <Text>#(loc.Save)</Text><ChangePageAction Page="Install" />
      </Button>
      <Button Name="OptionsCancelButton" X="-30" Y="-28" Width="110" Height="36" TabStop="yes" FontId="0">
        <Text>#(loc.Back)</Text><ChangePageAction Page="Install" Cancel="yes" />
      </Button>
    </Page>
    <Page Name="Modify">
      <Label X="32" Y="143" Width="-32" Height="48" FontId="1">#(loc.ModifyHeader)</Label>
      <Label X="32" Y="208" Width="-32" Height="72" FontId="4">#(loc.ModifyMessage)</Label>
      <Button Name="RepairButton" X="32" Y="-28" Width="124" Height="36" TabStop="yes" FontId="0" HideWhenDisabled="yes">#(loc.Repair)</Button>
      <Button Name="UninstallButton" X="174" Y="-28" Width="124" Height="36" TabStop="yes" FontId="0">#(loc.Uninstall)</Button>
      <Button Name="ModifyCancelButton" X="-30" Y="-28" Width="110" Height="36" TabStop="yes" FontId="0">
        <Text>#(loc.Close)</Text><CloseWindowAction />
      </Button>
    </Page>
    <Page Name="Progress">
      <Label X="32" Y="142" Width="-32" Height="47" FontId="1">#(loc.ProgressHeader)</Label>
      <Label Name="OverallProgressPackageText" X="32" Y="209" Width="-32" Height="38" FontId="2">#(loc.ProgressPackage)</Label>
      <Progressbar Name="OverallCalculatedProgressbar" X="32" Y="272" Width="-32" Height="17" />
      <Button Name="ProgressCancelButton" X="-30" Y="-28" Width="110" Height="36" TabStop="yes" FontId="0">#(loc.Cancel)</Button>
    </Page>
    <Page Name="Success">
      <Label X="32" Y="143" Width="-32" Height="49" FontId="1">
        <Text>#(loc.SuccessHeader)</Text>
        <Text Condition="WixBundleAction = 3">#(loc.SuccessUninstall)</Text>
        <Text Condition="WixBundleAction = 5">#(loc.SuccessInstall)</Text>
        <Text Condition="WixBundleAction = 7">#(loc.SuccessRepair)</Text>
      </Label>
      <Label X="32" Y="209" Width="-32" Height="65" FontId="4">#(loc.SuccessMessage)</Label>
      <Button Name="LaunchButton" X="32" Y="-28" Width="164" Height="36" TabStop="yes" FontId="0" HideWhenDisabled="yes" VisibleCondition="WixBundleAction = 5">#(loc.Launch)</Button>
      <Label X="32" Y="309" Width="-32" Height="32" FontId="2" VisibleCondition="WixStdBARestartRequired">#(loc.RestartNeeded)</Label>
      <Button Name="SuccessRestartButton" X="-158" Y="-28" Width="120" Height="36" TabStop="yes" FontId="0" HideWhenDisabled="yes">#(loc.Restart)</Button>
      <Button Name="SuccessCloseButton" X="-30" Y="-28" Width="110" Height="36" TabStop="yes" FontId="0">
        <Text>#(loc.Done)</Text><CloseWindowAction />
      </Button>
    </Page>
    <Page Name="Failure">
      <Label X="32" Y="143" Width="-32" Height="50" FontId="1">#(loc.FailureHeader)</Label>
      <Label X="32" Y="205" Width="-32" Height="40" FontId="4">#(loc.FailureMessage)</Label>
      <Hypertext Name="FailureLogFileLink" X="32" Y="253" Width="-32" Height="31" FontId="3" TabStop="yes" HideWhenDisabled="yes">#(loc.FailureHyperlinkLogText)</Hypertext>
      <Hypertext Name="FailureMessageText" X="32" Y="292" Width="-32" Height="58" FontId="2" TabStop="yes" HideWhenDisabled="yes" />
      <Button Name="FailureRestartButton" X="-158" Y="-28" Width="120" Height="36" TabStop="yes" FontId="0" HideWhenDisabled="yes">#(loc.Restart)</Button>
      <Button Name="FailureCloseButton" X="-30" Y="-28" Width="110" Height="36" TabStop="yes" FontId="0">
        <Text>#(loc.Close)</Text><CloseWindowAction />
      </Button>
    </Page>
  </Window>
</Theme>
'@
# Bootstrapper theme is embedded in this script; no external theme file is required.
[void][System.Xml.Linq.XDocument]::Parse($themeTemplate)

$themeCultures = @(
    @{ Culture = 'en-US'; Lcid = 1033 },
    @{ Culture = 'zh-CN'; Lcid = 2052 },
    @{ Culture = 'zh-TW'; Lcid = 1028 },
    @{ Culture = 'fr-FR'; Lcid = 1036 }
)
$themeTranslations = [ordered]@{
    Caption = @('OverInk Setup', 'OverInk 安装程序', 'OverInk 安裝程式', 'Installation d’OverInk')
    Tagline = @('Draw over your desktop', '在桌面上自由书写', '在桌面上自由書寫', 'Dessinez sur votre bureau')
    Preparing = @('Preparing setup…', '正在准备安装…', '正在準備安裝…', 'Préparation de l’installation…')
    HelpHeader = @('OverInk setup', 'OverInk 安装程序', 'OverInk 安裝程式', 'Installation d’OverInk')
    HelpMessage = @('Install, repair, or remove OverInk.', '安装、修复或卸载 OverInk。', '安裝、修復或解除安裝 OverInk。', 'Installez, réparez ou désinstallez OverInk.')
    Close = @('Close', '关闭', '關閉', 'Fermer')
    InstallHeader = @('Ready to install', '准备安装', '準備安裝', 'Prêt à installer')
    InstallMessage = @('A lightweight drawing overlay for presentations and teaching.', '轻量级桌面绘图工具，适用于演示和教学。', '適用於簡報與教學的輕量級桌面繪圖工具。', 'Un outil léger pour dessiner à l’écran lors de présentations et de cours.')
    VersionText = @('Version [WixBundleVersion]', '版本 [WixBundleVersion]', '版本 [WixBundleVersion]', 'Version [WixBundleVersion]')
    UpgradeFolder = @('The existing installation folder will be kept during the update.', '更新时将保留现有安装目录。', '更新時將保留現有安裝目錄。', 'Le dossier d’installation actuel sera conservé pendant la mise à jour.')
    Options = @('Options', '选项', '選項', 'Options')
    Install = @('Install', '安装', '安裝', 'Installer')
    OptionsHeader = @('Installation options', '安装选项', '安裝選項', 'Options d’installation')
    InstallLocation = @('Install location', '安装位置', '安裝位置', 'Dossier d’installation')
    Browse = @('Browse…', '浏览…', '瀏覽…', 'Parcourir…')
    DesktopShortcut = @('Create a desktop shortcut', '创建桌面快捷方式', '建立桌面捷徑', 'Créer un raccourci sur le Bureau')
    StartShortcut = @('Create a Start menu shortcut', '创建开始菜单快捷方式', '建立開始功能表捷徑', 'Créer un raccourci dans le menu Démarrer')
    Save = @('Save', '保存', '儲存', 'Enregistrer')
    Back = @('Back', '返回', '返回', 'Retour')
    ModifyHeader = @('OverInk is installed', 'OverInk 已安装', 'OverInk 已安裝', 'OverInk est installé')
    ModifyMessage = @('You can repair the installation or uninstall OverInk.', '可以修复安装或卸载 OverInk。', '您可以修復安裝或解除安裝 OverInk。', 'Vous pouvez réparer ou désinstaller OverInk.')
    Repair = @('Repair', '修复', '修復', 'Réparer')
    Uninstall = @('Uninstall', '卸载', '解除安裝', 'Désinstaller')
    ProgressHeader = @('Setting things up', '正在配置', '正在設定', 'Configuration en cours')
    ProgressPackage = @('Installing OverInk…', '正在安装 OverInk…', '正在安裝 OverInk…', 'Installation d’OverInk…')
    Cancel = @('Cancel', '取消', '取消', 'Annuler')
    SuccessHeader = @('Operation completed', '操作已完成', '作業已完成', 'Opération terminée')
    SuccessUninstall = @('OverInk was uninstalled', 'OverInk 已卸载', 'OverInk 已解除安裝', 'OverInk a été désinstallé')
    SuccessInstall = @('OverInk is ready', 'OverInk 已准备就绪', 'OverInk 已準備就緒', 'OverInk est prêt')
    SuccessRepair = @('OverInk was repaired', 'OverInk 已修复', 'OverInk 已修復', 'OverInk a été réparé')
    SuccessMessage = @('Everything finished successfully.', '操作已成功完成。', '作業已成功完成。', 'L’opération s’est terminée avec succès.')
    Launch = @('Launch OverInk', '启动 OverInk', '啟動 OverInk', 'Lancer OverInk')
    RestartNeeded = @('Restart Windows to finish setup.', '请重启 Windows 以完成安装。', '請重新啟動 Windows 以完成安裝。', 'Redémarrez Windows pour terminer l’installation.')
    Restart = @('Restart', '重启', '重新啟動', 'Redémarrer')
    Done = @('Done', '完成', '完成', 'Terminer')
    FailureHeader = @('Setup could not finish', '安装未能完成', '安裝未能完成', 'Impossible de terminer l’installation')
    FailureMessage = @('Windows Installer reported an error.', 'Windows Installer 报告了错误。', 'Windows Installer 回報了錯誤。', 'Windows Installer a signalé une erreur.')
    ViewLog = @('View installation log', '查看安装日志', '檢視安裝記錄', 'Afficher le journal d’installation')
    ConfirmCancelMessage = @('Are you sure you want to cancel?', '确定要取消吗？', '確定要取消嗎？', 'Voulez-vous vraiment annuler ?')
    ExecuteUpgradeRelatedBundleMessage = @('Previous version', '以前的版本', '舊版', 'Version précédente')
    FilesInUseTitle = @('Files in use', '文件正在使用中', '檔案正在使用中', 'Fichiers en cours d’utilisation')
    FilesInUseLabel = @('These apps are using files that must be updated:', '以下应用程序正在使用需要更新的文件：', '下列應用程式正在使用需要更新的檔案：', 'Les applications suivantes utilisent des fichiers à mettre à jour :')
    FilesInUseNetfxCloseRadioButton = @('Close the applications.', '关闭这些应用程序。', '關閉這些應用程式。', 'Fermer les applications.')
    FilesInUseCloseRadioButton = @('Close the applications and try to restart them.', '关闭这些应用程序并尝试重新启动。', '關閉這些應用程式並嘗試重新啟動。', 'Fermer les applications et essayer de les redémarrer.')
    FilesInUseDontCloseRadioButton = @('Do not close applications; a restart will be required.', '不关闭应用程序；稍后需要重启。', '不要關閉應用程式；稍後需要重新啟動。', 'Ne pas fermer les applications ; un redémarrage sera nécessaire.')
    FilesInUseRetryButton = @('Retry', '重试', '重試', 'Réessayer')
    FilesInUseIgnoreButton = @('Ignore', '忽略', '忽略', 'Ignorer')
    FilesInUseExitButton = @('Exit', '退出', '結束', 'Quitter')
    FailureHyperlinkLogText = @('View the <a href="#">installation log</a>', '查看<a href="#">安装日志</a>', '檢視<a href="#">安裝記錄</a>', 'Afficher le <a href="#">journal d’installation</a>')
}
$themeTokens = [regex]::Matches($themeTemplate, '#\(loc\.([A-Za-z][A-Za-z0-9_]*)\)') | ForEach-Object { $_.Groups[1].Value } | Select-Object -Unique
foreach ($token in $themeTokens) {
    if (-not $themeTranslations.Contains($token)) { throw "Missing localized string: $token" }
}
$themeLocalization = @{}
for ($index = 0; $index -lt $themeCultures.Count; $index++) {
    $culture = $themeCultures[$index]
    $path = Join-Path $work ("$($culture.Culture).thm.wxl")
    $strings = foreach ($entry in $themeTranslations.GetEnumerator()) {
        $value = ConvertTo-XmlAttribute ([string]$entry.Value[$index])
        "  <String Id=`"$($entry.Key)`" Value=`"$value`" />"
    }
    $xml = "<?xml version=`"1.0`" encoding=`"utf-8`"?>`n<WixLocalization xmlns=`"http://wixtoolset.org/schemas/v4/wxl`" Culture=`"$($culture.Culture)`" Language=`"$($culture.Lcid)`" Codepage=`"65001`">`n$($strings -join "`n")`n</WixLocalization>"
    [void][System.Xml.Linq.XDocument]::Parse($xml)
    [System.IO.File]::WriteAllText($path, $xml, [System.Text.UTF8Encoding]::new($false))
    $themeLocalization[$culture.Culture] = $path
}
$localizedPayloads = @()
$localeMappings = [ordered]@{
    1033 = 'en-US'
    2052 = 'zh-CN'
    4100 = 'zh-CN'
    1028 = 'zh-TW'
    3076 = 'zh-TW'
    5124 = 'zh-TW'
    1036 = 'fr-FR'
    2060 = 'fr-FR'
    3084 = 'fr-FR'
    4108 = 'fr-FR'
    5132 = 'fr-FR'
}
foreach ($lcid in $localeMappings.Keys) {
    $source = ConvertTo-XmlAttribute $themeLocalization[$localeMappings[$lcid]]
    $localizedPayloads += "      <Payload Id=`"OverInkLocale$lcid`" Name=`"${lcid}\thm.wxl`" SourceFile=`"$source`" Compressed=`"yes`" />"
}
$localizedPayloadXml = $localizedPayloads -join "`n"


$architectureMap = [ordered]@{
    x64 = @{ Triple = 'x86_64-pc-windows-msvc'; Machine = 0x8664; Vcvars = 'x64'; WixArch = 'x64'; ProgramFiles = 'ProgramFiles64Folder'; BurnProgramFiles = 'ProgramFiles64Folder'; Bitness = 'always64'; MsiUpgrade = '60D11D65-227D-4CF2-B0F4-1B2B74D129A1'; BundleUpgrade = '75BE92A5-CDB7-4BA0-B623-C2CE58091521' }
    i386 = @{ Triple = 'i686-pc-windows-msvc'; Machine = 0x014C; Vcvars = 'x64_x86'; WixArch = 'x86'; ProgramFiles = 'ProgramFilesFolder'; BurnProgramFiles = 'ProgramFilesFolder'; Bitness = 'always32'; MsiUpgrade = '0E462605-4017-4A9D-A453-E9352E974020'; BundleUpgrade = 'B984C07A-308D-4D41-B45A-97694AA36FB7' }
    arm64 = @{ Triple = 'aarch64-pc-windows-msvc'; Machine = 0xAA64; Vcvars = 'x64_arm64'; WixArch = 'arm64'; ProgramFiles = 'ProgramFiles64Folder'; BurnProgramFiles = 'ProgramFiles64Folder'; Bitness = 'always64'; MsiUpgrade = '296936D7-8EA0-4396-9EA1-564FDD7BE7D0'; BundleUpgrade = '81759159-0874-4DA4-8AE0-5E043CBEE2B9' }
}
$selected = if ($Arch -eq 'all') { @('x64', 'i386', 'arm64') } else { @($Arch) }
$originalEnvironment = @{}
Get-ChildItem Env: | ForEach-Object { $originalEnvironment[$_.Name] = $_.Value }

Push-Location $root
try {
    foreach ($name in $selected) {
        $a = $architectureMap[$name]
        Write-Host "Building OverInk $Version for $name ($($a.Triple))"
        Set-MsvcEnvironment $vcvars $a.Vcvars $originalEnvironment
        $env:CMAKE_GENERATOR = 'Ninja'
        $env:RUSTFLAGS = '-C target-feature=+crt-static'
        $env:CARGO_TARGET_DIR = Join-Path $root 'target'
        $toolchainFile = Join-Path $work 'static-crt.cmake'
        [System.IO.File]::WriteAllLines($toolchainFile, @(
            'set(CMAKE_POLICY_DEFAULT_CMP0091 NEW)',
            'set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded" CACHE STRING "" FORCE)'
        ), [System.Text.Encoding]::ASCII)
        $env:CMAKE_TOOLCHAIN_FILE = $toolchainFile
        $toolchain = "+$RustVersion"
        Invoke-Checked 'rustup' @('target', 'add', '--toolchain', $RustVersion, $a.Triple)

        $rcFile = Join-Path $work 'OverInk.rc'
        $resFile = Join-Path $work "$name.res"
        [System.IO.File]::WriteAllText($rcFile, '1 ICON "' + $icon.Replace('\', '/') + '"', [System.Text.Encoding]::ASCII)
        Invoke-Checked 'rc.exe' @('/nologo', "/fo$resFile", $rcFile)
        Invoke-Checked 'cargo' @($toolchain, 'rustc', '--locked', '--release', '--target', $a.Triple, '--bin', 'overink', '--', '-C', "link-arg=$resFile")
        $binary = Join-Path $root "target/$($a.Triple)/release/overink.exe"
        Assert-PeMachine $binary $a.Machine

        $inputs = @{
            Version = ConvertTo-XmlAttribute $Version
            Arch = ConvertTo-XmlAttribute $name
            Executable = ConvertTo-XmlAttribute $binary
            License = ConvertTo-XmlAttribute (Join-Path $root 'LICENSE')
            FontLicense = ConvertTo-XmlAttribute (Join-Path $root 'assets/fonts/OFL.txt')
            Icon = ConvertTo-XmlAttribute $icon
            ProgramFiles = $a.ProgramFiles
            MsiUpgrade = $a.MsiUpgrade
            BundleUpgrade = $a.BundleUpgrade
        }

        $msiXml = @'
<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">
  <Package Name="OverInk ({Arch})" Manufacturer="OverInk Contributors" Version="{Version}" Language="1033" UpgradeCode="{MsiUpgrade}" Scope="perMachine" InstallerVersion="500">
    <MajorUpgrade DowngradeErrorMessage="A newer version of OverInk is already installed." />
    <Property Id="CREATE_DESKTOP_SHORTCUT" Secure="yes" Value="1" />
    <Property Id="CREATE_STARTMENU_SHORTCUT" Secure="yes" Value="1" />
    <Property Id="PREVIOUS_INSTALLFOLDER" Secure="yes">
      <RegistrySearch Id="PreviousOverInkInstallDir" Root="HKLM" Key="Software\OverInk\{Arch}" Name="InstallDir" Type="raw" />
    </Property>
    <Launch Condition="NOT WIX_UPGRADE_DETECTED OR PREVIOUS_INSTALLFOLDER" Message="Cannot find the previous installation directory. Uninstall the old version before installing this version." />
    <SetDirectory Id="INSTALLFOLDER" Value="[PREVIOUS_INSTALLFOLDER]" Condition="WIX_UPGRADE_DETECTED AND PREVIOUS_INSTALLFOLDER" />
    <MediaTemplate EmbedCab="yes" />
    <StandardDirectory Id="{ProgramFiles}">
      <Directory Id="INSTALLFOLDER" Name="OverInk" />
    </StandardDirectory>
    <StandardDirectory Id="ProgramMenuFolder">
      <Directory Id="OVERINKMENU" Name="OverInk" />
    </StandardDirectory>
    <Component Id="AppExecutable" Directory="INSTALLFOLDER" Guid="*">
      <File Id="OverInkExe" Source="{Executable}" Name="OverInk.exe" KeyPath="yes" />
      <RegistryValue Root="HKLM" Key="Software\OverInk\{Arch}" Name="InstallDir" Type="string" Value="[INSTALLFOLDER]" />
      <RegistryValue Root="HKLM" Key="Software\OverInk\{Arch}" Name="DesktopEnabled" Type="string" Value="[CREATE_DESKTOP_SHORTCUT]" />
      <RegistryValue Root="HKLM" Key="Software\OverInk\{Arch}" Name="StartMenuEnabled" Type="string" Value="[CREATE_STARTMENU_SHORTCUT]" />
    </Component>
    <Component Id="LicenseFile" Directory="INSTALLFOLDER" Guid="*">
      <File Id="License" Source="{License}" Name="LICENSE.txt" KeyPath="yes" />
    </Component>
    <Component Id="FontLicenseFile" Directory="INSTALLFOLDER" Guid="*">
      <File Id="FontLicense" Source="{FontLicense}" Name="OFL.txt" KeyPath="yes" />
    </Component>
    <Component Id="DesktopShortcutComponent" Directory="DesktopFolder" Guid="*">
      <Shortcut Id="DesktopShortcut" Name="OverInk" Target="[INSTALLFOLDER]OverInk.exe" WorkingDirectory="INSTALLFOLDER" />
      <RegistryValue Root="HKLM" Key="Software\OverInk\{Arch}" Name="DesktopShortcut" Type="integer" Value="1" KeyPath="yes" />
    </Component>
    <Component Id="StartMenuShortcutComponent" Directory="OVERINKMENU" Guid="*">
      <Shortcut Id="StartMenuShortcut" Name="OverInk" Target="[INSTALLFOLDER]OverInk.exe" WorkingDirectory="INSTALLFOLDER" />
      <RemoveFolder Id="RemoveOverInkMenu" On="uninstall" />
      <RegistryValue Root="HKLM" Key="Software\OverInk\{Arch}" Name="StartMenuShortcut" Type="integer" Value="1" KeyPath="yes" />
    </Component>
    <Feature Id="Main" Title="OverInk application" Description="Installs the OverInk application" Level="1" Display="expand" AllowAbsent="no" AllowAdvertise="no" ConfigurableDirectory="INSTALLFOLDER">
      <ComponentRef Id="AppExecutable" />
      <ComponentRef Id="LicenseFile" />
      <ComponentRef Id="FontLicenseFile" />
      <Feature Id="DesktopOption" Title="Desktop shortcut" Level="1" AllowAdvertise="no">
        <Level Condition='CREATE_DESKTOP_SHORTCUT = "0"' Value="0" />
        <ComponentRef Id="DesktopShortcutComponent" />
      </Feature>
      <Feature Id="StartMenuOption" Title="Start menu shortcut" Level="1" AllowAdvertise="no">
        <Level Condition='CREATE_STARTMENU_SHORTCUT = "0"' Value="0" />
        <ComponentRef Id="StartMenuShortcutComponent" />
      </Feature>
    </Feature>
    <Icon Id="OverInkIcon" SourceFile="{Icon}" />
    <Property Id="ARPPRODUCTICON" Value="OverInkIcon" />
  </Package>
</Wix>
'@
        foreach ($key in $inputs.Keys) { $msiXml = $msiXml.Replace('{' + $key + '}', [string]$inputs[$key]) }
        [void][System.Xml.Linq.XDocument]::Parse($msiXml)
        $msiSource = Join-Path $work "$name.msi.wxs"
        [System.IO.File]::WriteAllText($msiSource, $msiXml, [System.Text.UTF8Encoding]::new($false))
        $msi = Join-Path $work "OverInk-$Version-$name.msi"
        Invoke-Checked $wix @('build', $msiSource, '-arch', $a.WixArch, '-o', $msi)

        $themePath = Join-Path $work "$name.theme.xml"
        [System.IO.File]::WriteAllText($themePath, $themeTemplate, [System.Text.UTF8Encoding]::new($false))
        $bundleXml = @'
<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs" xmlns:bal="http://wixtoolset.org/schemas/v4/wxs/bal" xmlns:util="http://wixtoolset.org/schemas/v4/wxs/util">
  <Bundle Name="OverInk ({Arch})" Manufacturer="OverInk Contributors" Version="{Version}" UpgradeCode="{BundleUpgrade}" IconSourceFile="{Icon}">
    <Variable Name="InstallFolder" Type="string" Value="[{BurnProgramFiles}]OverInk" />
    <Variable Name="CreateDesktopShortcut" Type="numeric" Value="1" />
    <Variable Name="CreateStartMenuShortcut" Type="numeric" Value="1" />
    <util:RegistrySearch Id="PreviousPath" Root="HKLM" Key="Software\OverInk\{Arch}" Value="InstallDir" Variable="PreviousInstallFolder" Result="value" Bitness="{Bitness}" />
    <util:RegistrySearch Id="PreviousDesktop" Root="HKLM" Key="Software\OverInk\{Arch}" Value="DesktopEnabled" Variable="PreviousDesktopEnabled" Result="value" Bitness="{Bitness}" />
    <util:RegistrySearch Id="PreviousStartMenu" Root="HKLM" Key="Software\OverInk\{Arch}" Value="StartMenuEnabled" Variable="PreviousStartMenuEnabled" Result="value" Bitness="{Bitness}" />
    <SetVariable Id="KeepOldFolder" Variable="InstallFolder" Value="[PreviousInstallFolder]" Type="formatted" Condition="PreviousInstallFolder" After="PreviousPath" />
    <SetVariable Id="KeepDesktopChoice" Variable="CreateDesktopShortcut" Value="[PreviousDesktopEnabled]" Type="numeric" Condition='PreviousDesktopEnabled = "0" OR PreviousDesktopEnabled = "1"' After="PreviousDesktop" />
    <SetVariable Id="KeepStartMenuChoice" Variable="CreateStartMenuShortcut" Value="[PreviousStartMenuEnabled]" Type="numeric" Condition='PreviousStartMenuEnabled = "0" OR PreviousStartMenuEnabled = "1"' After="PreviousStartMenu" />
    <BootstrapperApplication>
      <bal:WixStandardBootstrapperApplication Theme="hyperlinkLargeLicense" ThemeFile="{Theme}" LocalizationFile="{Localization}" LogoFile="{Logo}" LicenseUrl="" ShowVersion="yes" LaunchTarget="[InstallFolder]\OverInk.exe" SuppressRepair="no" SuppressOptionsUI="no" />
{LocalizedPayloads}
    </BootstrapperApplication>
    <Chain>
      <MsiPackage Id="OverInkMsi" SourceFile="{Msi}" Compressed="yes" Visible="no" Vital="yes">
        <MsiProperty Name="INSTALLFOLDER" Value="[InstallFolder]" />
        <MsiProperty Name="CREATE_DESKTOP_SHORTCUT" Value="[CreateDesktopShortcut]" />
        <MsiProperty Name="CREATE_STARTMENU_SHORTCUT" Value="[CreateStartMenuShortcut]" />
      </MsiPackage>
    </Chain>
  </Bundle>
</Wix>
'@
        $bundleInputs = @{
            Arch = $inputs.Arch
            Version = $inputs.Version
            BundleUpgrade = $a.BundleUpgrade
            Icon = $inputs.Icon
            Msi = ConvertTo-XmlAttribute $msi
            Theme = ConvertTo-XmlAttribute $themePath
            Localization = ConvertTo-XmlAttribute $themeLocalization['en-US']
            LocalizedPayloads = $localizedPayloadXml
            Logo = ConvertTo-XmlAttribute $logo
            BurnProgramFiles = $a.BurnProgramFiles
            Bitness = $a.Bitness
        }
        foreach ($key in $bundleInputs.Keys) { $bundleXml = $bundleXml.Replace('{' + $key + '}', [string]$bundleInputs[$key]) }
        [void][System.Xml.Linq.XDocument]::Parse($bundleXml)
        $bundleSource = Join-Path $work "$name.bundle.wxs"
        [System.IO.File]::WriteAllText($bundleSource, $bundleXml, [System.Text.UTF8Encoding]::new($false))
        $setup = Join-Path $dist "OverInk-$Version-windows-$name-setup.exe"
        Invoke-Checked $wix @('build', $bundleSource, '-arch', $a.WixArch, '-ext', 'WixToolset.BootstrapperApplications.wixext', '-ext', 'WixToolset.Util.wixext', '-o', $setup)
        Assert-PeMachine $setup $a.Machine
        Write-Host "Created $setup"
    }
} finally {
    Pop-Location
    foreach ($entry in @(Get-ChildItem Env:)) {
        if (-not $originalEnvironment.ContainsKey($entry.Name)) {
            [Environment]::SetEnvironmentVariable($entry.Name, $null, 'Process')
        }
    }
    foreach ($key in $originalEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($key, $originalEnvironment[$key], 'Process')
    }
}
