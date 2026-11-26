import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { isTauri } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import './index.css'
import App from './App.tsx'
import TransferToast from './TransferToast.tsx'

const nonPassiveCapture = { capture: true, passive: false } as AddEventListenerOptions

function preventDefaultZoomGesture(event: Event) {
  if (event.cancelable) {
    event.preventDefault()
  }
}

function preventWebviewPageZoom() {
  for (const eventName of ['gesturestart', 'gesturechange', 'gestureend']) {
    window.addEventListener(eventName, preventDefaultZoomGesture, nonPassiveCapture)
  }

  window.addEventListener(
    'wheel',
    (event) => {
      if (event.ctrlKey || event.metaKey) {
        preventDefaultZoomGesture(event)
      }
    },
    nonPassiveCapture,
  )

  window.addEventListener(
    'keydown',
    (event) => {
      if (!(event.metaKey || event.ctrlKey)) {
        return
      }
      if (
        ['=', '+', '-', '_', '0'].includes(event.key) ||
        ['Equal', 'Minus', 'Digit0', 'NumpadAdd', 'NumpadSubtract'].includes(event.code)
      ) {
        preventDefaultZoomGesture(event)
      }
    },
    nonPassiveCapture,
  )
}

preventWebviewPageZoom()

// The same page serves the pop-up that shows a transfer in the corner of the
// screen; the backend opens that window under this label.
const transferToast = isTauri() && getCurrentWindow().label === 'transfer-toast'
if (transferToast) {
  // Nothing of the page shows around the card (App.css).
  document.documentElement.dataset.window = 'transfer-toast'
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>{transferToast ? <TransferToast /> : <App />}</StrictMode>,
)
