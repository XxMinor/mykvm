import { useEffect, useId, useRef, type ReactNode } from 'react'

export function FormModal({ title, description, pending = false, onClose, children }: {
  title: string
  description?: string
  pending?: boolean
  onClose: () => void
  children: ReactNode
}) {
  const dialog = useRef<HTMLDialogElement>(null)
  const titleId = useId()
  const descriptionId = useId()
  useEffect(() => {
    const element = dialog.current
    element?.showModal()
    return () => element?.close()
  }, [])
  return <dialog ref={dialog} className="form-modal" aria-labelledby={titleId}
    aria-describedby={description ? descriptionId : undefined}
    onCancel={event => { event.preventDefault(); if (!pending) onClose() }}>
    <div className="form-modal-heading">
      <div><h2 id={titleId}>{title}</h2>{description ? <p id={descriptionId}>{description}</p> : null}</div>
      <button type="button" className="form-modal-close" aria-label="Close" disabled={pending} onClick={onClose}>×</button>
    </div>
    {children}
  </dialog>
}
