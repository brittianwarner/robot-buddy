const camera = document.querySelector('#camera');
const preview = document.querySelector('#preview');
let watching = false;
function connect() { if (watching) preview.src = '/camera'; }
camera.onclick = () => {
  watching = !watching;
  camera.textContent = watching ? 'Stop camera' : 'Start camera';
  if (watching) connect(); else preview.removeAttribute('src');
};
preview.onerror = () => { if (watching) setTimeout(connect, 2000); };
const output = document.querySelector('#events');
const match = document.querySelector('#recognized');
async function refreshMatch() {
  if (!document.hidden) {
    try {
      const response = await fetch('/recognized', {cache: 'no-store'});
      match.textContent = response.ok ? await response.text() : 'Unknown';
      if (response.status === 401 || response.status === 403) return;
    } catch { match.textContent = 'Unknown'; }
  }
  setTimeout(refreshMatch, 2000);
}
setTimeout(refreshMatch, 2000);
const events = new EventSource('/events');
events.onmessage = e => {
  output.textContent = (output.textContent + '\n' + e.data).split('\n').slice(-96).join('\n');
};
