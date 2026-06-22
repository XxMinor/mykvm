!macro MYKVM_CLOSE_RUNNING_INSTANCES
  DetailPrint "Closing running mykvm instances..."
  IfFileExists "$INSTDIR\mykvm.exe" 0 +2
    ExecWait '"$INSTDIR\mykvm.exe" --mykvm-quit-existing'
  nsExec::ExecToLog 'powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -Command "$deadline=(Get-Date).AddSeconds(8); while ((Get-Process -Name mykvm -ErrorAction SilentlyContinue) -and ((Get-Date) -lt $deadline)) { Start-Sleep -Milliseconds 200 }; Get-Process -Name mykvm -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue"'
  Sleep 300
!macroend

!macro MYKVM_STOP_INPUT_SERVICE
  DetailPrint "Stopping MyKVM input service..."
  nsExec::ExecToLog 'powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -Command "$svc=Get-Service -Name ''MyKVMInputService'' -ErrorAction SilentlyContinue; if ($svc) { Stop-Service -Name ''MyKVMInputService'' -Force -ErrorAction SilentlyContinue; $deadline=(Get-Date).AddSeconds(12); while (((Get-Service -Name ''MyKVMInputService'' -ErrorAction SilentlyContinue).Status -ne ''Stopped'') -and ((Get-Date) -lt $deadline)) { Start-Sleep -Milliseconds 250 } }; Get-Process -Name ''mykvm-input-helper'' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 400"'
!macroend

!macro MYKVM_START_INPUT_SERVICE_IF_INSTALLED
  DetailPrint "Starting MyKVM input service if installed..."
  nsExec::ExecToLog 'powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -Command "$svc=Get-Service -Name ''MyKVMInputService'' -ErrorAction SilentlyContinue; if ($svc) { Start-Service -Name ''MyKVMInputService'' -ErrorAction SilentlyContinue }"'
!macroend

!macro MYKVM_DELETE_INPUT_SERVICE
  DetailPrint "Removing MyKVM input service..."
  nsExec::ExecToLog 'powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -Command "$svc=Get-Service -Name ''MyKVMInputService'' -ErrorAction SilentlyContinue; if ($svc) { Stop-Service -Name ''MyKVMInputService'' -Force -ErrorAction SilentlyContinue }; sc.exe delete MyKVMInputService"'
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro MYKVM_STOP_INPUT_SERVICE
  !insertmacro MYKVM_CLOSE_RUNNING_INSTANCES
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Allow inbound UDP to mykvm.exe so LAN peers can discover and reach this
  ; device. Best-effort: only succeeds when the installer runs elevated.
  DetailPrint "Configuring Windows Defender Firewall for mykvm..."
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="MyKVM (UDP-In)"'
  nsExec::ExecToLog 'netsh advfirewall firewall add rule name="MyKVM (UDP-In)" dir=in action=allow program="$INSTDIR\mykvm.exe" protocol=udp profile=any enable=yes'
  !insertmacro MYKVM_START_INPUT_SERVICE_IF_INSTALLED
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro MYKVM_DELETE_INPUT_SERVICE
  !insertmacro MYKVM_CLOSE_RUNNING_INSTANCES
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Remove the firewall rule we added during install.
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="MyKVM (UDP-In)"'
!macroend
