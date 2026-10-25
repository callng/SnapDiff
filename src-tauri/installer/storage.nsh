; Database selection is a bootstrap for users without a saved location.
; Existing app preferences and all database files survive installer maintenance.
!include nsDialogs.nsh
!include LogicLib.nsh

!macro SNAPDIFF_STORAGE_PAGE
Var SDStorageMode
Var SDStoragePath
Var SDStorageDefault
Var SDStorageInstall
Var SDStorageCustom
Var SDStorageEdit
Var SDStorageBrowse

  Page custom SDStoragePage SDStorageLeave

Function SDStoragePage
  ${If} $PassiveMode = 1
    Abort
  ${EndIf}
  !insertmacro MUI_HEADER_TEXT "数据库保存位置" "选择快照元数据的保存位置"
  nsDialogs::Create 1018
  Pop $0
  ${If} $0 == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 0 0 100% 24u "已有应用的数据库位置保持不变，可在应用内的“数据库位置”中迁移。"
  Pop $0
  ${NSD_CreateRadioButton} 0 32u 100% 12u "系统应用数据目录（默认）"
  Pop $SDStorageDefault
  ${NSD_OnClick} $SDStorageDefault SDStorageSelection
  ${NSD_CreateRadioButton} 0 54u 100% 12u "安装目录下的 data 文件夹"
  Pop $SDStorageInstall
  ${NSD_OnClick} $SDStorageInstall SDStorageSelection
  ${NSD_CreateRadioButton} 0 76u 100% 12u "自选文件夹"
  Pop $SDStorageCustom
  ${NSD_OnClick} $SDStorageCustom SDStorageSelection
  ${NSD_CreateText} 0 96u 78% 14u "$SDStoragePath"
  Pop $SDStorageEdit
  ${NSD_CreateBrowseButton} 80% 96u 20% 14u "浏览…"
  Pop $SDStorageBrowse
  ${NSD_OnClick} $SDStorageBrowse SDStorageChoose
  ${NSD_CreateLabel} 0 120u 100% 24u "仅保存记录、设置和哈希。安装目录需可写，卸载不会删除数据库。"
  Pop $0
  ${If} $SDStorageMode == "install"
    ${NSD_Check} $SDStorageInstall
  ${ElseIf} $SDStorageMode == "custom"
    ${NSD_Check} $SDStorageCustom
  ${Else}
    StrCpy $SDStorageMode "default"
    ${NSD_Check} $SDStorageDefault
  ${EndIf}
  Call SDStorageControls
  nsDialogs::Show
FunctionEnd

Function SDStorageSelection
  Pop $0
  Call SDStorageControls
FunctionEnd

Function SDStorageControls
  ${NSD_GetState} $SDStorageCustom $1
  EnableWindow $SDStorageEdit $1
  EnableWindow $SDStorageBrowse $1
FunctionEnd

Function SDStorageChoose
  Pop $0
  nsDialogs::SelectFolderDialog "选择数据库保存文件夹" "$SDStoragePath"
  Pop $0
  ${If} $0 != error
    ${NSD_SetText} $SDStorageEdit $0
  ${EndIf}
FunctionEnd

Function SDStorageLeave
  ${NSD_GetState} $SDStorageInstall $0
  ${If} $0 == ${BST_CHECKED}
    StrCpy $SDStorageMode "install"
  ${Else}
    ${NSD_GetState} $SDStorageCustom $0
    ${If} $0 == ${BST_CHECKED}
      StrCpy $SDStorageMode "custom"
      ${NSD_GetText} $SDStorageEdit $SDStoragePath
      ${If} $SDStoragePath == ""
        MessageBox MB_OK|MB_ICONEXCLAMATION "请选择数据库保存文件夹。"
        Abort
      ${EndIf}
      ${GetRoot} "$SDStoragePath" $0
      ${If} $0 == ""
        MessageBox MB_OK|MB_ICONEXCLAMATION "请使用完整的绝对路径。"
        Abort
      ${EndIf}
    ${Else}
      StrCpy $SDStorageMode "default"
    ${EndIf}
  ${EndIf}
FunctionEnd

!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Silent/passive updates leave the original bootstrap untouched.
  ${If} $SDStorageMode != ""
    ClearErrors
    FileOpen $0 "$INSTDIR\snapdiff-storage.ini" w
    ${IfNot} ${Errors}
      FileWriteUTF16LE /BOM $0 "[storage]$\r$\nmode=$SDStorageMode$\r$\ndirectory=$SDStoragePath$\r$\n"
      FileClose $0
    ${EndIf}
    ${If} ${Errors}
      MessageBox MB_OK|MB_ICONEXCLAMATION "数据库位置设置未能保存，请启动应用后在“数据库位置”中选择。"
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  Delete "$INSTDIR\snapdiff-storage.ini"
  ; Never remove data/ or any database from an arbitrary custom path.
!macroend
