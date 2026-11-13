// Main companion window. Renders from the snapshot the Rust side sends, and builds the
// protocol messages (see server/src/protocol.ts) that go back through send_message.

'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const ROLES = ['tank', 'healer', 'dps'];
const ROLE_LABEL = { tank: 'Tank', healer: 'Healer', dps: 'DPS' };
// Role icons: a shield, a cross and a sword, drawn for this app.
const ROLE_ICON = {
  tank: 'M12 2l8 3v6c0 5-3.4 9.4-8 11-4.6-1.6-8-6-8-11V5l8-3z',
  healer: 'M9.5 3h5v6.5H21v5h-6.5V21h-5v-6.5H3v-5h6.5V3z',
  dps: 'M18 3h3v3L11 16l-3-3L18 3zM5.3 11.3l1.4-1.4 7.4 7.4-1.4 1.4-7.4-7.4zM7.6 15l1.4 1.4-3.2 3.2a1.6 1.6 0 1 1-1.4-1.4L7.6 15z',
};
// The colours players already associate with each class.
const CLASS_COLOR = {
  warrior: '#c79c6e', paladin: '#f58cba', hunter: '#abd473', rogue: '#fff569', priest: '#ffffff',
  shaman: '#3b9bff', mage: '#69ccf0', warlock: '#9482c9', druid: '#ff7d0a',
};
function classStyle(cls) {
  const color = CLASS_COLOR[String(cls || '').toLowerCase()];
  return color ? 'color:' + color : null;
}
const CLASSES = ['Warrior', 'Paladin', 'Hunter', 'Rogue', 'Priest', 'Shaman', 'Mage', 'Warlock', 'Druid'];
const STALE_SECONDS = 2 * 60 * 60;

let snap = { connection: 'connecting', detail: null, welcome: null, state: null, stats: null, update: null, version: null, settings: {} };
let clockOffset = 0; // server time minus local time, in ms
let view = null; // 'settings' or 'character' when the player opened one of those; null follows the server state
let addonChars = [];
let leaveDialog = false;
let copied = false;
let alarmTimer = null;

// -- small helpers ----------------------------------------------------------------

function load(key, fallback) {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : JSON.parse(raw);
  } catch {
    return fallback;
  }
}

function save(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Preferences are a convenience; carry on without them.
  }
}

const prefs = load('prefs', { instances: [], roles: ['dps'], willLead: false, region: null, ruleset: null, manual: null });
let ratings = load('ratings', {}); // { groupId: { memberId: 1 | -1 } }

function h(tag, props, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'class') node.className = value;
    else if (key === 'text') node.textContent = value;
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else if (key === 'value') node.value = value;
    else if (key === 'checked' || key === 'disabled' || key === 'selected' || key === 'readOnly') node[key] = Boolean(value);
    else node.setAttribute(key, value === true ? '' : String(value));
  }
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

function svg(path, size = 16) {
  const el = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  el.setAttribute('viewBox', '0 0 24 24');
  el.setAttribute('width', size);
  el.setAttribute('height', size);
  el.setAttribute('aria-hidden', 'true');
  const p = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  p.setAttribute('d', path);
  p.setAttribute('fill', 'currentColor');
  el.append(p);
  return el;
}

const THUMB_UP = 'M2 21h4V9H2v12zm20-11a2 2 0 0 0-2-2h-6.3l1-4.6v-.3a1.5 1.5 0 0 0-.5-1L13.2 1 6.6 7.6A2 2 0 0 0 6 9v10a2 2 0 0 0 2 2h9a2 2 0 0 0 1.8-1.2l3-7.1A2 2 0 0 0 22 12v-2z';
const THUMB_DOWN = 'M22 3h-4v12h4V3zM2 14a2 2 0 0 0 2 2h6.3l-1 4.6v.3c0 .4.2.8.5 1l1 1.1 6.6-6.6A2 2 0 0 0 18 15V5a2 2 0 0 0-2-2H7a2 2 0 0 0-1.8 1.2l-3 7.1A2 2 0 0 0 2 12v2z';

function serverNow() {
  return Date.now() + clockOffset;
}

function clock(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const m = Math.floor(s / 60);
  return String(m).padStart(2, '0') + ':' + String(s % 60).padStart(2, '0');
}

function ageText(seconds) {
  if (seconds < 90) return 'just now';
  if (seconds < 3600) return Math.round(seconds / 60) + ' minutes ago';
  if (seconds < 86400 * 2) return Math.round(seconds / 3600) + ' hours ago';
  return Math.round(seconds / 86400) + ' days ago';
}

function instanceName(id) {
  const found = (snap.welcome?.instances || []).find((i) => i.id === id);
  return found ? found.name : id;
}

// "~4 min" style text for an average wait in seconds, or null when there is no data.
function waitText(seconds) {
  if (seconds === null || seconds === undefined) return null;
  if (seconds < 60) return 'under a minute';
  const minutes = Math.round(seconds / 60);
  return minutes < 60 ? '~' + minutes + ' min' : '~' + Math.round(minutes / 60) + ' h';
}

// One line under an instance: who is queued for it and how long the chosen roles usually wait.
function instanceMeta(id, roles) {
  const stats = snap.stats?.instances?.[id];
  if (!stats) return '';
  const queued = ROLES.filter((r) => stats.queued[r] > 0).map((r) => stats.queued[r] + ' ' + ROLE_LABEL[r]);
  const waits = roles.filter((r) => stats.avgWaitSeconds[r] !== null).map((r) => ROLE_LABEL[r] + ' ' + waitText(stats.avgWaitSeconds[r]));
  return [queued.length ? 'In queue: ' + queued.join(', ') : '', waits.length ? 'Usual wait: ' + waits.join(', ') : ''].filter(Boolean).join(' · ');
}

// "12 online · 3 in queue" for the player's own world. Totals only; the server sends no names.
function totalsLine() {
  const stats = snap.stats;
  if (!stats) return null;
  return h('p', { class: 'muted small center totals', text: stats.online + ' online · ' + stats.queued + ' in queue in your world' });
}

// The shortest usual wait across the instances and roles a player is queued for.
function bestWait(queue) {
  let best = null;
  for (const id of queue.instances) {
    const stats = snap.stats?.instances?.[id];
    if (!stats) continue;
    for (const role of queue.roles) {
      const seconds = stats.avgWaitSeconds[role];
      if (seconds !== null && (best === null || seconds < best)) best = seconds;
    }
  }
  return best;
}

function toast(text, isError) {
  const node = h('div', { class: 'toast' + (isError ? ' error' : ''), text });
  document.getElementById('toasts').append(node);
  setTimeout(() => node.remove(), isError ? 8000 : 6000);
}

async function send(message) {
  try {
    await invoke('send_message', { message });
  } catch (err) {
    toast(String(err), true);
  }
}

// -- ready check alarm ---------------------------------------------------------------

let audio = null;

function beep() {
  try {
    audio = audio || new AudioContext();
    audio.resume();
    const t = audio.currentTime;
    for (const [offset, freq] of [[0, 660], [0.18, 880]]) {
      const osc = audio.createOscillator();
      const gain = audio.createGain();
      osc.frequency.value = freq;
      gain.gain.setValueAtTime(0.0001, t + offset);
      gain.gain.exponentialRampToValueAtTime(0.25, t + offset + 0.02);
      gain.gain.exponentialRampToValueAtTime(0.0001, t + offset + 0.16);
      osc.connect(gain).connect(audio.destination);
      osc.start(t + offset);
      osc.stop(t + offset + 0.18);
    }
  } catch {
    // No sound device; the window still comes to the front.
  }
}

function setAlarm(on) {
  if (on && !alarmTimer) {
    beep();
    alarmTimer = setInterval(beep, 2500);
  } else if (!on && alarmTimer) {
    clearInterval(alarmTimer);
    alarmTimer = null;
  }
}

// -- character view --------------------------------------------------------------------

function worldPickers() {
  const regions = snap.welcome?.regions || [];
  const rulesets = snap.welcome?.rulesets || [];
  if (!regions.includes(prefs.region)) prefs.region = regions[0] || null;
  if (!rulesets.includes(prefs.ruleset)) prefs.ruleset = rulesets[0] || null;
  const pick = (id, label, items, key) =>
    h('div', {},
      h('label', { for: id, text: label }),
      h('select', { id, onchange: (e) => { prefs[key] = e.target.value; save('prefs', prefs); } },
        items.map((item) => h('option', { value: item, selected: item === prefs[key], text: item }))));
  return h('div', { class: 'grid2' }, pick('region', 'Region', regions, 'region'), pick('ruleset', 'Ruleset', rulesets, 'ruleset'));
}

function chooseCharacter(c, fromAddon) {
  send({
    type: 'set_character',
    character: {
      name: c.name,
      region: prefs.region,
      ruleset: prefs.ruleset,
      faction: c.faction,
      class: c.class,
      level: c.level,
      spec: c.spec || null,
      ilvl: c.ilvl || null,
      snapshotAt: fromAddon ? c.snapshotAt || null : null,
      addon: fromAddon,
    },
  });
  view = null;
}

async function refreshAddonCharacters() {
  try {
    addonChars = await invoke('read_characters');
  } catch {
    addonChars = [];
  }
  render(true);
}

function characterView() {
  const current = snap.state?.character;
  const manual = prefs.manual || { name: '', class: CLASSES[0], level: 1, faction: 'horde', spec: '', ilvl: '' };
  const field = (id, label, input) => h('div', {}, h('label', { for: id, text: label }), input);
  const now = serverNow() / 1000;

  const addonList = addonChars.length
    ? addonChars.map((c) =>
        h('div', { class: 'member' },
          h('div', { class: 'info' },
            h('div', { class: 'line1' }, h('span', { class: 'name', style: classStyle(c.class), text: c.name }), h('span', { class: 'pill', text: c.faction })),
            h('div', { class: 'line2', text: `Level ${c.level} ${c.class}${c.ilvl ? ' · item level ' + c.ilvl : ''} · saved ${c.snapshotAt ? ageText(now - c.snapshotAt) : 'at an unknown time'}` })),
          h('button', { class: 'primary small', text: 'Use', onclick: () => chooseCharacter(c, true) })))
    : h('p', { class: 'muted small', text: snap.settings.wowDir
        ? 'No characters found yet. Install the Anti Social addon, log in, then type /reload in game and press Refresh.'
        : 'Set your World of Warcraft folder in Settings so the companion can read your characters from the addon.' });

  return [
    h('div', { class: 'row' }, h('h2', { class: 'grow', text: 'Choose your character' }),
      current ? h('button', { class: 'ghost small', text: 'Back', onclick: () => { view = null; render(); } }) : null),
    h('div', { class: 'card stack' }, h('h3', { text: 'Your world' }), worldPickers()),
    h('div', { class: 'card stack' },
      h('div', { class: 'row' }, h('h3', { class: 'grow', text: 'From the addon' }), h('button', { class: 'ghost small', text: 'Refresh', onclick: refreshAddonCharacters })),
      addonList),
    h('div', { class: 'card stack' },
      h('h3', { text: 'Or type it in' }),
      h('p', { class: 'muted small', text: 'Others will see these stats marked as typed in.' }),
      field('m-name', 'Character name', h('input', { id: 'm-name', value: manual.name, maxlength: 48, autocomplete: 'off' })),
      h('div', { class: 'grid2' },
        field('m-class', 'Class', h('select', { id: 'm-class' }, CLASSES.map((c) => h('option', { value: c, selected: c === manual.class, text: c })))),
        field('m-faction', 'Faction', h('select', { id: 'm-faction' }, ['horde', 'alliance'].map((f) => h('option', { value: f, selected: f === manual.faction, text: f })))),
        field('m-level', 'Level', h('input', { id: 'm-level', type: 'number', min: 1, max: snap.welcome?.limits?.maxLevel || 60, value: manual.level })),
        field('m-ilvl', 'Item level (optional)', h('input', { id: 'm-ilvl', type: 'number', min: 0, value: manual.ilvl }))),
      h('button', {
        class: 'primary',
        text: 'Use this character',
        onclick: () => {
          const get = (id) => document.getElementById(id).value.trim();
          const entry = { name: get('m-name'), class: get('m-class'), faction: get('m-faction'), level: Number(get('m-level')), spec: '', ilvl: get('m-ilvl') };
          if (entry.name.length < 2) return toast('Enter your character name.', true);
          if (!Number.isInteger(entry.level) || entry.level < 1) return toast('Enter your level.', true);
          prefs.manual = entry;
          save('prefs', prefs);
          chooseCharacter({ ...entry, ilvl: Number(entry.ilvl) || null }, false);
        },
      })),
  ];
}

// -- queue view ----------------------------------------------------------------------------

function characterChip(character, locked) {
  const now = serverNow() / 1000;
  const stale = character.addon && character.snapshotAt && now - character.snapshotAt > STALE_SECONDS;
  return [
    h('div', { class: 'card who' },
      h('div', { class: 'grow' },
        h('div', { class: 'name', style: classStyle(character.class), text: character.name }),
        h('div', { class: 'muted small', text: `Level ${character.level} ${character.class}${character.ilvl ? ' · item level ' + character.ilvl : ''} · ${character.faction} · ${character.region} / ${character.ruleset}` })),
      h('span', { class: 'pill ' + (character.addon ? 'good' : 'warn'), text: character.addon ? 'addon' : 'typed in' }),
      h('button', { class: 'ghost small', text: 'Change', disabled: locked, onclick: () => { view = 'character'; refreshAddonCharacters(); } })),
    stale && !locked
      ? h('div', { class: 'hint row' },
          h('span', { class: 'grow', text: `Your stats were saved ${ageText(now - character.snapshotAt)}. Type /reload in game, then refresh.` }),
          h('button', {
            class: 'small',
            text: 'Refresh',
            onclick: async () => {
              addonChars = await invoke('read_characters').catch(() => []);
              const fresh = addonChars.find((c) => c.name === character.name);
              if (fresh) chooseCharacter(fresh, true);
              else toast('That character was not found in the addon data.', true);
            },
          }))
      : null,
  ];
}

function queueView(state) {
  const character = state.character;
  const queue = state.queue;
  const instances = [...(snap.welcome?.instances || [])].sort((a, b) => a.minLevel - b.minLevel);

  if (queue) {
    return [
      characterChip(character, true),
      h('div', { class: 'card status' },
        h('div', { class: 'muted' }, h('span', { class: 'pulse' }), 'In queue'),
        h('div', { class: 'big-time', 'data-since': queue.queuedAt, text: clock(serverNow() - queue.queuedAt) }),
        h('div', { class: 'row wrap', style: 'justify-content:center;margin-top:8px' },
          queue.roles.map((r) => h('span', { class: 'pill ' + r, text: ROLE_LABEL[r] })),
          queue.willLead ? h('span', { class: 'pill gold', text: 'will lead' }) : null),
        h('p', { class: 'muted small', style: 'margin-top:10px', text: queue.instances.map(instanceName).join(', ') }),
        h('p', { class: 'small', style: 'margin-top:8px', text: bestWait(queue) === null ? 'No wait times recorded yet for this.' : 'Usual wait: ' + waitText(bestWait(queue)) })),
      totalsLine(),
      h('p', { class: 'muted small center', text: 'You can close this window. The overlay icon will flash when a group is ready.' }),
      h('div', { class: 'footer' }, h('button', { class: 'danger', text: 'Leave queue', onclick: () => send({ type: 'queue_leave' }) })),
    ];
  }

  const eligible = instances.filter((i) => character.level >= i.minLevel);
  prefs.instances = prefs.instances.filter((id) => eligible.some((i) => i.id === id));
  const toggleRole = (role) => {
    prefs.roles = prefs.roles.includes(role) ? prefs.roles.filter((r) => r !== role) : [...prefs.roles, role];
    save('prefs', prefs);
    render();
  };

  return [
    characterChip(character, false),
    totalsLine(),
    h('div', { class: 'card stack' },
      h('h3', { text: 'Your role' }),
      h('div', { class: 'roles' },
        ROLES.map((r) => h('button', { class: 'role-btn ' + r, 'aria-pressed': String(prefs.roles.includes(r)), onclick: () => toggleRole(r) }, svg(ROLE_ICON[r], 22), ROLE_LABEL[r]))),
      h('label', { class: 'check' },
        h('input', { type: 'checkbox', checked: prefs.willLead, onchange: (e) => { prefs.willLead = e.target.checked; save('prefs', prefs); } }),
        'I am willing to lead the group')),
    h('div', { class: 'card stack' },
      h('h3', { text: 'Instances' }),
      eligible.length
        ? h('div', { class: 'instances' },
            eligible.map((i) =>
              h('label', { class: 'instance' + (character.level > i.maxLevel ? ' above' : '') },
                h('input', {
                  type: 'checkbox',
                  checked: prefs.instances.includes(i.id),
                  onchange: (e) => {
                    prefs.instances = e.target.checked ? [...prefs.instances, i.id] : prefs.instances.filter((id) => id !== i.id);
                    save('prefs', prefs);
                    document.getElementById('join').disabled = !prefs.instances.length || !prefs.roles.length;
                  },
                }),
                h('span', { class: 'grow' },
                  i.name,
                  instanceMeta(i.id, prefs.roles) ? h('span', { class: 'meta', text: instanceMeta(i.id, prefs.roles) }) : null),
                h('span', { class: 'lv', text: `${i.minLevel}-${i.maxLevel}` }))))
        : h('p', { class: 'muted small', text: 'No instances are available at your level yet.' })),
    h('div', { class: 'footer' },
      h('button', {
        id: 'join',
        class: 'primary big',
        text: 'Join queue',
        disabled: !prefs.instances.length || !prefs.roles.length,
        onclick: () => send({ type: 'queue_join', instances: prefs.instances, roles: prefs.roles, willLead: prefs.willLead }),
      })),
  ];
}

// -- ready check -----------------------------------------------------------------------------

function readyView(group) {
  const total = (snap.welcome?.limits?.readyCheckSeconds || 60) * 1000;
  const left = group.readyCheck.deadline - serverNow();
  if (!group.you.accepted) {
    return [
      h('div', { class: 'card ready' },
        h('h2', { text: group.phase === 'active' ? 'A group needs you' : 'Group found' }),
        h('p', { class: 'muted', text: instanceName(group.instanceId) }),
        h('p', { style: 'margin-top:8px' }, 'Your role: ', h('span', { class: 'pill ' + group.you.role, text: ROLE_LABEL[group.you.role] })),
        h('div', { class: 'countdown', 'data-until': group.readyCheck.deadline, text: clock(left) }),
        h('div', { class: 'bar' }, h('i', { 'data-until': group.readyCheck.deadline, 'data-total': total, style: `width:${Math.max(0, Math.min(100, (left / total) * 100))}%` })),
        h('div', { class: 'stack' },
          h('button', { class: 'good big block', text: 'Accept', onclick: () => send({ type: 'ready_response', groupId: group.id, accept: true }) }),
          h('button', { class: 'ghost danger block', text: 'Decline', onclick: () => send({ type: 'ready_response', groupId: group.id, accept: false }) }))),
      h('p', { class: 'muted small center', text: 'If you do not answer in time you are removed from the queue.' }),
    ];
  }
  return [
    h('div', { class: 'card status' },
      h('div', { class: 'muted' }, h('span', { class: 'pulse' }), 'Waiting for the others'),
      h('div', { class: 'big-time', text: `${group.readyCheck.accepted} / ${group.readyCheck.total}` }),
      h('p', { class: 'muted small', style: 'margin-top:8px', text: instanceName(group.instanceId) + ' · you are ' + ROLE_LABEL[group.you.role] })),
    h('div', { class: 'footer' }, h('button', { class: 'danger', text: 'Leave', onclick: () => send({ type: 'ready_response', groupId: group.id, accept: false }) })),
  ];
}

// -- party view ---------------------------------------------------------------------------------

function myRatings(groupId) {
  return ratings[groupId] || (ratings[groupId] = {});
}

function rate(groupId, memberId, value) {
  const mine = myRatings(groupId);
  if (mine[memberId] === value) delete mine[memberId];
  else mine[memberId] = value;
  save('ratings', ratings);
  render();
}

function leave(group, finished) {
  send({ type: 'leave_group', groupId: group.id, ratings: myRatings(group.id), finished });
  delete ratings[group.id];
  save('ratings', ratings);
  leaveDialog = false;
}

function memberRow(group, m) {
  const mine = myRatings(group.id);
  const rep = m.thumbsUp === null
    ? h('span', { class: 'muted', text: 'not enough ratings yet' })
    : h('span', { class: 'rep' }, h('span', { class: 'up', text: '+' + m.thumbsUp }), ' / ', h('span', { class: 'down', text: '-' + m.thumbsDown }));
  const canReplace = group.you.isLeader && group.phase === 'active' && !m.isYou && !m.left;

  return h('div', { class: 'member' + (m.left ? ' left' : '') },
    h('div', { class: 'role-mark ' + m.role, title: ROLE_LABEL[m.role] }, svg(ROLE_ICON[m.role], 16)),
    h('div', { class: 'info' },
      h('div', { class: 'line1' },
        m.inParty === null || m.left ? null : h('span', { class: 'party-dot ' + (m.inParty ? 'in' : 'out'), title: m.inParty ? 'In the party' : 'Not in the party yet' }),
        h('span', { class: 'name', style: classStyle(m.class), text: m.name }),
        m.isYou ? h('span', { class: 'pill', text: 'you' }) : null,
        m.isLeader ? h('span', { class: 'pill gold', text: 'leader' }) : null,
        m.left ? h('span', { class: 'pill bad', text: 'left' }) : null),
      h('div', { class: 'line2' },
        `${m.level} ${m.class}${m.ilvl ? ' · ilvl ' + m.ilvl : ''}${m.statsFromAddon ? '' : ' (typed in)'} · `, rep,
        ` · ${m.runs} ${m.runs === 1 ? 'run' : 'runs'} here`),
      canReplace
        ? h('button', { class: 'ghost small danger', style: 'margin-top:4px', text: 'Replace', onclick: () => {
            if (confirm(`Remove ${m.name} from the group and look for a replacement?`)) send({ type: 'find_replacement', groupId: group.id, memberId: m.id });
          } })
        : null),
    m.isYou
      ? null
      : h('div', { class: 'thumbs' },
          h('button', { class: 'thumb up', title: 'Thumbs up', 'aria-label': `Thumbs up for ${m.name}`, 'aria-pressed': String(mine[m.id] === 1), onclick: () => rate(group.id, m.id, 1) }, svg(THUMB_UP)),
          h('button', { class: 'thumb down', title: 'Thumbs down', 'aria-label': `Thumbs down for ${m.name}`, 'aria-pressed': String(mine[m.id] === -1), onclick: () => rate(group.id, m.id, -1) }, svg(THUMB_DOWN))));
}

function partyView(group) {
  const done = group.phase === 'done';
  const parts = [
    h('div', { class: 'row' },
      h('h2', { class: 'grow', text: instanceName(group.instanceId) }),
      h('span', { class: 'pill ' + (done ? 'gold' : 'good'), text: done ? 'run complete' : 'in progress' })),
  ];

  if (done) {
    parts.push(h('div', { class: 'card' },
      h('p', {}, 'The leader marked this run as done. Rate your group, then leave.'),
      h('p', { class: 'muted small', style: 'margin-top:4px' }, 'Closes in ', h('span', { 'data-until': group.closesAt, text: clock(group.closesAt - serverNow()) }))));
  } else if (group.you.isLeader) {
    parts.push(h('div', { class: 'card stack' },
      h('h3', { text: 'You are the leader' }),
      group.inviteNames.length
        ? [
            h('p', { class: 'small', text: 'Copy this code, then in game type /as and paste it. The addon invites everyone below.' }),
            h('div', { class: 'code-box' },
              h('input', { readOnly: true, value: group.code, 'aria-label': 'Group code', onfocus: (e) => e.target.select() }),
              h('button', {
                class: 'primary',
                text: copied ? 'Copied' : 'Copy',
                onclick: async () => {
                  try {
                    await invoke('copy_text', { text: group.code });
                    copied = true;
                    render();
                    setTimeout(() => { copied = false; render(); }, 2000);
                  } catch (err) {
                    toast('Could not copy: ' + err, true);
                  }
                },
              })),
            h('p', { class: 'muted small', text: 'To invite: ' + group.inviteNames.join(', ') }),
          ]
        : h('p', { class: 'small', text: 'Everyone is in your party.' })));
  } else {
    parts.push(h('div', { class: 'card' },
      h('p', {}, 'Leader: ', h('strong', { text: group.leaderName || 'being chosen' })),
      h('p', { class: 'muted small', style: 'margin-top:4px', text: 'Expect a party invite from them in game.' })));
  }

  if (group.openSlots.length) {
    parts.push(h('div', { class: 'hint' }, 'Looking for a replacement: ', group.openSlots.map((r) => ROLE_LABEL[r]).join(', ')));
  }

  parts.push(h('div', { class: 'card' }, group.members.map((m) => memberRow(group, m))));

  if (leaveDialog) {
    parts.push(h('div', { class: 'card dialog stack' },
      h('p', {}, h('strong', { text: 'Did the run finish?' })),
      h('p', { class: 'muted small', text: 'Your thumbs are sent when you leave. This does not leave the in-game party.' }),
      h('div', { class: 'row' },
        h('button', { class: 'good grow', text: 'Yes', onclick: () => leave(group, true) }),
        h('button', { class: 'grow', text: 'No', onclick: () => leave(group, false) }),
        h('button', { class: 'ghost', text: 'Cancel', onclick: () => { leaveDialog = false; render(); } }))));
  } else {
    parts.push(h('div', { class: 'footer' },
      group.you.isLeader && !done
        ? h('button', { class: 'good', text: 'Mark done', onclick: () => {
            if (confirm('Mark this run as finished for everyone?')) send({ type: 'mark_done', groupId: group.id });
          } })
        : null,
      h('button', {
        class: done ? 'primary' : 'danger',
        text: done ? 'Send ratings and leave' : 'Leave group',
        onclick: () => {
          if (done) leave(group, true);
          else { leaveDialog = true; render(); }
        },
      })));
  }
  return parts;
}

// -- settings -------------------------------------------------------------------------------------

// Shows which addon version is installed, in the Settings card.
async function refreshAddonStatus() {
  const line = document.getElementById('addon-status');
  if (!line) return;
  try {
    const status = await invoke('addon_status');
    if (!status.folderFound) line.textContent = 'Set your World of Warcraft Forever folder above first.';
    else if (status.installed) line.textContent = 'Installed: version ' + status.installed + '. Updating fetches the latest release.';
    else line.textContent = 'Not installed yet.';
  } catch (err) {
    line.textContent = String(err);
  }
}

// Downloads the latest addon release and unpacks it into the game's AddOns folder.
async function installAddon() {
  const button = document.getElementById('addon-install');
  if (button) {
    button.disabled = true;
    button.textContent = 'Downloading';
  }
  try {
    const result = await invoke('install_addon');
    const how = !result.previous ? 'installed' : result.previous === result.version ? 'reinstalled' : 'updated from ' + result.previous;
    toast('Addon version ' + result.version + ' ' + how + '. Type /reload in game to load it.');
  } catch (err) {
    toast(String(err), true);
  }
  const again = document.getElementById('addon-install');
  if (again) {
    again.disabled = false;
    again.textContent = 'Install or update the addon';
  }
  refreshAddonStatus();
}

function settingsView() {
  const s = snap.settings;
  // The overlay switches take effect straight away; the tray menu shows the same two as check marks.
  const overlaySwitch = (id, key, label) =>
    h('label', { class: 'check' },
      h('input', {
        id,
        type: 'checkbox',
        checked: s[key],
        onchange: (e) => invoke('update_settings', { patch: { [key]: e.target.checked } }).catch((err) => toast(String(err), true)),
      }),
      label);

  return [
    h('div', { class: 'row' }, h('h2', { class: 'grow', text: 'Settings' }), h('button', { class: 'ghost small', text: 'Back', onclick: () => { view = null; render(); } })),
    h('div', { class: 'card stack' },
      h('h3', { text: 'Overlay icon' }),
      overlaySwitch('s-overlay', 'overlayEnabled', 'Show the overlay icon over the game'),
      overlaySwitch('s-lock', 'overlayLocked', 'Lock the overlay icon in place'),
      h('button', { text: 'Move it to the game window', onclick: () => invoke('snap_overlay_to_game') })),
    h('div', { class: 'card stack' },
      h('h3', { text: 'Game folder' }),
      h('div', {},
        h('label', { for: 's-wow', text: 'World of Warcraft Forever folder' }),
        h('div', { class: 'row' },
          h('input', { id: 's-wow', value: s.wowDir || '', autocomplete: 'off', spellcheck: 'false' }),
          h('button', { text: 'Browse', onclick: async () => {
            const picked = await invoke('pick_folder');
            if (picked) document.getElementById('s-wow').value = picked;
          } }))),
      h('button', {
        class: 'primary',
        text: 'Save',
        onclick: async () => {
          try {
            await invoke('update_settings', {
              patch: { wowDir: document.getElementById('s-wow').value },
            });
            view = null;
            toast('Settings saved.');
            render();
          } catch (err) {
            toast(String(err), true);
          }
        },
      })),
    h('div', { class: 'card stack' },
      h('h3', { text: 'This app' }),
      h('p', { class: 'small', text: 'Version ' + (snap.version || 'unknown') + (snap.update ? '. Version ' + snap.update + ' is available.' : '') }),
      h('div', { class: 'row' },
        h('button', {
          class: 'grow',
          text: 'Check for updates',
          onclick: async () => {
            try {
              const found = await invoke('check_for_update');
              toast(found ? 'Version ' + found + ' is available.' : 'You have the latest version.');
            } catch (err) {
              toast('Could not check for updates: ' + err, true);
            }
          },
        }),
        snap.update ? h('button', { class: 'primary', text: 'Download', onclick: () => invoke('open_download_page') }) : null)),
    h('div', { class: 'card stack' },
      h('h3', { text: 'In-game addon' }),
      h('p', { id: 'addon-status', class: 'small', text: 'Checking.' }),
      h('button', { id: 'addon-install', class: 'primary', text: 'Install or update the addon', onclick: installAddon })),
    h('div', { class: 'footer' }, h('button', { class: 'danger', text: 'Quit Anti Social', onclick: () => invoke('quit') })),
  ];
}

// -- render ------------------------------------------------------------------------------------------

function render(force) {
  const conn = document.getElementById('conn');
  conn.className = 'conn ' + snap.connection;
  conn.textContent = snap.connection === 'online' ? 'Online' : snap.connection === 'connecting' ? 'Connecting' : 'Offline';

  const banner = document.getElementById('banner');
  if (snap.connection === 'online') {
    banner.hidden = true;
  } else {
    banner.hidden = false;
    banner.className = 'banner' + (snap.connection === 'offline' ? ' bad' : '');
    banner.textContent = snap.detail || (snap.connection === 'offline' ? 'Not connected. Trying again shortly.' : 'Connecting to the server.');
  }

  const update = document.getElementById('update');
  update.hidden = !snap.update;
  if (snap.update) document.getElementById('update-text').textContent = 'Version ' + snap.update + ' is available.';

  const state = snap.state;
  const group = state?.group || null;
  const waiting = Boolean(group && !group.you.accepted);
  setAlarm(waiting);
  if (!group) leaveDialog = false;
  // A ready check outranks whatever screen is open: it has a deadline.
  if (waiting) view = null;

  let content;
  let shown;
  if (view === 'settings') shown = 'settings';
  else if (!state) shown = 'waiting';
  else if (group && (waiting || group.phase === 'ready_check')) shown = 'ready';
  else if (group) shown = 'party';
  else if (!state.character || view === 'character') shown = 'character';
  else shown = 'queue';

  const main = document.getElementById('content');
  const typing = shown === 'settings' || shown === 'character';
  if (typing && !force && main.dataset.view === shown) {
    // Left as it is so typing is not lost, but the overlay switches still follow the tray menu.
    const overlayBox = document.getElementById('s-overlay');
    const lockBox = document.getElementById('s-lock');
    if (overlayBox) overlayBox.checked = Boolean(snap.settings.overlayEnabled);
    if (lockBox) lockBox.checked = Boolean(snap.settings.overlayLocked);
    return;
  }
  main.dataset.view = shown;

  if (view === 'settings') content = settingsView();
  else if (!state) content = [h('p', { class: 'muted center', style: 'margin-top:40px', text: snap.connection === 'online' ? 'Signing in.' : 'Waiting for the server.' })];
  else if (group && (waiting || group.phase === 'ready_check')) content = readyView(group);
  else if (group) content = partyView(group);
  else if (!state.character || view === 'character') content = characterView();
  else content = queueView(state);

  const scroll = main.scrollTop;
  const list = main.querySelector('.instances');
  const listScroll = list ? list.scrollTop : 0;
  main.replaceChildren(...content.flat().filter(Boolean));
  main.scrollTop = scroll;
  const newList = main.querySelector('.instances');
  if (newList) newList.scrollTop = listScroll;
  if (shown === 'settings') refreshAddonStatus();
}

// Keeps countdowns and the queue timer moving between server updates.
setInterval(() => {
  const now = serverNow();
  for (const node of document.querySelectorAll('[data-since]')) node.textContent = clock(now - Number(node.dataset.since));
  for (const node of document.querySelectorAll('[data-until]')) {
    const left = Number(node.dataset.until) - now;
    if (node.dataset.total) node.style.width = Math.max(0, Math.min(100, (left / Number(node.dataset.total)) * 100)) + '%';
    else node.textContent = clock(left);
  }
}, 500);

function apply(next) {
  const hadCharacter = Boolean(snap.state?.character);
  snap = next;
  if (snap.state?.serverTime) clockOffset = snap.state.serverTime - Date.now();
  else if (snap.welcome?.serverTime && !clockOffset) clockOffset = snap.welcome.serverTime - Date.now();
  if (snap.state && !snap.state.character && !hadCharacter && addonChars.length === 0) refreshAddonCharacters();
  render();
}

document.getElementById('btn-hide').addEventListener('click', () => invoke('hide_main'));
document.getElementById('update-get').addEventListener('click', () => invoke('open_download_page'));
document.getElementById('btn-settings').addEventListener('click', () => {
  view = view === 'settings' ? null : 'settings';
  render();
});
document.addEventListener('contextmenu', (e) => e.preventDefault());

listen('app-state', (event) => apply(event.payload));
listen('server-message', (event) => toast(event.payload.message || event.payload.code, event.payload.type === 'error'));
invoke('get_snapshot').then(apply);
