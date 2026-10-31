; Installer hooks for the .exe (NSIS) build. Wired in through
; bundle.windows.nsis.installerHooks in tauri.conf.json.
;
; productName stays "PrintVault" for good: it names the uninstall registry key
; the installer finds an existing copy by, and on Linux the deb and rpm package
; names, so changing it would install a second copy beside the first. See the
; productName decision in the project notes.
;
; What people actually read can still say Printery, because none of it is used
; to find anything. This relabels three things after every install:
;   - the Start menu shortcut
;   - the desktop shortcut
;   - the name shown in Installed apps / Add or remove programs
; The registry key keeps its PrintVault name. Only its DisplayName changes, and
; the template never reads DisplayName back for its own install. Its one read
; of that value is to spot an .msi install, which carries its own name and is
; untouched by any of this.
;
; Checked against the template for tauri-cli 2.11.4:
;   - An update made by the auto-updater never recreates shortcuts, it only
;     repoints ones that exist. So once a shortcut is renamed it stays renamed.
;   - A fresh install creates "PrintVault.lnk" and the hook renames it straight
;     after, so nobody ever sees the old name.
;   - An upgrade runs the previous version's uninstaller first, with /UPDATE.
;     The template skips removing shortcuts in that mode, but the POSTUNINSTALL
;     hook runs either way. Removing Printery.lnk there without checking
;     $UpdateMode would delete everyone's Start menu entry on their next
;     update, and nothing would put it back. So it checks.
;
; Every rename and delete goes through IsShortcutTarget, so only a shortcut
; that really points at this app's exe is touched. A file someone happens to
; have called PrintVault.lnk or Printery.lnk for another reason is left alone.
;
; The .msi build is not covered. Its shortcuts come from the WiX template, and
; renaming them means forking that template and carrying it across Tauri
; upgrades, which is not worth it for a label.

!define PV_DISPLAYNAME "Printery"

!macro PV_RENAME_SHORTCUT folder
  !insertmacro IsShortcutTarget "${folder}\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  Pop $0
  ${If} $0 = 1
    ; A previous install may already have left a Printery.lnk. Rename will not
    ; overwrite, so clear it first, but only if it is ours too.
    !insertmacro IsShortcutTarget "${folder}\${PV_DISPLAYNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    Pop $1
    ${If} $1 = 1
      Delete "${folder}\${PV_DISPLAYNAME}.lnk"
    ${EndIf}
    Rename "${folder}\${PRODUCTNAME}.lnk" "${folder}\${PV_DISPLAYNAME}.lnk"
  ${EndIf}
!macroend

!macro PV_REMOVE_SHORTCUT folder
  !insertmacro IsShortcutTarget "${folder}\${PV_DISPLAYNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  Pop $0
  ${If} $0 = 1
    !insertmacro UnpinShortcut "${folder}\${PV_DISPLAYNAME}.lnk"
    Delete "${folder}\${PV_DISPLAYNAME}.lnk"
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayName" "${PV_DISPLAYNAME}"
  !insertmacro PV_RENAME_SHORTCUT "$SMPROGRAMS"
  !insertmacro PV_RENAME_SHORTCUT "$DESKTOP"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Only on a real uninstall. During an update this would remove the shortcut
  ; and the new version would never make another one.
  ${If} $UpdateMode <> 1
    !insertmacro PV_REMOVE_SHORTCUT "$SMPROGRAMS"
    !insertmacro PV_REMOVE_SHORTCUT "$DESKTOP"
  ${EndIf}
!macroend
