const LOGOS = ['/logos/color-1.png', '/logos/color-2.png', '/logos/color-3.png', '/logos/color-4.png']
const ALERT_INTERVAL_MS = 1000

let timer: ReturnType<typeof setInterval> | null = null
let idx = 0

function iconLink(): HTMLLinkElement | null {
  return document.querySelector<HTMLLinkElement>('head link[rel="icon"]')
}

function setIcon(i: number) {
  const link = iconLink()
  if (link) link.href = `${LOGOS[i]}?v=${i}`
}

/** Cycle the favicon through the colour logos every second (pending decision alert). */
export function startFaviconAlert() {
  if (timer !== null) return
  timer = setInterval(() => {
    idx = (idx + 1) % LOGOS.length
    setIcon(idx)
  }, ALERT_INTERVAL_MS)
}

/** Stop cycling and restore the default favicon. */
export function stopFaviconAlert() {
  if (timer !== null) {
    clearInterval(timer)
    timer = null
  }
  idx = 0
  setIcon(0)
}
