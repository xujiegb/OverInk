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

$installerLocaleStrings['ja-JP'] = @(
    'OverInk セットアップ'
    'デスクトップに描画'
    '準備しています…'
    'OverInk セットアップ'
    'インストール・修復・削除を行います。'
    '閉じる'
    'インストール準備完了'
    '授業や発表で画面に描ける軽量ツール。'
    'バージョン [WixBundleVersion]'
    '更新時は現在の保存先を維持します。'
    'オプション'
    'インストール'
    'インストール設定'
    'インストール先'
    '参照…'
    'デスクトップにショートカット'
    'スタートにショートカット'
    '保存'
    '戻る'
    'OverInk はインストール済みです'
    '修復またはアンインストールできます。'
    '修復'
    'アンインストール'
    '設定中…'
    'OverInk をインストール中…'
    'キャンセル'
    '操作が完了しました'
    'OverInk を削除しました'
    'OverInk を使用できます'
    'OverInk を修復しました'
    '正常に完了しました。'
    'OverInk を起動'
    '完了には Windows の再起動が必要です。'
    '再起動'
    '完了'
    'セットアップできませんでした'
    'Windows Installer でエラーが発生しました。'
    'ログを見る'
    'キャンセルしますか？'
    '以前のバージョン'
    '使用中のファイル'
    '更新するファイルを使用中のアプリ：'
    'アプリを閉じる。'
    'アプリを閉じて再起動を試みる。'
    'アプリを閉じない。Windows の再起動が必要です。'
    '再試行'
    '無視'
    '終了'
    '<a href="#">インストールログ</a>を見る'
)

$installerLocaleStrings['de-DE'] = @(
    'OverInk-Setup'
    'Direkt auf dem Desktop zeichnen'
    'Setup wird vorbereitet…'
    'OverInk-Setup'
    'OverInk installieren, reparieren oder entfernen.'
    'Schließen'
    'Bereit zur Installation'
    'Leichtes Zeichenwerkzeug für Vortrag und Unterricht.'
    'Version [WixBundleVersion]'
    'Der bisherige Installationsordner bleibt erhalten.'
    'Optionen'
    'Installieren'
    'Installationsoptionen'
    'Installationsordner'
    'Durchsuchen…'
    'Desktopverknüpfung erstellen'
    'Startmenüverknüpfung erstellen'
    'Speichern'
    'Zurück'
    'OverInk ist installiert'
    'Sie können OverInk reparieren oder entfernen.'
    'Reparieren'
    'Deinstallieren'
    'Einrichtung läuft…'
    'OverInk wird installiert…'
    'Abbrechen'
    'Vorgang abgeschlossen'
    'OverInk wurde entfernt'
    'OverInk ist bereit'
    'OverInk wurde repariert'
    'Erfolgreich abgeschlossen.'
    'OverInk starten'
    'Windows zum Abschluss neu starten.'
    'Neustart'
    'Fertig'
    'Setup fehlgeschlagen'
    'Windows Installer meldet einen Fehler.'
    'Protokoll ansehen'
    'Wirklich abbrechen?'
    'Vorherige Version'
    'Dateien in Verwendung'
    'Folgende Apps verwenden Dateien, die aktualisiert werden:'
    'Apps schließen.'
    'Apps schließen und neu starten.'
    'Apps offen lassen; Windows-Neustart nötig.'
    'Wiederholen'
    'Ignorieren'
    'Beenden'
    '<a href="#">Installationsprotokoll</a> anzeigen'
)

$installerLocaleStrings['ru-RU'] = @(
    'Установка OverInk'
    'Рисуйте поверх рабочего стола'
    'Подготовка установки…'
    'Установка OverInk'
    'Установка, восстановление или удаление OverInk.'
    'Закрыть'
    'Готово к установке'
    'Лёгкий инструмент для рисования при показе и обучении.'
    'Версия [WixBundleVersion]'
    'При обновлении папка установки сохранится.'
    'Параметры'
    'Установить'
    'Параметры установки'
    'Папка установки'
    'Обзор…'
    'Ярлык на рабочем столе'
    'Ярлык в меню «Пуск»'
    'Сохранить'
    'Назад'
    'OverInk установлен'
    'Вы можете восстановить или удалить OverInk.'
    'Восстановить'
    'Удалить'
    'Настройка…'
    'Установка OverInk…'
    'Отмена'
    'Операция завершена'
    'OverInk удалён'
    'OverInk готов к работе'
    'OverInk восстановлен'
    'Успешно завершено.'
    'Запустить OverInk'
    'Для завершения перезапустите Windows.'
    'Перезагрузить'
    'Готово'
    'Установка не завершена'
    'Ошибка Windows Installer.'
    'Открыть журнал'
    'Отменить установку?'
    'Предыдущая версия'
    'Файлы используются'
    'Эти приложения используют обновляемые файлы:'
    'Закрыть приложения.'
    'Закрыть и перезапустить приложения.'
    'Не закрывать; потребуется перезапуск Windows.'
    'Повторить'
    'Игнорировать'
    'Выход'
    'Открыть <a href="#">журнал установки</a>'
)
