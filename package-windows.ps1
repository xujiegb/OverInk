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

function Protect-UpgradeInstallDirectory {
    param([string]$MsiPath)
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $installer.OpenDatabase($MsiPath, 1)
    try {
        foreach ($action in @('Hide', 'Disable')) {
            $sql = "INSERT INTO ``ControlCondition`` (``Dialog_``, ``Control_``, ``Action``, ``Condition``) VALUES ('CustomizeDlg', 'Browse', '$action', 'WIX_UPGRADE_DETECTED')"
            $view = $database.OpenView($sql)
            try { $view.Execute() } finally { [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($view) }
        }
        $sql = "UPDATE ``ControlEvent`` SET ``Condition`` = 'NOT WIX_UPGRADE_DETECTED' WHERE ``Dialog_`` = 'CustomizeDlg' AND ``Control_`` = 'Browse' AND ``Event`` = 'SelectionBrowse'"
        $view = $database.OpenView($sql)
        try { $view.Execute() } finally { [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($view) }
        $database.Commit()
    } finally {
        [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($database)
        [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($installer)
    }
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
foreach ($extension in @('WixToolset.UI.wixext', 'WixToolset.BootstrapperApplications.wixext')) {
    Invoke-Checked $wix @('extension', 'add', '-g', "$extension/$WixVersion")
}
Invoke-Checked 'rustup' @('toolchain', 'install', $RustVersion, '--profile', 'minimal')

$icon = Join-Path $work 'OverInk.ico'
New-Icon (Join-Path $root 'assets/overink.png') $icon
$licenseRtf = Join-Path $work 'LICENSE.rtf'
$licenseText = [System.IO.File]::ReadAllText((Join-Path $root 'LICENSE'))
$escapedLicense = $licenseText.Replace('\', '\\').Replace('{', '\{').Replace('}', '\}')
$escapedLicense = [regex]::Replace($escapedLicense, '\r?\n', '\par ')
[System.IO.File]::WriteAllText($licenseRtf, "{\rtf1\ansi $escapedLicense}", [System.Text.Encoding]::ASCII)

$architectureMap = [ordered]@{
    x64 = @{ Triple = 'x86_64-pc-windows-msvc'; Machine = 0x8664; Vcvars = 'x64'; WixArch = 'x64'; ProgramFiles = 'ProgramFiles64Folder'; MsiUpgrade = '60D11D65-227D-4CF2-B0F4-1B2B74D129A1'; BundleUpgrade = '75BE92A5-CDB7-4BA0-B623-C2CE58091521' }
    i386 = @{ Triple = 'i686-pc-windows-msvc'; Machine = 0x014C; Vcvars = 'x64_x86'; WixArch = 'x86'; ProgramFiles = 'ProgramFilesFolder'; MsiUpgrade = '0E462605-4017-4A9D-A453-E9352E974020'; BundleUpgrade = 'B984C07A-308D-4D41-B45A-97694AA36FB7' }
    arm64 = @{ Triple = 'aarch64-pc-windows-msvc'; Machine = 0xAA64; Vcvars = 'x64_arm64'; WixArch = 'arm64'; ProgramFiles = 'ProgramFiles64Folder'; MsiUpgrade = '296936D7-8EA0-4396-9EA1-564FDD7BE7D0'; BundleUpgrade = '81759159-0874-4DA4-8AE0-5E043CBEE2B9' }
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
            Rtf = ConvertTo-XmlAttribute $licenseRtf
            Icon = ConvertTo-XmlAttribute $icon
            ProgramFiles = $a.ProgramFiles
            MsiUpgrade = $a.MsiUpgrade
            BundleUpgrade = $a.BundleUpgrade
        }

        $msiXml = @'
<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs" xmlns:ui="http://wixtoolset.org/schemas/v4/wxs/ui">
  <Package Name="OverInk ({Arch})" Manufacturer="OverInk Contributors" Version="{Version}" Language="1033" UpgradeCode="{MsiUpgrade}" Scope="perMachine" InstallerVersion="500">
    <MajorUpgrade DowngradeErrorMessage="A newer version of OverInk is already installed." />
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
      <Feature Id="DesktopOption" Title="Desktop shortcut" Description="Create an OverInk desktop shortcut" Level="1" AllowAdvertise="no">
        <ComponentRef Id="DesktopShortcutComponent" />
      </Feature>
      <Feature Id="StartMenuOption" Title="Start menu shortcut" Description="Create an OverInk Start menu shortcut" Level="1" AllowAdvertise="no">
        <ComponentRef Id="StartMenuShortcutComponent" />
      </Feature>
    </Feature>
    <ui:WixUI Id="WixUI_FeatureTree" InstallDirectory="INSTALLFOLDER" />
    <WixVariable Id="WixUILicenseRtf" Value="{Rtf}" />
    <Icon Id="OverInkIcon" SourceFile="{Icon}" />
    <Property Id="ARPPRODUCTICON" Value="OverInkIcon" />
  </Package>
</Wix>
'@
        foreach ($key in $inputs.Keys) { $msiXml = $msiXml.Replace('{' + $key + '}', [string]$inputs[$key]) }
        $msiSource = Join-Path $work "$name.msi.wxs"
        [System.IO.File]::WriteAllText($msiSource, $msiXml, [System.Text.UTF8Encoding]::new($false))
        $msi = Join-Path $work "OverInk-$Version-$name.msi"
        Invoke-Checked $wix @('build', $msiSource, '-arch', $a.WixArch, '-ext', 'WixToolset.UI.wixext', '-o', $msi)
        Protect-UpgradeInstallDirectory $msi

        $bundleXml = @'
<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs" xmlns:bal="http://wixtoolset.org/schemas/v4/wxs/bal">
  <Bundle Name="OverInk ({Arch})" Manufacturer="OverInk Contributors" Version="{Version}" UpgradeCode="{BundleUpgrade}" IconSourceFile="{Icon}">
    <BootstrapperApplication>
      <bal:WixInternalUIBootstrapperApplication Theme="standard" />
    </BootstrapperApplication>
    <Chain>
      <MsiPackage Id="OverInkMsi" SourceFile="{Msi}" Compressed="yes" Visible="no" />
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
        }
        foreach ($key in $bundleInputs.Keys) { $bundleXml = $bundleXml.Replace('{' + $key + '}', [string]$bundleInputs[$key]) }
        $bundleSource = Join-Path $work "$name.bundle.wxs"
        [System.IO.File]::WriteAllText($bundleSource, $bundleXml, [System.Text.UTF8Encoding]::new($false))
        $setup = Join-Path $dist "OverInk-$Version-windows-$name-setup.exe"
        Invoke-Checked $wix @('build', $bundleSource, '-arch', $a.WixArch, '-ext', 'WixToolset.BootstrapperApplications.wixext', '-o', $setup)
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
