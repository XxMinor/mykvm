import { useId } from 'react'

export function InfoTip({ label, text }: { label: string; text: string }) {
  const id = useId()
  return <button type="button" className="info-tooltip-host" aria-label={label} aria-describedby={id}>
    <span aria-hidden="true">?</span>
    <span className="info-tooltip" id={id} role="tooltip">{text}</span>
  </button>
}

export function HelpHeading({ label, text }: { label: string; text: string }) {
  return <div className="help-heading"><h2>{label}</h2><InfoTip label={label} text={text} /></div>
}
