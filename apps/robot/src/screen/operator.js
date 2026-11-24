const camera = document.querySelector('#camera');
const preview = document.querySelector('#preview');
const WATCHDOG_MS = 45000;
const RETRY_BASE_MS = 2000;
const RETRY_MAX_MS = 30000;
const MAX_ERRORS = 8;
let watching = false;
let authorized = true;
let attempt = 0;
let errors = 0;
let timer = null;

function clearTimer() {
  if (timer !== null) {
    clearTimeout(timer);
    timer = null;
  }
}

function stopCamera(label) {
  watching = false;
  errors = 0;
  clearTimer();
  camera.textContent = 'Start camera';
  preview.alt = label;
  preview.removeAttribute('src');
}

function connect() {
  clearTimer();
  if (!watching || !authorized || document.hidden) return;
  attempt += 1;
  preview.src = `/camera?t=${Date.now()}-${attempt}`;
  timer = setTimeout(connect, WATCHDOG_MS);
}

camera.onclick = () => {
  if (watching) {
    stopCamera('Camera off');
  } else if (authorized) {
    watching = true;
    errors = 0;
    camera.textContent = 'Stop camera';
    preview.alt = 'Starting camera';
    connect();
  }
};

preview.onload = () => {
  if (!watching) return;
  errors = 0;
  preview.alt = 'Camera preview';
};

preview.onerror = () => {
  if (!watching || !authorized || document.hidden) return;
  clearTimer();
  errors += 1;
  if (errors > MAX_ERRORS) {
    stopCamera('Camera unavailable');
    return;
  }
  const delay = Math.min(RETRY_BASE_MS * (2 ** (errors - 1)), RETRY_MAX_MS);
  timer = setTimeout(connect, delay);
};

document.addEventListener('visibilitychange', () => {
  if (!watching) return;
  if (document.hidden) {
    clearTimer();
    preview.removeAttribute('src');
  } else {
    errors = 0;
    connect();
  }
});

const output = document.querySelector('#events');
const match = document.querySelector('#recognized');
const events = new EventSource('/events');
events.onmessage = e => {
  output.textContent = (output.textContent + '\n' + e.data).split('\n').slice(-96).join('\n');
};
async function refreshMatch() {
  if (!document.hidden) {
    try {
      const response = await fetch('/recognized', {cache: 'no-store'});
      match.textContent = response.ok ? await response.text() : 'Unknown';
      if (response.status === 401 || response.status === 403) {
        authorized = false;
        stopCamera('Session expired');
        events.close();
        return;
      }
    } catch { match.textContent = 'Unknown'; }
  }
  setTimeout(refreshMatch, 2000);
}
setTimeout(refreshMatch, 2000);
