[CmdletBinding()]
param(
    [ValidateSet('all', 'x64', 'i386', 'arm64')]
    [string]$Arch = 'all',
    [string]$Version,
    [string]$RustVersion = '1.99.1',
    [string]$WixVersion = '6.0.2'
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
    $bitmap = [System.Drawing.Bitmap]::new(64, 64)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.DrawImage($image, 0, 0, 64, 64)
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
$wix = Join-Path $toolDir 'wix.exe'
if (-not (Test-Path $wix)) {
    Invoke-Checked 'dotnet' @('tool', 'install', 'wix', '--tool-path', $toolDir, '--version', $WixVersion)
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
  <Font Id="0" Height="-13" Weight="500" Foreground="292222" Background="ffffff">Segoe UI</Font>
  <Font Id="1" Height="-25" Weight="650" Foreground="252020">Segoe UI</Font>
  <Font Id="2" Height="-13" Weight="450" Foreground="777070">Segoe UI</Font>
  <Font Id="3" Height="-14" Weight="600" Foreground="966863">Segoe UI</Font>
  <Font Id="4" Height="-15" Weight="450" Foreground="363030">Segoe UI</Font>
  <Window Width="540" Height="355" HexStyle="100a0000" FontId="0" Caption="OverInk Setup">
    <ImageControl X="28" Y="23" Width="58" Height="58" ImageFile="logo.png" Visible="yes" />
    <Label X="98" Y="25" Width="410" Height="34" FontId="1" DisablePrefix="yes">OverInk</Label>
    <Label X="100" Y="62" Width="410" Height="22" FontId="3" DisablePrefix="yes">Draw over your desktop</Label>
    <Page Name="Loading">
      <Label X="28" Y="115" Width="475" Height="32" FontId="4">Preparing setup...</Label>
      <Label Name="CheckingForUpdatesLabel" X="28" Y="152" Width="475" Height="20" FontId="2" Visible="no"/>
    </Page>
    <Page Name="Help">
      <Label X="28" Y="116" Width="475" Height="35" FontId="4">OverInk setup</Label>
      <Label X="28" Y="151" Width="475" Height="34" FontId="2">Install, repair, or remove OverInk.</Label>
      <Button Name="HelpCloseButton" X="-28" Y="-25" Width="96" Height="30" TabStop="yes" FontId="0">
        <Text>Close</Text><CloseWindowAction />
      </Button>
    </Page>
    <Page Name="Install">
      <Label X="28" Y="120" Width="470" Height="34" FontId="1">Ready to install</Label>
      <Label X="28" Y="170" Width="470" Height="45" FontId="4">A lightweight drawing overlay for presentations and teaching.</Label>
      <Label Name="InstallVersion" X="28" Y="230" Width="310" Height="20" FontId="2" VisibleCondition="WixStdBAShowVersion">Version {Version}</Label>
      <Label X="28" Y="258" Width="465" Height="20" FontId="2" VisibleCondition="PreviousInstallFolder">Upgrade will keep your existing installation folder.</Label>
      <Button Name="OptionsButton" X="28" Y="-25" Width="112" Height="30" TabStop="yes" FontId="0" VisibleCondition="NOT PreviousInstallFolder">
        <Text>Options</Text><ChangePageAction Page="Options" />
      </Button>
      <Button Name="InstallButton" X="-136" Y="-25" Width="105" Height="30" TabStop="yes" FontId="0" ElevationShieldCondition='(WixBundleAuthoredScope = 1 AND NOT WixBundleElevated) OR ((WixBundleAuthoredScope = 2 OR WixBundleAuthoredScope = 3) AND WixStdBAScope = "PerMachine")'>Install</Button>
      <Button Name="InstallCancelButton" X="-28" Y="-25" Width="96" Height="30" TabStop="yes" FontId="0">
        <Text>Close</Text><CloseWindowAction />
      </Button>
    </Page>
    <Page Name="Options">
      <Label X="28" Y="105" Width="470" Height="30" FontId="1">Installation options</Label>
      <Label X="28" Y="150" Width="470" Height="20" FontId="2">Install location</Label>
      <Editbox Name="InstallFolder" X="28" Y="175" Width="365" Height="26" TabStop="yes" FontId="0" FileSystemAutoComplete="yes" />
      <Button Name="BrowseButton" X="405" Y="175" Width="100" Height="26" TabStop="yes" FontId="0">
        <Text>Browse</Text><BrowseDirectoryAction VariableName="InstallFolder" />
      </Button>
      <Checkbox Name="CreateDesktopShortcut" X="28" Y="222" Width="470" Height="22" TabStop="yes" FontId="4">Create a desktop shortcut</Checkbox>
      <Checkbox Name="CreateStartMenuShortcut" X="28" Y="251" Width="470" Height="22" TabStop="yes" FontId="4">Create a Start menu shortcut</Checkbox>
      <Button Name="OptionsOkButton" X="-136" Y="-25" Width="105" Height="30" TabStop="yes" FontId="0">
        <Text>Save</Text><ChangePageAction Page="Install" />
      </Button>
      <Button Name="OptionsCancelButton" X="-28" Y="-25" Width="96" Height="30" TabStop="yes" FontId="0">
        <Text>Back</Text><ChangePageAction Page="Install" Cancel="yes" />
      </Button>
    </Page>
    <Page Name="Modify">
      <Label X="28" Y="124" Width="470" Height="36" FontId="1">OverInk is installed</Label>
      <Label X="28" Y="171" Width="470" Height="45" FontId="4">You can repair the installation or uninstall OverInk.</Label>
      <Button Name="RepairButton" X="28" Y="-25" Width="105" Height="30" TabStop="yes" FontId="0" HideWhenDisabled="yes">Repair</Button>
      <Button Name="UninstallButton" X="145" Y="-25" Width="105" Height="30" TabStop="yes" FontId="0">Uninstall</Button>
      <Button Name="ModifyCancelButton" X="-28" Y="-25" Width="96" Height="30" TabStop="yes" FontId="0">
        <Text>Close</Text><CloseWindowAction />
      </Button>
    </Page>
    <Page Name="Progress">
      <Label X="28" Y="122" Width="470" Height="36" FontId="1">Setting things up</Label>
      <Label Name="OverallProgressPackageText" X="28" Y="170" Width="470" Height="24" FontId="2">Installing OverInk...</Label>
      <Progressbar Name="OverallCalculatedProgressbar" X="28" Y="214" Width="477" Height="12" />
      <Button Name="ProgressCancelButton" X="-28" Y="-25" Width="96" Height="30" TabStop="yes" FontId="0">Cancel</Button>
    </Page>
    <Page Name="Success">
      <Label X="28" Y="122" Width="470" Height="36" FontId="1">
        <Text>Operation completed</Text>
        <Text Condition="WixBundleAction = 3">OverInk removed</Text>
        <Text Condition="WixBundleAction = 5">OverInk is ready</Text>
        <Text Condition="WixBundleAction = 7">OverInk repaired</Text>
      </Label>
      <Label X="28" Y="170" Width="470" Height="44" FontId="4">Everything finished successfully.</Label>
      <Button Name="LaunchButton" X="28" Y="-25" Width="130" Height="30" TabStop="yes" FontId="0" HideWhenDisabled="yes" VisibleCondition="WixBundleAction = 5">Launch OverInk</Button>
      <Label X="28" Y="248" Width="470" Height="25" FontId="2" VisibleCondition="WixStdBARestartRequired">Restart Windows to finish setup.</Label>
      <Button Name="SuccessRestartButton" X="-136" Y="-25" Width="105" Height="30" TabStop="yes" FontId="0" HideWhenDisabled="yes">Restart</Button>
      <Button Name="SuccessCloseButton" X="-28" Y="-25" Width="96" Height="30" TabStop="yes" FontId="0">
        <Text>Done</Text><CloseWindowAction />
      </Button>
    </Page>
    <Page Name="Failure">
      <Label X="28" Y="122" Width="470" Height="36" FontId="1">Setup could not finish</Label>
      <Label X="28" Y="170" Width="470" Height="26" FontId="4">Windows Installer reported an error.</Label>
      <Hypertext Name="FailureLogFileLink" X="28" Y="211" Width="470" Height="24" FontId="3" TabStop="yes" HideWhenDisabled="yes">View installation log</Hypertext>
      <Hypertext Name="FailureMessageText" X="28" Y="239" Width="470" Height="37" FontId="2" TabStop="yes" HideWhenDisabled="yes" />
      <Button Name="FailureRestartButton" X="-136" Y="-25" Width="105" Height="30" TabStop="yes" FontId="0" HideWhenDisabled="yes">Restart</Button>
      <Button Name="FailureCloseButton" X="-28" Y="-25" Width="96" Height="30" TabStop="yes" FontId="0">
        <Text>Close</Text><CloseWindowAction />
      </Button>
    </Page>
  </Window>
</Theme>
'@
# Bootstrapper theme is embedded in this script; no external theme file is required.
[void][System.Xml.Linq.XDocument]::Parse($themeTemplate)

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
        $msiSource = Join-Path $work "$name.msi.wxs"
        [System.IO.File]::WriteAllText($msiSource, $msiXml, [System.Text.UTF8Encoding]::new($false))
        $msi = Join-Path $work "OverInk-$Version-$name.msi"
        Invoke-Checked $wix @('build', $msiSource, '-arch', $a.WixArch, '-o', $msi)

        $themePath = Join-Path $work "$name.theme.xml"
        [System.IO.File]::WriteAllText($themePath, $themeTemplate.Replace('{Version}', $Version), [System.Text.UTF8Encoding]::new($false))
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
      <bal:WixStandardBootstrapperApplication Theme="hyperlinkLargeLicense" ThemeFile="{Theme}" LogoFile="{Logo}" LicenseUrl="" ShowVersion="yes" LaunchTarget="[InstallFolder]\OverInk.exe" SuppressRepair="no" SuppressOptionsUI="no" />
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
            Logo = ConvertTo-XmlAttribute $logo
            BurnProgramFiles = $a.BurnProgramFiles
            Bitness = $a.Bitness
        }
        foreach ($key in $bundleInputs.Keys) { $bundleXml = $bundleXml.Replace('{' + $key + '}', [string]$bundleInputs[$key]) }
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
