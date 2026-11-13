// The floating icon. Click opens or hides the companion window; drag moves the icon unless
// it is locked. Locking and hiding are done from the tray menu or Settings, and the icon
// itself shows no text about either.

'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const orb = document.getElementById('orb');

function statusOf(snap) {
  if (snap.connection !== 'online' || !snap.state) return 'offline';
  const group = snap.state.group;
  if (group) {
    if (!group.you.accepted) return 'ready';
    if (group.phase === 'done') return 'done';
    if (group.phase === 'ready_check') return 'queued';
    return 'group';
  }
  return snap.state.queue ? 'queued' : 'idle';
}

function apply(snap) {
  orb.className = 'orb ' + statusOf(snap);
}

// A press that moves more than a few pixels is a drag; anything else is a click.
// Whether a drag is allowed is decided on the Rust side, which knows if the icon is locked.
let press = null;

orb.addEventListener('mousedown', (event) => {
  if (event.button === 0) press = { x: event.screenX, y: event.screenY };
});

window.addEventListener('mousemove', (event) => {
  if (!press) return;
  if (Math.hypot(event.screenX - press.x, event.screenY - press.y) > 4) {
    press = null;
    invoke('overlay_drag');
  }
});

window.addEventListener('mouseup', (event) => {
  if (event.button === 0 && press) {
    press = null;
    invoke('toggle_main');
  }
});

window.addEventListener('contextmenu', (event) => event.preventDefault());

listen('app-state', (event) => apply(event.payload));
invoke('get_snapshot').then(apply);
