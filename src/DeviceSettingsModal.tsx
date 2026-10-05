import { useState } from 'react'
import { FormModal } from './FormModal'
import { InfoTip } from './InfoTip'
import type { Device } from './types'

const POINTER_SPEEDS = [0.5, 0.75, 1, 1.25, 1.5, 2]
const SCROLL_SPEEDS = [0.5, 1, 1.5, 2, 3]

export function DeviceSettingsModal({ device, language, onClose, onSave, onRepair, onRemove }: {
  device: Device
  language: 'cn' | 'en'
  onClose: () => void
  onSave: (pointerSpeed: number, scrollSpeed: number) => Promise<void>
  onRepair: () => void
  onRemove: () => void
}) {
  const cn = language === 'cn'
  const [pointerSpeed, setPointerSpeed] = useState(device.pointerSpeed ?? 1)
  const [scrollSpeed, setScrollSpeed] = useState(device.scrollSpeed ?? 1)
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)
  async function save() {
    setPending(true); setError(null)
    try { await onSave(pointerSpeed, scrollSpeed); onClose() }
    catch (error) { setError(error instanceof Error ? error.message : String(error)) }
    finally { setPending(false) }
  }
  return <FormModal title={cn ? '设备设置' : 'Device settings'} description={device.name} pending={pending} onClose={onClose}>
    <form onSubmit={event => { event.preventDefault(); void save() }}>
      <div className="modal-field-grid">
        <div className="modal-field"><div><label htmlFor="device-pointer-speed">{cn ? '指针速度' : 'Pointer speed'}</label><InfoTip label={cn ? '速度说明' : 'Speed help'} text={cn ? '1× 保持原始速度。' : '1× keeps the original speed.'} /></div>
          <select id="device-pointer-speed" value={pointerSpeed} disabled={pending} onChange={event => setPointerSpeed(Number(event.target.value))}>
            {POINTER_SPEEDS.map(speed => <option key={speed} value={speed}>{speed}×{speed === 1 ? (cn ? '（默认）' : ' (default)') : ''}</option>)}
          </select></div>
        <label className="modal-field"><span>{cn ? '滚轮速度' : 'Scroll speed'}</span>
          <select value={scrollSpeed} disabled={pending} onChange={event => setScrollSpeed(Number(event.target.value))}>
            {SCROLL_SPEEDS.map(speed => <option key={speed} value={speed}>{speed}×{speed === 1 ? (cn ? '（默认）' : ' (default)') : ''}</option>)}
          </select></label>
      </div>
      <div className="modal-maintenance"><span>{cn ? '连接维护' : 'Connection maintenance'}</span>
        <div><button type="button" className="secondary-button compact-button" disabled={pending} onClick={onRepair}>{cn ? '重新配对' : 'Re-pair'}</button>
          <button type="button" className="secondary-button compact-button danger-button" disabled={pending} onClick={onRemove}>{cn ? '移除设备' : 'Remove device'}</button></div>
      </div>
      {error ? <p className="modal-error" role="alert">{error}</p> : null}
      <div className="form-modal-actions"><button type="button" className="secondary-button" disabled={pending} onClick={onClose}>{cn ? '取消' : 'Cancel'}</button>
        <button type="submit" className="primary-button" disabled={pending}>{pending ? (cn ? '保存中…' : 'Saving…') : (cn ? '保存设置' : 'Save settings')}</button></div>
    </form>
  </FormModal>
}
