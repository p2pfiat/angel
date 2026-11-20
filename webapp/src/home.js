import qrcode from 'qrcode-generator'

const ua = navigator.userAgent || ''
const isIOS = /iPhone|iPad|iPod/.test(ua) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1)
const isAndroid = /Android/i.test(ua)
const isMobile = isIOS || isAndroid

const qrBox = document.getElementById('homeQrBox')
const qrHost = document.getElementById('homeAppQr')
const stepsIos = document.getElementById('stepsIos')
const stepsAndroid = document.getElementById('stepsAndroid')
const openBtn = document.getElementById('homeOpenApp')

if (qrHost) {
  const SVG_NS = 'http://www.w3.org/2000/svg'
  const appUrl = new URL('./app.html', location.href).href
  const qr = qrcode(0, 'M')
  qr.addData(appUrl)
  qr.make()
  const n = qr.getModuleCount()
  const quiet = 3
  const total = n + quiet * 2
  const parts = []
  for (let r = 0; r < n; r++) {
    for (let c = 0; c < n; c++) {
      if (qr.isDark(r, c)) parts.push(`M${c + quiet},${r + quiet}h1v1h-1z`)
    }
  }
  const svg = document.createElementNS(SVG_NS, 'svg')
  svg.setAttribute('viewBox', `0 0 ${total} ${total}`)
  svg.setAttribute('shape-rendering', 'crispEdges')
  svg.setAttribute('aria-hidden', 'true')
  const bg = document.createElementNS(SVG_NS, 'rect')
  bg.setAttribute('width', String(total))
  bg.setAttribute('height', String(total))
  bg.setAttribute('fill', '#ffffff')
  const fg = document.createElementNS(SVG_NS, 'path')
  fg.setAttribute('d', parts.join(''))
  fg.setAttribute('fill', '#202124')
  svg.append(bg, fg)
  qrHost.textContent = ''
  qrHost.appendChild(svg)
}

if (isMobile) {
  if (qrBox) qrBox.hidden = true
  if (stepsIos) stepsIos.hidden = !isIOS
  if (stepsAndroid) stepsAndroid.hidden = !isAndroid
  if (openBtn) {
    openBtn.textContent = 'Open & Install Sync'
    openBtn.className = 'btn btnPrimary'
  }
} else {
  if (qrBox) qrBox.hidden = false
  if (stepsIos) stepsIos.hidden = false
  if (stepsAndroid) stepsAndroid.hidden = false
  if (openBtn) {
    openBtn.textContent = 'Open in this browser'
    openBtn.className = 'btn btnOutline'
  }
}
