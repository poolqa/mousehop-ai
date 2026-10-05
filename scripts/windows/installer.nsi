Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "WinVer.nsh"
!include "nsDialogs.nsh"
Var AllowPublic
Var PublicCheckbox
Name "Mousehop"
OutFile "${SETUP_OUTPUT}"
InstallDir "$PROGRAMFILES64\Mousehop"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
VIProductVersion "${APP_VERSION}.0"
VIAddVersionKey "ProductName" "Mousehop"
VIAddVersionKey "FileDescription" "Mousehop Windows Service Installer"
VIAddVersionKey "FileVersion" "${APP_VERSION}"
VIAddVersionKey "LegalCopyright" "Mousehop contributors (GPL-3.0-or-later)"
!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "../../LICENSE"
Page custom NetworkOptionsPage NetworkOptionsLeave
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_TEXT "Mousehop is installed as a Windows service.$\r$\n$\r$\nOpen Mousehop Settings from Start and approve administrator consent to pair trusted devices. Service pairing is separate from portable installations.$\r$\n$\r$\nTrusted peers can control the console login, lock and UAC desktops. Closing Settings does not stop sharing."
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "TradChinese"

!macro ServiceAction Action
    DetailPrint "Mousehop ${Action}: native Windows helper"
    ${If} ${Silent}
        StrCpy $0 "--silent"
    ${Else}
        StrCpy $0 ""
    ${EndIf}
    ${If} "${Action}" == "Install"
        ${If} $AllowPublic == 1
            StrCpy $0 "$0 --allow-public"
        ${EndIf}
    ${EndIf}
    nsExec::ExecToLog '"$PLUGINSDIR\mousehop-installer.exe" ${Action} $0'
    Pop $0
    ${If} $0 != 0
        MessageBox MB_ICONSTOP "Mousehop ${Action} failed. See installation details for the affected process, path or Windows error."
        SetErrorLevel 1
        Abort
    ${EndIf}
!macroend

Function .onInit
    ${IfNot} ${RunningX64}
        MessageBox MB_ICONSTOP "Mousehop requires 64-bit Windows."
        Abort
    ${EndIf}
    ${IfNot} ${AtLeastWin10}
        MessageBox MB_ICONSTOP "Mousehop requires Windows 10 or later."
        Abort
    ${EndIf}
    SetRegView 64
    SetShellVarContext all
    StrCpy $AllowPublic 0
    ReadRegDWORD $AllowPublic HKLM "Software\Mousehop\Installer" "AllowPublicNetwork"
    ${If} $AllowPublic != 1
        StrCpy $AllowPublic 0
    ${EndIf}
FunctionEnd

Function NetworkOptionsPage
    !insertmacro MUI_HEADER_TEXT "Network access" "Choose which network profiles allow incoming Mousehop connections."
    nsDialogs::Create 1018
    Pop $0
    ${If} $0 == error
        Abort
    ${EndIf}
    ${NSD_CreateLabel} 0 0 100% 42u "Mousehop allows incoming connections on Private and Domain networks from the local subnet. Trusted peers must also be authorized in Mousehop Settings."
    Pop $0
    ${NSD_CreateCheckbox} 0 48u 100% 28u "Also allow incoming connections on Public networks (local subnet only)"
    Pop $PublicCheckbox
    ${NSD_SetState} $PublicCheckbox $AllowPublic
    ${NSD_CreateLabel} 0 82u 100% 52u "Enable this only when you trust the local network. Windows can classify a home LAN as Public. This option changes only Mousehop's firewall rules; it does not turn off the firewall or change the network category."
    Pop $0
    nsDialogs::Show
FunctionEnd

Function NetworkOptionsLeave
    ${NSD_GetState} $PublicCheckbox $AllowPublic
FunctionEnd

Section "Mousehop"
    ; Never permit /D= to redirect a SYSTEM executable to user-writable storage.
    StrCpy $INSTDIR "$PROGRAMFILES64\Mousehop"
    InitPluginsDir
    SetOutPath "$PLUGINSDIR"
    File /oname=mousehop-installer.exe "${INSTALLER_HELPER}"
    !insertmacro ServiceAction Prepare
    SetOutPath "$INSTDIR"
    ClearErrors
    File "${PAYLOAD}/mousehop.exe"
    File "${PAYLOAD}/*.dll"
    File /oname=LICENSE.txt "../../LICENSE"
    FileOpen $0 "$INSTDIR\service-mode" w
    FileWrite $0 "Mousehop Windows Service installation$\r$\n"
    FileClose $0
    WriteUninstaller "$INSTDIR\uninstall.exe"
    ${If} ${Errors}
        MessageBox MB_ICONSTOP "Could not write Mousehop installation files. Close Mousehop and retry."
        SetErrorLevel 1
        Abort
    ${EndIf}
    ; Register removal before service startup, so a failed startup is recoverable.
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Mousehop" "DisplayName" "Mousehop"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Mousehop" "DisplayVersion" "${APP_VERSION}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Mousehop" "UninstallString" '$\"$INSTDIR\uninstall.exe$\"'
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Mousehop" "InstallLocation" "$INSTDIR"
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Mousehop" "NoModify" 1
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Mousehop" "NoRepair" 1
    CreateDirectory "$SMPROGRAMS\Mousehop"
    CreateShortcut "$SMPROGRAMS\Mousehop\Mousehop Settings.lnk" "$INSTDIR\mousehop.exe"
    CreateShortcut "$SMPROGRAMS\Mousehop\Uninstall Mousehop.lnk" "$INSTDIR\uninstall.exe"
    !insertmacro ServiceAction Install
    WriteRegDWORD HKLM "Software\Mousehop\Installer" "AllowPublicNetwork" $AllowPublic
SectionEnd

Function un.onInit
    SetRegView 64
    SetShellVarContext all
    StrCpy $INSTDIR "$PROGRAMFILES64\Mousehop"
FunctionEnd

Section "Uninstall"
    InitPluginsDir
    SetOutPath "$PLUGINSDIR"
    File /oname=mousehop-installer.exe "${INSTALLER_HELPER}"
    !insertmacro ServiceAction Uninstall
    Delete "$INSTDIR\mousehop.exe"
    Delete "$INSTDIR\*.dll"
    Delete "$INSTDIR\service-mode"
    Delete "$INSTDIR\LICENSE.txt"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"
    Delete "$SMPROGRAMS\Mousehop\Mousehop Settings.lnk"
    Delete "$SMPROGRAMS\Mousehop\Uninstall Mousehop.lnk"
    RMDir "$SMPROGRAMS\Mousehop"
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Mousehop"
    ; Deliberately preserve the machine identity/configuration in ProgramData.
SectionEnd
