; ============================================================================
; Blackhole IME - NSIS 安装脚本（cargo-packager 自定义模板）
;
; 配合 Packager.toml 使用，扩展标准模板：
;   1. 安装后自动注册 IME DLL（regsvr32）
;   2. 卸载前自动取消注册 IME DLL（regsvr32 /u）
;   3. 清理残留注册表项
;
; 使用 staging 目录方案：所有文件由 before-packaging-command
; 汇集到 target/packaging/staging/，模板只需 File /r 复制。
; ============================================================================

Unicode true
ManifestDPIAware true

; --- 产品信息 ---
!define PRODUCT_NAME     "BlackholeIME"
!define PRODUCT_VERSION  "0.1.0"
!define PRODUCT_PUBLISHER "Blackhole IME Team"
!define MAIN_EXE         "blackhole.exe"
!define IME_DLL          "blackhole_platform.dll"

Name "${PRODUCT_NAME} ${PRODUCT_VERSION}"
OutFile "nsis-output.exe"
InstallDir "$PROGRAMFILES64\${PRODUCT_NAME}"
RequestExecutionLevel admin
SetCompressor lzma

; --- Modern UI 2 ---
!include "MUI2.nsh"
!include "LogicLib.nsh"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

; ============================================================================
; 安装前初始化
; ============================================================================
Function .onInit
    ; 检查已安装版本
    ReadRegStr $R0 HKLM "Software\${PRODUCT_PUBLISHER}\${PRODUCT_NAME}" "InstallDir"
    ${If} $R0 != ""
        ReadRegStr $R1 HKLM "Software\${PRODUCT_PUBLISHER}\${PRODUCT_NAME}" "Version"
        MessageBox MB_YESNO|MB_ICONQUESTION \
            "检测到 ${PRODUCT_NAME} $R1 已安装在:$\n$R0$\n$\n是否覆盖安装？" \
            IDYES +2
        Quit
    ${EndIf}

    ; 尝试关闭正在运行的进程
    nsExec::ExecToStack 'taskkill /f /im ${MAIN_EXE}'
    Pop $0
    Sleep 500
FunctionEnd

; ============================================================================
; 安装区段
; ============================================================================
Section "Install"
    SetOutPath "$INSTDIR"

    ; --- 从 staging 目录复制所有文件 ---
    ; NSIS File 命令路径相对于 .nsi 脚本所在目录。
    ; cargo-packager 将脚本生成在 target/{profile}/.cargo-packager/nsis/x64/，
    ; staging 目录在 target/packaging/staging/，需向上 4 级。
    File /r "..\..\..\..\packaging\staging\*"

    ; --- 写入卸载程序 ---
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ; --- 注册表：卸载信息 ---
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "DisplayName" "${PRODUCT_NAME} ${PRODUCT_VERSION}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "DisplayIcon" '"$INSTDIR\${MAIN_EXE}"'
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "DisplayVersion" "${PRODUCT_VERSION}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "Publisher" "${PRODUCT_PUBLISHER}"
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "NoModify" 1
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "NoRepair" 1

    ; --- 记录安装信息 ---
    WriteRegStr HKLM "Software\${PRODUCT_PUBLISHER}\${PRODUCT_NAME}" "InstallDir" "$INSTDIR"
    WriteRegStr HKLM "Software\${PRODUCT_PUBLISHER}\${PRODUCT_NAME}" "Version" "${PRODUCT_VERSION}"

    ; --- ★ 注册 IME DLL ---
    DetailPrint "正在注册输入法组件..."
    nsExec::ExecToStack 'regsvr32 /s "$INSTDIR\${IME_DLL}"'
    Pop $0
    ${If} $0 != 0
        MessageBox MB_ICONEXCLAMATION \
            "输入法组件注册失败 (错误码: $0)。$\n$\n请以管理员身份重新运行安装程序，或手动执行:$\nregsvr32 $\"$INSTDIR\${IME_DLL}$\"" \
            /SD IDOK
    ${Else}
        DetailPrint "输入法组件注册成功"
    ${EndIf}

    ; --- 刷新输入法列表 ---
    DetailPrint "正在刷新输入法列表..."
    nsExec::ExecToStack 'taskkill /f /im ctfmon.exe'
    Sleep 1000
    nsExec::ExecToStack '"$SYSDIR\ctfmon.exe"'

SectionEnd

; --- 开始菜单快捷方式 ---
Section "Shortcuts"
    CreateDirectory "$SMPROGRAMS\${PRODUCT_NAME}"
    CreateShortcut "$SMPROGRAMS\${PRODUCT_NAME}\${PRODUCT_NAME}.lnk" \
        "$INSTDIR\${MAIN_EXE}" "" "$INSTDIR\${MAIN_EXE}" 0
    CreateShortcut "$SMPROGRAMS\${PRODUCT_NAME}\卸载 ${PRODUCT_NAME}.lnk" \
        "$INSTDIR\uninstall.exe"
SectionEnd

; ============================================================================
; ★ 卸载区段（核心：卸载前取消注册 IME）
; ============================================================================
Section "Uninstall"

    ; --- ★ 1. 取消注册 IME DLL（必须在删除文件之前执行） ---
    DetailPrint "正在取消注册输入法组件..."
    nsExec::ExecToStack 'regsvr32 /u /s "$INSTDIR\${IME_DLL}"'
    Pop $0
    ${If} $0 != 0
        MessageBox MB_ICONEXCLAMATION \
            "取消注册输入法组件失败 (错误码: $0)。$\n$\n将继续卸载，但可能残留注册表项。" \
            /SD IDOK
    ${Else}
        DetailPrint "输入法组件已取消注册"
    ${EndIf}

    ; --- ★ 2. 清理可能残留的注册表项 ---
    DetailPrint "正在清理残留注册表项..."
    !define IME_CLSID "{A1B2C3D4-E5F6-7890-1234-567890ABCDEF}"
    DeleteRegKey HKLM "SOFTWARE\Microsoft\CTF\TIP\${IME_CLSID}"
    DeleteRegKey HKLM "SOFTWARE\WOW6432Node\Microsoft\CTF\TIP\${IME_CLSID}"
    DeleteRegKey HKCR "CLSID\${IME_CLSID}"
    DeleteRegKey HKCU "SOFTWARE\Microsoft\CTF\TIP\${IME_CLSID}"
    !undef IME_CLSID

    ; --- 3. 停止进程 ---
    nsExec::ExecToStack 'taskkill /f /im ${MAIN_EXE}'
    Sleep 1000

    ; --- 4. 删除所有文件 ---
    Delete "$INSTDIR\${MAIN_EXE}"
    Delete "$INSTDIR\${IME_DLL}"
    Delete "$INSTDIR\uninstall.exe"
    RMDir /r "$INSTDIR\dicts"
    RMDir "$INSTDIR"

    ; --- 5. 删除快捷方式 ---
    Delete "$SMPROGRAMS\${PRODUCT_NAME}\${PRODUCT_NAME}.lnk"
    Delete "$SMPROGRAMS\${PRODUCT_NAME}\卸载 ${PRODUCT_NAME}.lnk"
    RMDir "$SMPROGRAMS\${PRODUCT_NAME}"
    Delete "$DESKTOP\${PRODUCT_NAME}.lnk"

    ; --- 6. 删除注册表 ---
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}"
    DeleteRegKey HKLM "Software\${PRODUCT_PUBLISHER}\${PRODUCT_NAME}"

    ; --- 7. 刷新输入法列表 ---
    nsExec::ExecToStack 'taskkill /f /im ctfmon.exe'
    Sleep 500
    nsExec::ExecToStack '"$SYSDIR\ctfmon.exe"'

SectionEnd

; ============================================================================
; 安装/卸载完成回调
; ============================================================================
Function .onInstSuccess
    MessageBox MB_YESNO|MB_ICONINFORMATION \
        "${PRODUCT_NAME} 安装完成！$\n$\n是否立即启动？" \
        IDYES launch IDNO done
launch:
    Exec '"$INSTDIR\${MAIN_EXE}"'
done:
FunctionEnd

Function un.onUninstSuccess
    MessageBox MB_OK|MB_ICONINFORMATION \
        "${PRODUCT_NAME} 已成功卸载。$\n$\n建议重启计算机以完全清除输入法列表。" \
        /SD IDOK
FunctionEnd
