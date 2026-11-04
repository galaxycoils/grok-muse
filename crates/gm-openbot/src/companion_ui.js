'use strict';

const ui = {
  token: '', channel: '', audit: '', epoch: 0, refreshing: false,
  transcript: '', channels: [], connectionError: false, pending: new Set(),
};
const $ = id => document.getElementById(id);
function node(tag, text, className) {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  if (className) el.className = className;
  return el;
}
function notice(message = '') { $('notice').textContent = message; }
function status(message, ok) {
  $('status-pill').textContent = message;
  $('status-pill').className = `badge ${ok ? 'permitted' : 'failed'}`;
}
async function api(path, options = {}) {
  const headers = new Headers(options.headers);
  if (ui.token) headers.set('Authorization', `Bearer ${ui.token}`);
  if (options.body) headers.set('Content-Type', 'application/json');
  const response = await fetch(path, { ...options, headers, cache: 'no-store', signal: AbortSignal.timeout(10000) });
  if (!response.ok) {
    const messages = {
      401: 'Authentication required. Enter the server token and connect.',
      400: 'Request rejected. Check the bot, channel, and message; the channel may be paused or archived.',
      404: 'This channel or task no longer exists. Refresh and try again.',
      409: 'That channel already exists. Choose another ID.',
    };
    throw new Error(messages[response.status] || `Server returned HTTP ${response.status}. Try again.`);
  }
  return response.json();
}
function fillBots(id, bots, includeNone = false) {
  const select = $(id);
  const values = new Set([...select.selectedOptions].map(o => o.value));
  const options = bots.map(b => {
    const option = node('option', `@${b.id}`);
    option.value = b.id;
    option.selected = values.has(b.id);
    return option;
  });
  if (includeNone) {
    const none = node('option', 'No default responder');
    none.value = '';
    none.selected = values.has('');
    options.unshift(none);
  }
  select.replaceChildren(...options);
}
function renderBots(bots) {
  const view = JSON.stringify(bots);
  if ($('bots-list').dataset.view === view) return;
  $('bots-list').dataset.view = view;
  $('bots-list').replaceChildren(...bots.map(b => {
    const item = node('div', undefined, 'item');
    item.append(node('strong', `${b.avatar_emoji} @${b.id} · ${b.display_name}`), node('div', b.role_title, 'muted'));
    return item;
  }));
  // Avoid replacing focused controls or clearing a multi-selection during polling.
  const signature = JSON.stringify(bots.map(b => b.id));
  if ($('task-bot').dataset.bots !== signature) {
    fillBots('task-bot', bots);
    fillBots('channel-owner', bots, true);
    fillBots('channel-members', bots);
    $('task-bot').dataset.bots = signature;
  }
}
function renderChannels(channels) {
  ui.channels = channels;
  if (!channels.some(c => c.id === ui.channel)) ui.channel = channels[0]?.id || '';
  const view = JSON.stringify([channels, ui.channel]);
  if ($('channels-list').dataset.view === view) return;
  $('channels-list').dataset.view = view;
  $('channels-list').replaceChildren(...channels.map(c => {
    const button = node('button', `${c.name}${c.paused ? ' · paused' : ''}`, 'channel-button');
    button.type = 'button';
    button.dataset.channel = c.id;
    button.setAttribute('aria-pressed', String(c.id === ui.channel));
    return button;
  }));
  const current = channels.find(c => c.id === ui.channel);
  $('channel-heading').textContent = current?.name || 'No channels';
  $('channel-description').textContent = current ? `${current.topic} · Default responder: ${current.owner_bot_id ? '@' + current.owner_bot_id : 'none'}${current.paused ? ' · Paused: messages are saved without starting work.' : ''}` : 'Create a channel to start a conversation.';
  $('message-form').querySelector('button').disabled = !current;
}
function renderMessages(messages, channel) {
  const signature = JSON.stringify([channel, messages]);
  if (signature === ui.transcript) return;
  const list = $('messages-list');
  const follow = list.scrollHeight - list.scrollTop - list.clientHeight < 50 || channel !== list.dataset.channel;
  list.replaceChildren(...messages.map(m => {
    const item = node('div', undefined, 'item');
    item.append(node('strong', m.bot_id ? `@${m.bot_id}` : m.author));
    const time = node('time', ` · ${new Date(m.timestamp).toLocaleString()}`, 'muted');
    time.dateTime = m.timestamp;
    item.append(time, node('div', m.content, 'content'));
    return item;
  }));
  if (!messages.length) list.append(node('p', 'No messages yet. Send the first one below.', 'muted'));
  list.dataset.channel = channel;
  ui.transcript = signature;
  if (follow) list.scrollTop = list.scrollHeight;
}
function actionButton(id, action, label) {
  const button = node('button', label, 'secondary');
  button.type = 'button';
  button.dataset.task = id;
  button.dataset.action = action;
  return button;
}
function renderQueue(tasks) {
  const view = JSON.stringify(tasks);
  if ($('queue-list').dataset.view === view) return;
  $('queue-list').dataset.view = view;
  $('queue-list').replaceChildren(...tasks.map(t => {
    const item = node('div', undefined, 'item');
    item.append(node('strong', `@${t.bot_id} · ${t.status.replaceAll('_', ' ').toUpperCase()}`), node('div', t.instruction, 'content'));
    if (t.result_summary) item.append(node('div', t.result_summary, 'content'));
    const actions = node('div', undefined, 'row');
    if (t.status === 'waiting_for_human' || (t.mutating_in_flight && ['failed', 'cancelled'].includes(t.status))) {
      item.append(node('div', `Take the Wheel: ${t.blocker_note || 'Human intervention needed'}`, 'content'));
      actions.append(actionButton(t.id, 'wheel-resume', 'Hand Back to Bot'));
    }
    if (['queued', 'running'].includes(t.status)) actions.append(actionButton(t.id, 'pause', 'Pause'));
    if (t.status === 'paused') actions.append(actionButton(t.id, 'wheel-resume', 'Review / Resume'));
    if (['failed', 'cancelled'].includes(t.status) && !t.mutating_in_flight) actions.append(actionButton(t.id, 'retry', 'Retry')); 
    if (['queued', 'running', 'pause_requested', 'paused', 'waiting_for_human'].includes(t.status)) actions.append(actionButton(t.id, 'cancel', 'Cancel'));
    item.append(actions);
    return item;
  }));
  if (!tasks.length) $('queue-list').append(node('p', 'No queued tasks.', 'muted'));
}
function renderAudit(records) {
  const view = JSON.stringify(records);
  if ($('audit-list').dataset.view === view) return;
  $('audit-list').dataset.view = view;
  $('audit-list').replaceChildren(...records.map(r => {
    const item = node('div', undefined, 'item');
    const category = ['permitted', 'refused', 'failed'].includes(r.category) ? r.category : '';
    item.append(node('span', r.category.toUpperCase(), `badge ${category}`), node('code', ` ${r.rule_id}`), node('div', r.action_summary, 'content'), node('div', r.citation, 'muted'));
    return item;
  }));
  if (!records.length) $('audit-list').append(node('p', 'No matching audit entries.', 'muted'));
}
async function refreshAll() {
  if (ui.refreshing) return;
  ui.refreshing = true;
  const epoch = ui.epoch;
  const audit = ui.audit;
  const channel = ui.channel;
  try {
    const [bots, channels, tasks, records, runtime] = await Promise.all([
      api('/api/bots'), api('/api/channels'), api('/api/queue'),
      api(`/api/audit${audit ? '?category=' + encodeURIComponent(audit) : ''}`), api('/api/runtime'),
    ]);
    if (epoch !== ui.epoch) return;
    renderBots(bots);
    renderChannels(channels);
    renderQueue(tasks);
    if (audit === ui.audit) renderAudit(records);
    const selected = ui.channel;
    if (selected) {
      const messages = await api(`/api/channels/${encodeURIComponent(selected)}/messages`);
      if (epoch === ui.epoch && selected === ui.channel) renderMessages(messages, selected);
    } else renderMessages([], '');
    if (epoch === ui.epoch) {
      status(runtime.worker_running ? (runtime.demo_mode ? 'Demo workers · simulated model' : 'Connected · workers active') : 'Connected · task worker unavailable', true);
      $('runtime-status').textContent = runtime.worker_running ? `Tasks execute automatically. Host desktop control: ${runtime.host_desktop_opt_in ? 'enabled by opt-in' : 'disabled'}. Shell: ${runtime.shell_environment}.` : 'Task worker is unavailable; requests remain queued until a runtime starts.';
      if (ui.connectionError) notice();
      ui.connectionError = false;
    }
  } catch (error) {
    if (epoch === ui.epoch) { ui.connectionError = true; status('Connection unavailable', false); notice(error.message); }
  } finally {
    ui.refreshing = false;
    // A connection, channel, or audit selection changed while requests were in flight.
    if (epoch !== ui.epoch || audit !== ui.audit || channel !== ui.channel) void refreshAll();
  }
}
async function mutate(button, path, body, onSuccess) {
  if (ui.pending.has(path)) return;
  ui.pending.add(path);
  button.disabled = true;
  ui.connectionError = false;
  notice();
  try {
    const result = await api(path, { method: 'POST', body: body === undefined ? undefined : JSON.stringify(body) });
    if (result.ok === false) throw new Error('The task could not be updated. Refresh and try again.');
    onSuccess?.(result);
    // Invalidate any snapshot that started before this write.
    ui.epoch++;
    void refreshAll();
  } catch (error) { notice(error.message); }
  finally { ui.pending.delete(path); button.disabled = false; }
}
$('connection-form').addEventListener('submit', event => {
  event.preventDefault();
  ui.token = $('api-token').value.trim();
  $('api-token').value = '';
  ui.epoch++;
  notice();
  void refreshAll();
});
$('channels-list').addEventListener('click', event => {
  const button = event.target.closest('[data-channel]');
  if (!button) return;
  ui.channel = button.dataset.channel;
  ui.epoch++;
  notice();
  renderChannels(ui.channels);
  $('messages-list').replaceChildren(node('p', 'Loading messages…', 'muted'));
  ui.transcript = '';
  void refreshAll();
});
$('message-form').addEventListener('submit', event => {
  event.preventDefault();
  const input = $('message-input');
  const content = input.value;
  if (!content.trim() || !ui.channel) return;
  void mutate(event.submitter || event.target.querySelector('button'), `/api/channels/${encodeURIComponent(ui.channel)}/messages`, { content }, () => {
    if (input.value === content) input.value = '';
  });
});
$('task-form').addEventListener('submit', event => {
  event.preventDefault();
  const input = $('task-input');
  const instruction = input.value;
  if (!instruction.trim()) return;
  void mutate(event.submitter || event.target.querySelector('button'), '/api/queue', { bot_id: $('task-bot').value, instruction, priority: 5 }, () => {
    if (input.value === instruction) input.value = '';
  });
});
$('channel-create-form').addEventListener('submit', event => {
  event.preventDefault();
  const form = event.target;
  const body = {
    id: $('channel-id').value.trim(), topic: $('channel-topic').value,
    owner_bot_id: $('channel-owner').value || null,
    participants: [...$('channel-members').selectedOptions].map(o => o.value),
  };
  void mutate(event.submitter || form.querySelector('button'), '/api/channels', body, channel => {
    ui.channel = channel.id;
    form.reset();
  });
});
$('queue-list').addEventListener('click', event => {
  const button = event.target.closest('[data-task]');
  if (!button) return;
  let body;
  if (button.dataset.action === 'wheel-resume') {
    const note = prompt('Optional note about what you completed:', 'Completed login/2FA step');
    if (note === null) return;
    body = { note };
  }
  void mutate(button, `/api/tasks/${encodeURIComponent(button.dataset.task)}/${button.dataset.action}`, body);
});
document.querySelectorAll('[data-audit]').forEach(button => button.addEventListener('click', () => {
  ui.audit = button.dataset.audit;
  ui.epoch++;
  notice();
  void refreshAll();
}));
document.addEventListener('visibilitychange', () => { if (!document.hidden) void refreshAll(); });
setInterval(() => { if (!document.hidden) void refreshAll(); }, 3000);
void refreshAll();
