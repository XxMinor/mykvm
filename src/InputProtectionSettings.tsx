import { useState } from 'react'
import type { InputProtection } from './types'
import type { InputProtectionStatus } from './runtime'


export function InputProtectionSettings({ settings, status, language, pending, onChange, onLock }: {
  settings: InputProtection
  status?: InputProtectionStatus
  language: 'cn' | 'en'
  pending: boolean
  onChange: (settings: InputProtection) => void
  onLock: (locked: boolean) => void
}) {
  const cn = language === 'cn'
  const [applications, setApplications] = useState<string | null>(null)
  const [hotkey, setHotkey] = useState<string | null>(null)
  const paused = settings.localOnly ? 'localOnly' : status?.reason
  const reason = paused === 'localOnly' ? (cn ? '本机已锁定' : 'Locked to this computer')
    : paused === 'fullscreen' ? (cn ? '全屏保护中' : 'Full-screen protection')
      : paused === 'application' ? (cn ? '应用保护中' : 'Application protection')
        : (cn ? '允许跨屏' : 'Screen switching allowed')
  return <section className="surface-card input-protection-card">
    <div className="card-title-row"><h2>{cn ? '游戏与防误切' : 'Gaming and screen protection'}</h2>
      <span className={`input-protection-status ${paused ? 'paused' : ''}`}>{reason}</span></div>
    <p className="muted-copy">{cn
      ? '锁定本机可暂停键鼠跨屏。全屏或名单应用在前台时，阻止新的跨屏进入；已在控制客户端时不会自动拉回。剪贴板同步仍由原开关控制。'
      : 'Lock mouse and keyboard to this computer. Full-screen or listed foreground apps block new screen switches without pulling back an existing remote session. Clipboard sharing uses its own switch.'}</p>
    <div className="settings-control-row"><span>{cn ? '锁定本机' : 'Lock to this computer'}</span>
      <div className="segmented-control">
        <button type="button" disabled={pending} className={settings.localOnly ? 'active' : ''} onClick={() => onLock(true)}>{cn ? '锁定' : 'Lock'}</button>
        <button type="button" disabled={pending} className={!settings.localOnly ? 'active' : ''} onClick={() => onLock(false)}>{cn ? '解锁' : 'Unlock'}</button>
      </div></div>
    <div className="settings-control-row"><label htmlFor="local-lock-hotkey">{cn ? '锁定／解锁快捷键' : 'Lock/unlock shortcut'}</label>
      <input id="local-lock-hotkey" className="input-protection-hotkey" value={hotkey ?? settings.lockHotkey} onChange={e => setHotkey(e.target.value)} onBlur={() => {
        if (hotkey !== null) { onChange({ ...settings, lockHotkey: hotkey || 'alt+shift+l' }); setHotkey(null) }
      }} /></div>
    <div className="settings-control-row"><span>{cn ? '全屏保护' : 'Full-screen protection'}</span>
      <div className="segmented-control">
        <button type="button" className={settings.protectFullscreen ? 'active' : ''} onClick={() => onChange({ ...settings, protectFullscreen: true })}>{cn ? '开启' : 'On'}</button>
        <button type="button" className={!settings.protectFullscreen ? 'active' : ''} onClick={() => onChange({ ...settings, protectFullscreen: false })}>{cn ? '关闭' : 'Off'}</button>
      </div></div>
    <label className="input-protection-apps" htmlFor="blocked-applications">{cn ? '禁止跨屏的应用' : 'Apps that block screen switching'}
      <textarea id="blocked-applications" rows={3} placeholder={'cs2.exe\ncom.valvesoftware.steam'} value={applications ?? settings.blockedApplications.join('\n')}
        onChange={e => setApplications(e.target.value)} onBlur={() => {
          if (applications !== null) { onChange({ ...settings, blockedApplications: applications.split(/[\n,]/).map(s => s.trim()).filter(Boolean) }); setApplications(null) }
        }} />
      <span className="muted-copy">{cn ? '每行一个程序名（Windows）或应用 bundle ID／名称（Mac）。离开这些应用后自动恢复跨屏。' : 'One executable name (Windows) or bundle ID/app name (Mac) per line. Switching resumes after leaving these apps.'}</span>
    </label>
  </section>
}
