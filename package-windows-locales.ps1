# Shared WiX 7 Burn translations for the nine additional installer languages.
# Order is intentionally validated against $themeTranslations in package-windows.ps1.
# A missing or reordered token fails before building any architecture.
$installerLocaleTokens = @(
    'Caption',
    'Tagline',
    'Preparing',
    'HelpHeader',
    'HelpMessage',
    'Close',
    'InstallHeader',
    'InstallMessage',
    'VersionText',
    'UpgradeFolder',
    'Options',
    'Install',
    'OptionsHeader',
    'InstallLocation',
    'Browse',
    'DesktopShortcut',
    'StartShortcut',
    'Save',
    'Back',
    'ModifyHeader',
    'ModifyMessage',
    'Repair',
    'Uninstall',
    'ProgressHeader',
    'ProgressPackage',
    'Cancel',
    'SuccessHeader',
    'SuccessUninstall',
    'SuccessInstall',
    'SuccessRepair',
    'SuccessMessage',
    'Launch',
    'RestartNeeded',
    'Restart',
    'Done',
    'FailureHeader',
    'FailureMessage',
    'ViewLog',
    'ConfirmCancelMessage',
    'ExecuteUpgradeRelatedBundleMessage',
    'FilesInUseTitle',
    'FilesInUseLabel',
    'FilesInUseNetfxCloseRadioButton',
    'FilesInUseCloseRadioButton',
    'FilesInUseDontCloseRadioButton',
    'FilesInUseRetryButton',
    'FilesInUseIgnoreButton',
    'FilesInUseExitButton',
    'FailureHyperlinkLogText'
)
$installerLocaleStrings = @{}

$installerLocaleStrings['es-ES'] = @(
    'Instalador de OverInk'
    'Dibuja sobre tu escritorio'
    'Preparando instalación…'
    'Instalador de OverInk'
    'Instala, repara o quita OverInk.'
    'Cerrar'
    'Listo para instalar'
    'Dibuja sobre el escritorio en clase o presentaciones.'
    'Versión [WixBundleVersion]'
    'Se conservará la carpeta actual al actualizar.'
    'Opciones'
    'Instalar'
    'Opciones de instalación'
    'Carpeta de instalación'
    'Examinar…'
    'Acceso directo en el escritorio'
    'Acceso directo en Inicio'
    'Guardar'
    'Volver'
    'OverInk está instalado'
    'Puedes reparar o desinstalar OverInk.'
    'Reparar'
    'Desinstalar'
    'Preparando…'
    'Instalando OverInk…'
    'Cancelar'
    'Operación completada'
    'OverInk desinstalado'
    'OverInk listo'
    'OverInk reparado'
    'Todo terminó correctamente.'
    'Abrir OverInk'
    'Reinicia Windows para terminar.'
    'Reiniciar'
    'Listo'
    'No se pudo completar'
    'Windows Installer detectó un error.'
    'Ver registro'
    '¿Seguro que quieres cancelar?'
    'Versión anterior'
    'Archivos en uso'
    'Estas aplicaciones usan archivos que deben actualizarse:'
    'Cerrar aplicaciones.'
    'Cerrar y reiniciar aplicaciones.'
    'No cerrar; será necesario reiniciar Windows.'
    'Reintentar'
    'Ignorar'
    'Salir'
    'Ver el <a href="#">registro de instalación</a>'
)
