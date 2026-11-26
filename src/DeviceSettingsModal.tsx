import { useState } from 'react'
import { FormModal } from './FormModal'
import { InfoTip } from './InfoTip'
import type { AppText } from './i18n'
import type { Device } from './types'

const POINTER_SPEEDS = [0.5, 0.75, 1, 1.25, 1.5, 2]
const SCROLL_SPEEDS = [0.5, 1, 1.5, 2, 3]

export function DeviceSettingsModal({ device, ui, onClose, onSave, onRepair, onRemove }: {
  device: Device
  ui: AppText
  onClose: () => void
  onSave: (pointerSpeed: number, scrollSpeed: number, invertScroll: boolean) => Promise<void>
  onRepair: () => void
  onRemove: () => void
}) {
  const text = ui.devices
  const [pointerSpeed, setPointerSpeed] = useState(device.pointerSpeed ?? 1)
  const [scrollSpeed, setScrollSpeed] = useState(device.scrollSpeed ?? 1)
  const [invertScroll, setInvertScroll] = useState(device.invertScroll ?? false)
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)
  async function save() {
    setPending(true); setError(null)
    try { await onSave(pointerSpeed, scrollSpeed, invertScroll); onClose() }
    catch (error) { setError(error instanceof Error ? error.message : String(error)) }
    finally { setPending(false) }
  }
  return <FormModal title={text.settingsTitle} description={device.name} closeLabel={ui.errors.close} pending={pending} onClose={onClose}>
    <form onSubmit={event => { event.preventDefault(); void save() }}>
      <div className="modal-field-grid">
        <div className="modal-field"><div><label htmlFor="device-pointer-speed">{text.pointerSpeed}</label><InfoTip label={text.speedHelpLabel} text={text.speedHelp} /></div>
          <select id="device-pointer-speed" value={pointerSpeed} disabled={pending} onChange={event => setPointerSpeed(Number(event.target.value))}>
            {POINTER_SPEEDS.map(speed => <option key={speed} value={speed}>{speed}×{speed === 1 ? text.defaultSuffix : ''}</option>)}
          </select></div>
        <label className="modal-field"><span>{text.scrollSpeed}</span>
          <select value={scrollSpeed} disabled={pending} onChange={event => setScrollSpeed(Number(event.target.value))}>
            {SCROLL_SPEEDS.map(speed => <option key={speed} value={speed}>{speed}×{speed === 1 ? text.defaultSuffix : ''}</option>)}
          </select></label>
      </div>
      <div>
        <label className="modal-checkbox"><input type="checkbox" checked={invertScroll} disabled={pending} onChange={event => setInvertScroll(event.target.checked)} />
          <span>{text.invertScroll}</span></label>
        <p className="modal-help">{text.invertScrollHelp}</p>
      </div>
      <div className="modal-maintenance"><span>{text.maintenance}</span>
        <div><button type="button" className="secondary-button compact-button" disabled={pending} onClick={onRepair}>{text.repair}</button>
          <button type="button" className="secondary-button compact-button danger-button" disabled={pending} onClick={onRemove}>{text.removeDevice}</button></div>
      </div>
      {error ? <p className="modal-error" role="alert">{error}</p> : null}
      <div className="form-modal-actions"><button type="button" className="secondary-button" disabled={pending} onClick={onClose}>{ui.common.cancel}</button>
        <button type="submit" className="primary-button" disabled={pending}>{pending ? text.savingSettings : text.saveSettings}</button></div>
    </form>
  </FormModal>
}
