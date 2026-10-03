; Standard Windows tools (cmd, tasklist, findstr, taskkill, sc) only: security
; software flagged the hidden `powershell -ExecutionPolicy Bypass` calls these
; hooks used to make.
Var MyKvmWaitTries
Var MyKvmExecResult

!macro MYKVM_CLOSE_RUNNING_INSTANCES
  DetailPrint "Closing running mykvm instances..."
  IfFileExists "$INSTDIR\mykvm.exe" 0 +2
    ExecWait '"$INSTDIR\mykvm.exe" --mykvm-quit-existing'
  ; The quit request returns at once while the app hands its network over and
  ; exits: wait up to 8 s for it to be gone, then end whatever is left.
  StrCpy $MyKvmWaitTries 0
  ${Do}
    nsExec::Exec '"$SYSDIR\cmd.exe" /c tasklist /FI "IMAGENAME eq mykvm.exe" /NH | findstr /I "mykvm.exe"'
    Pop $MyKvmExecResult
    ${If} $MyKvmExecResult != 0
      ${Break}
    ${EndIf}
    IntOp $MyKvmWaitTries $MyKvmWaitTries + 1
    ${If} $MyKvmWaitTries >= 40
      nsExec::Exec '"$SYSDIR\taskkill.exe" /F /IM mykvm.exe'
      Pop $MyKvmExecResult
      ${Break}
    ${EndIf}
    Sleep 200
  ${Loop}
  Sleep 300
!macroend

!macro MYKVM_FREE_INPUT_HELPER
  ; The input helper is run by the LocalSystem MyKVMInputService, which a per-user
  ; (non-elevated) installer cannot stop, so the .exe stays locked and a plain
  ; overwrite fails with "Error opening file for writing". Windows DOES allow
  ; RENAMING a running executable, so move the locked file aside to free its path:
  ; the new helper is then written normally and the service runs it on its next
  ; (re)start/reboot. The left-over copy is removed on the next update.
  ; No /REBOOTOK: a per-user installer cannot schedule a delete-on-reboot,
  ; yet the attempt sets the reboot flag, so the finish page asked to restart
  ; Windows instead of offering to run MyKVM.
  DetailPrint "Freeing input helper for replacement..."
  Delete "$INSTDIR\mykvm-input-helper.exe.old"
  IfFileExists "$INSTDIR\mykvm-input-helper.exe" 0 +2
    Rename "$INSTDIR\mykvm-input-helper.exe" "$INSTDIR\mykvm-input-helper.exe.old"
!macroend

!macro MYKVM_START_INPUT_SERVICE_IF_INSTALLED
  ; Make sure the service runs (a no-op when it already does or is not
  ; installed). It is not restarted: it runs its own copy of the helper in
  ; Program Files, which this per-user installer does not replace.
  DetailPrint "Starting MyKVM input service if installed..."
  nsExec::Exec '"$SYSDIR\sc.exe" start MyKVMInputService'
  Pop $MyKvmExecResult
!macroend

!macro MYKVM_DELETE_INPUT_SERVICE
  DetailPrint "Removing MyKVM input service..."
  nsExec::ExecToLog '"$SYSDIR\sc.exe" stop MyKVMInputService'
  nsExec::ExecToLog '"$SYSDIR\sc.exe" delete MyKVMInputService'
  Delete /REBOOTOK "$PROGRAMFILES64\MyKVM\mykvm-input-helper.exe"
  Delete /REBOOTOK "$PROGRAMFILES64\MyKVM\mykvm-input-helper.exe.installing"
  RMDir "$PROGRAMFILES64\MyKVM"
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; The input service is left running: it runs its own copy of the helper in
  ; Program Files, and it is what keeps the machine controllable while the app
  ; is being replaced. (Stopping it needs admin rights this per-user installer
  ; lacks; the attempt only waited 12 s.)
  !insertmacro MYKVM_FREE_INPUT_HELPER
  !insertmacro MYKVM_CLOSE_RUNNING_INSTANCES
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Allow inbound UDP to mykvm.exe so LAN peers can discover and reach this
  ; device. Best-effort: only succeeds when the installer runs elevated.
  DetailPrint "Configuring Windows Defender Firewall for mykvm..."
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="MyKVM (UDP-In)"'
  nsExec::ExecToLog 'netsh advfirewall firewall add rule name="MyKVM (UDP-In)" dir=in action=allow program="$INSTDIR\mykvm.exe" protocol=udp profile=any enable=yes'
  !insertmacro MYKVM_START_INPUT_SERVICE_IF_INSTALLED
  ; An interactive install starts MyKVM again as soon as its files are in
  ; place instead of at the finish page: a client controlled from another
  ; machine gets that control back for the remaining clicks. Update and
  ; passive installs are restarted by the installer's own /R.
  ${If} $PassiveMode <> 1
  ${AndIf} $UpdateMode <> 1
  ${AndIfNot} ${Silent}
    Call RunMainBinary
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; A new version's installer runs this uninstaller in place (_?=$INSTDIR,
  ; "uninstall before installing"); a real uninstall runs from a temp copy.
  ; Keep the input service across an upgrade: removing it needs admin rights,
  ; so the attempt failed, set the reboot flag and the installer finished by
  ; asking to restart Windows instead of starting MyKVM again.
  ${If} $EXEDIR != $INSTDIR
    !insertmacro MYKVM_DELETE_INPUT_SERVICE
  ${EndIf}
  !insertmacro MYKVM_CLOSE_RUNNING_INSTANCES
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Remove the firewall rule we added during install.
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="MyKVM (UDP-In)"'
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="MyKVM Headless Input (UDP-In)"'
!macroend
