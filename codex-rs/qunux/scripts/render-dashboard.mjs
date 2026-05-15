#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const USAGE = `Usage:
  node codex-rs/qunux/scripts/render-dashboard.mjs [options]

Options:
  --workspace <path>  Workspace containing .qunux/ (default: current working directory)
  --process <id>      Render only one Qunux process id
  --output <path>     Output HTML file (default: <workspace>/.qunux/dashboard.html)
  --help              Show this help
`;

function parseArgs(argv) {
  const args = {
    workspace: process.cwd(),
    processId: null,
    output: null,
    help: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const nextValue = () => {
      i += 1;
      if (i >= argv.length) {
        throw new Error(`${arg} requires a value`);
      }
      return argv[i];
    };
    if (arg === "--help" || arg === "-h") {
      args.help = true;
    } else if (arg === "--workspace" || arg === "-w") {
      args.workspace = nextValue();
    } else if (arg.startsWith("--workspace=")) {
      args.workspace = arg.slice("--workspace=".length);
    } else if (arg === "--process" || arg === "-p") {
      args.processId = nextValue();
    } else if (arg.startsWith("--process=")) {
      args.processId = arg.slice("--process=".length);
    } else if (arg === "--output" || arg === "-o") {
      args.output = nextValue();
    } else if (arg.startsWith("--output=")) {
      args.output = arg.slice("--output=".length);
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }
  args.workspace = path.resolve(args.workspace);
  args.output = args.output
    ? path.resolve(args.output)
    : path.join(args.workspace, ".qunux", "dashboard.html");
  return args;
}

function readJson(filePath) {
  try {
    return JSON.parse(fs.readFileSync(filePath, "utf8"));
  } catch (error) {
    throw new Error(`failed to read ${filePath}: ${error.message}`);
  }
}

function loadProcesses(workspace, processId) {
  const processesDir = path.join(workspace, ".qunux", "processes");
  if (!fs.existsSync(processesDir)) {
    throw new Error(`Qunux process directory not found: ${processesDir}`);
  }

  const loaded = [];
  for (const entry of fs.readdirSync(processesDir, { withFileTypes: true })) {
    if (!entry.isDirectory()) {
      continue;
    }
    if (processId && entry.name !== processId) {
      continue;
    }
    const statePath = path.join(processesDir, entry.name, "closure.json");
    if (!fs.existsSync(statePath)) {
      continue;
    }
    const stat = fs.statSync(statePath);
    const state = readJson(statePath);
    loaded.push({
      processId: state.process_id || entry.name,
      statePath,
      modifiedAt: stat.mtime.toISOString(),
      state,
    });
  }

  loaded.sort((left, right) => left.processId.localeCompare(right.processId));
  if (loaded.length === 0) {
    const scope = processId ? ` for process ${processId}` : "";
    throw new Error(`No Qunux closure states found${scope} in ${processesDir}`);
  }
  return loaded;
}

function htmlEscape(value) {
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function safeJsonForScript(value) {
  return JSON.stringify(value).replace(/</g, "\\u003c");
}

function renderDashboard(payload) {
  const payloadJson = safeJsonForScript(payload);
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Qunux Runtime Observatory</title>
<style>
:root {
  color-scheme: dark;
  --bg: #101214;
  --panel: #181b1f;
  --panel-2: #20242a;
  --line: #333942;
  --muted: #9aa4b2;
  --text: #f3f6f8;
  --accent: #6bb7ff;
  --green: #57c785;
  --amber: #f3bd4f;
  --red: #ef6b73;
  --gray: #7f8792;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font: 14px/1.45 ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}
button, select {
  font: inherit;
}
.app {
  min-height: 100vh;
  display: grid;
  grid-template-rows: auto 1fr auto;
}
.topbar {
  display: grid;
  grid-template-columns: minmax(260px, 1fr) auto;
  gap: 16px;
  align-items: center;
  padding: 14px 18px;
  border-bottom: 1px solid var(--line);
  background: #14171a;
}
.title h1 {
  margin: 0;
  font-size: 18px;
  letter-spacing: 0;
}
.title p {
  margin: 2px 0 0;
  color: var(--muted);
  font-size: 12px;
}
.process-picker {
  display: flex;
  gap: 8px;
  align-items: center;
}
select {
  min-width: 240px;
  max-width: 420px;
  color: var(--text);
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 7px 9px;
}
.layout {
  display: grid;
  grid-template-columns: 320px minmax(360px, 1fr) 360px;
  min-height: 0;
}
.sidebar, .detail, .scheduler {
  min-height: 0;
  overflow: auto;
  padding: 14px;
}
.sidebar, .detail {
  border-right: 1px solid var(--line);
}
.cards {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 10px;
  padding: 12px 14px;
  border-bottom: 1px solid var(--line);
  background: #121518;
}
.card, .panel {
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 6px;
}
.card {
  padding: 10px;
}
.metric {
  color: var(--muted);
  font-size: 11px;
  text-transform: uppercase;
}
.value {
  margin-top: 4px;
  font-size: 18px;
  font-weight: 650;
  overflow-wrap: anywhere;
}
.panel {
  margin-bottom: 12px;
  overflow: hidden;
}
.panel h2 {
  margin: 0;
  padding: 10px 12px;
  font-size: 13px;
  border-bottom: 1px solid var(--line);
  background: var(--panel-2);
}
.panel-body {
  padding: 10px;
}
.tree {
  display: grid;
  gap: 4px;
}
.tree-node {
  width: 100%;
  color: var(--text);
  background: transparent;
  border: 1px solid transparent;
  border-radius: 5px;
  padding: 6px 7px;
  text-align: left;
  cursor: pointer;
}
.tree-node:hover, .tree-node.active {
  background: #242a31;
  border-color: var(--line);
}
.node-title {
  display: flex;
  gap: 6px;
  align-items: center;
  min-width: 0;
}
.node-title strong {
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.node-sub {
  margin-top: 3px;
  color: var(--muted);
  font-size: 12px;
  overflow-wrap: anywhere;
}
.badge {
  display: inline-flex;
  align-items: center;
  min-height: 20px;
  padding: 2px 7px;
  border-radius: 999px;
  font-size: 11px;
  font-weight: 650;
  border: 1px solid transparent;
  white-space: nowrap;
}
.badge.green { color: #dbffea; background: rgba(87, 199, 133, .18); border-color: rgba(87, 199, 133, .45); }
.badge.amber { color: #fff1cb; background: rgba(243, 189, 79, .18); border-color: rgba(243, 189, 79, .45); }
.badge.red { color: #ffdadd; background: rgba(239, 107, 115, .18); border-color: rgba(239, 107, 115, .45); }
.badge.blue { color: #d8ecff; background: rgba(107, 183, 255, .18); border-color: rgba(107, 183, 255, .45); }
.badge.gray { color: #e3e7eb; background: rgba(127, 135, 146, .18); border-color: rgba(127, 135, 146, .45); }
.kv {
  display: grid;
  grid-template-columns: 132px minmax(0, 1fr);
  gap: 6px 10px;
  align-items: start;
}
.kv dt {
  color: var(--muted);
}
.kv dd {
  margin: 0;
  overflow-wrap: anywhere;
}
.body-block {
  max-height: 360px;
  overflow: auto;
  white-space: pre-wrap;
  background: #101316;
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 10px;
  color: #d9e0e7;
}
.scheduler-step {
  display: grid;
  gap: 10px;
}
.big-disposition {
  display: flex;
  gap: 10px;
  align-items: center;
  justify-content: space-between;
  padding: 12px;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 6px;
}
.big-disposition strong {
  font-size: 20px;
}
.instruction {
  padding: 10px;
  border-left: 3px solid var(--accent);
  background: #121820;
  border-radius: 4px;
}
.tabs {
  border-top: 1px solid var(--line);
  background: #121518;
}
.tab-buttons {
  display: flex;
  gap: 6px;
  padding: 10px 14px 0;
  overflow-x: auto;
}
.tab-buttons button {
  color: var(--text);
  background: var(--panel);
  border: 1px solid var(--line);
  border-bottom: none;
  border-radius: 6px 6px 0 0;
  padding: 7px 10px;
  cursor: pointer;
}
.tab-buttons button.active {
  background: var(--panel-2);
  color: white;
}
.tab-content {
  padding: 12px 14px 16px;
  max-height: 320px;
  overflow: auto;
}
table {
  width: 100%;
  border-collapse: collapse;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 6px;
  overflow: hidden;
}
th, td {
  padding: 8px 9px;
  border-bottom: 1px solid var(--line);
  text-align: left;
  vertical-align: top;
}
th {
  color: var(--muted);
  font-size: 11px;
  text-transform: uppercase;
  background: var(--panel-2);
}
td {
  overflow-wrap: anywhere;
}
tr.clickable {
  cursor: pointer;
}
tr.clickable:hover {
  background: #242a31;
}
.empty {
  color: var(--muted);
  padding: 10px;
}
.mono {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
.muted {
  color: var(--muted);
}
@media (max-width: 1180px) {
  .layout { grid-template-columns: 280px minmax(320px, 1fr); }
  .scheduler { grid-column: 1 / -1; border-top: 1px solid var(--line); }
  .cards { grid-template-columns: repeat(2, minmax(0, 1fr)); }
}
@media (max-width: 760px) {
  .topbar { grid-template-columns: 1fr; }
  .layout { grid-template-columns: 1fr; }
  .sidebar, .detail { border-right: none; border-bottom: 1px solid var(--line); }
  .cards { grid-template-columns: 1fr; }
}
</style>
</head>
<body>
<div id="app" class="app"></div>
<script>window.__QUNUX_DASHBOARD__ = ${payloadJson};</script>
<script>
(function () {
  const payload = window.__QUNUX_DASHBOARD__;
  const app = document.getElementById('app');
  const viewState = {
    processIndex: 0,
    selected: null,
    tab: 'threads'
  };

  function values(map) {
    return Object.values(map || {}).sort(function (a, b) {
      return String(a.id || '').localeCompare(String(b.id || ''));
    });
  }

  function state() {
    return payload.processes[viewState.processIndex]?.state || {};
  }

  function currentProcess() {
    return payload.processes[viewState.processIndex] || null;
  }

  function h(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function tone(value) {
    const text = String(value || '').toLowerCase();
    if (['done', 'success', 'terminal', 'consumed', 'ready'].includes(text)) return 'green';
    if (['io_wait', 'waiting', 'waiting_children', 'waiting_io', 'pending'].includes(text)) return 'amber';
    if (['failed', 'cancelled', 'not_success'].includes(text)) return 'red';
    if (['runnable', 'running', 'doing', 'checking', 'classified', 'executing', 'splitting'].includes(text)) return 'blue';
    return 'gray';
  }

  function badge(value) {
    return '<span class="badge ' + tone(value) + '">' + h(value || 'unknown') + '</span>';
  }

  function normalizedEvents(s) {
    return (s.events || []).map(function (event, index) {
      return Object.assign({ id: event.id || 'event-' + index, event_source: 'state' }, event);
    }).concat((s.io_events || []).map(function (event, index) {
      return Object.assign({ id: event.id || 'io-event-' + index, event_source: 'io' }, event);
    })).sort(function (a, b) {
      return String(a.created_at || '').localeCompare(String(b.created_at || ''));
    });
  }

  function entity(kind, id) {
    const s = state();
    if (!id) return null;
    if (kind === 'problem') return s.problems?.[id] || null;
    if (kind === 'thread') return s.threads?.[id] || null;
    if (kind === 'ticket') return s.tickets?.[id] || null;
    if (kind === 'result') return s.results?.[id] || null;
    if (kind === 'check') return s.checks?.[id] || null;
    if (kind === 'handle') return s.handles?.[id] || null;
    if (kind === 'wait') return s.waits?.[id] || null;
    if (kind === 'event') return normalizedEvents(s).find(function (event) { return event.id === id; }) || null;
    return null;
  }

  function dispositionFor(action) {
    if (action === 'wait_thread') return 'io_wait';
    if (action === 'none') return 'terminal';
    return 'runnable';
  }

  function step(action, threadId, targetThreadId, problemId, ticketId, instruction, reason) {
    return {
      action: action,
      disposition: dispositionFor(action),
      thread_id: threadId,
      target_thread_id: targetThreadId || null,
      problem_id: problemId || null,
      ticket_id: ticketId || null,
      instruction: instruction,
      reason: reason
    };
  }

  function threadForRootProblem(s, problemId) {
    return values(s.threads).find(function (thread) {
      return thread.root_problem_id === problemId;
    }) || null;
  }

  function childProblemIdsFromTicket(s, ticketId) {
    return values(s.problems)
      .filter(function (problem) { return problem.created_from_ticket_id === ticketId; })
      .map(function (problem) { return problem.id; });
  }

  function nextForProblem(s, problemId, threadId) {
    const problem = s.problems?.[problemId];
    if (!problem || problem.owner_thread_id !== threadId || problem.status === 'done') {
      return null;
    }

    for (const childId of problem.child_problem_ids || []) {
      const child = s.problems?.[childId];
      if (!child) continue;
      const childThread = threadForRootProblem(s, childId);
      if (childThread && childThread.id !== threadId) {
        if (childThread.status === 'done' && childThread.joined_at) continue;
        if (childThread.status === 'done') {
          return step('join_thread', threadId, childThread.id, childId, null, 'Join the completed child thread before summarizing the parent split ticket.', 'child thread is done but not joined');
        }
        if (childThread.status === 'failed' || childThread.status === 'cancelled') {
          return step('recover_thread', threadId, childThread.id, childId, null, 'Recover the failed child thread before the parent split ticket can be summarized.', 'child thread failed before closing its subtree');
        }
        return step('wait_thread', threadId, childThread.id, childId, null, 'Wait for the child thread to close its bound subtree, then join it.', 'child thread is still running');
      }
      if (child.status === 'done') continue;
      if (child.owner_thread_id === threadId) {
        return step('spawn_thread', threadId, null, childId, null, 'Spawn a child Qunux thread bound to this child problem subtree.', 'split child problem is not assigned to a child thread');
      }
    }

    for (const followupId of problem.followup_problem_ids || []) {
      const followup = s.problems?.[followupId];
      if (!followup) continue;
      if (followup.owner_thread_id === threadId) {
        if (followup.status === 'done') continue;
        const next = nextForProblem(s, followupId, threadId);
        if (next) return next;
      } else {
        const followupThread = threadForRootProblem(s, followupId);
        if (!followupThread) continue;
        if (followupThread.status === 'done' && followupThread.joined_at) continue;
        if (followupThread.status === 'done') {
          return step('join_thread', threadId, followupThread.id, followupId, null, 'Join the completed follow-up thread before re-checking the parent problem.', 'follow-up thread is done but not joined');
        }
        if (followupThread.status === 'failed' || followupThread.status === 'cancelled') {
          return step('recover_thread', threadId, followupThread.id, followupId, null, 'Recover the failed follow-up thread before re-checking the parent problem.', 'follow-up thread failed before closing its subtree');
        }
        return step('wait_thread', threadId, followupThread.id, followupId, null, 'Wait for the follow-up thread to close its bound subtree, then join it.', 'follow-up thread is still running');
      }
    }

    const ticketId = problem.ticket_id;
    if (!ticketId) {
      return step('create_solution_ticket', threadId, null, problemId, null, 'Create exactly one solution ticket for this problem.', 'problem has no solution ticket');
    }
    const ticket = s.tickets?.[ticketId];
    if (!ticket) {
      return step('create_solution_ticket', threadId, null, problemId, null, 'Repair missing ticket record for this problem.', 'problem points at a missing ticket');
    }
    if (ticket.status === 'created') {
      return step('define_ticket', threadId, null, problemId, ticketId, 'Complete the current ticket definition.', 'ticket is created but not defined');
    }
    if (ticket.status === 'defined') {
      return step('classify_ticket', threadId, null, problemId, ticketId, 'Classify the ticket as one_go or split; prefer split unless the work is clearly bounded.', 'ticket is defined but not classified');
    }
    if (ticket.status === 'classified') {
      if (ticket.classification === 'one_go') {
        return step('execute_ticket', threadId, null, problemId, ticketId, 'Execute the ticket, then record the actual result.', 'ticket is classified one_go');
      }
      if (ticket.classification === 'split') {
        return step('split_ticket', threadId, null, problemId, ticketId, 'Move the ticket to splitting and create child problems.', 'ticket is classified split');
      }
      return step('classify_ticket', threadId, null, problemId, ticketId, 'Repair the missing ticket classification.', 'ticket classification is missing');
    }
    if (ticket.status === 'executing') {
      return step('record_result', threadId, null, problemId, ticketId, 'Record the result for the current executing ticket.', 'ticket is executing');
    }
    if (ticket.status === 'splitting') {
      const children = childProblemIdsFromTicket(s, ticketId);
      if (children.length === 0) {
        return step('split_ticket', threadId, null, problemId, ticketId, 'Create at least one child problem from this splitting ticket.', 'split ticket has no child problems');
      }
      return step('record_result', threadId, null, problemId, ticketId, 'Record the parent ticket summary result after all split children are done.', 'split children are closed');
    }
    if (ticket.status === 'done') {
      return step('check_success', threadId, null, problemId, ticketId, 'Run strict problem-level check_success using recorded result IDs.', 'ticket is done and problem needs success check');
    }
    return step('classify_ticket', threadId, null, problemId, ticketId, 'Repair unknown ticket status.', 'ticket status is not recognized');
  }

  function computeNext(s, threadId) {
    const thread = s.threads?.[threadId];
    if (!thread) {
      return step('none', threadId, null, null, null, 'Unknown thread.', 'thread is missing from state');
    }
    const next = nextForProblem(s, thread.root_problem_id, threadId);
    if (next) return next;
    return step('none', threadId, null, null, null, 'No open work remains in this thread subtree.', 'thread frontier is closed');
  }

  function selectedThreadId() {
    const s = state();
    if (viewState.selected?.kind === 'thread') return viewState.selected.id;
    if (viewState.selected?.kind === 'problem') return s.problems?.[viewState.selected.id]?.owner_thread_id || s.main_thread_id;
    return s.main_thread_id;
  }

  function processSummary(s) {
    const problems = values(s.problems);
    const threads = values(s.threads);
    const handles = values(s.handles);
    const waits = values(s.waits);
    return {
      threads: threads.length,
      openProblems: problems.filter(function (p) { return p.status !== 'done'; }).length,
      pendingHandles: handles.filter(function (h) { return h.status === 'pending'; }).length,
      waitingThreads: threads.filter(function (t) { return t.status === 'waiting_children' || t.status === 'waiting_io'; }).length,
      failedThreads: threads.filter(function (t) { return t.status === 'failed'; }).length,
      waits: waits.length
    };
  }

  function renderHeader() {
    const proc = currentProcess();
    const s = state();
    const summary = processSummary(s);
    const options = payload.processes.map(function (item, index) {
      const selected = index === viewState.processIndex ? ' selected' : '';
      return '<option value="' + index + '"' + selected + '>' + h(item.processId) + '</option>';
    }).join('');
    const next = computeNext(s, selectedThreadId());
    return '<header class="topbar">' +
      '<div class="title"><h1>Qunux Runtime Observatory</h1><p>Read-only process, thread, scheduler, IO, and closure inspector</p></div>' +
      '<div class="process-picker"><span class="muted">Process</span><select id="process-select">' + options + '</select></div>' +
      '</header>' +
      '<section class="cards">' +
      metric('Process', proc?.processId || s.process_id || 'unknown') +
      metric('Selected next', next.disposition + ' / ' + next.action) +
      metric('Threads', String(summary.threads) + ' total, ' + String(summary.waitingThreads) + ' waiting') +
      metric('Open problems', String(summary.openProblems) + ' open, ' + String(summary.failedThreads) + ' failed threads') +
      '</section>';
  }

  function metric(label, value) {
    return '<div class="card"><div class="metric">' + h(label) + '</div><div class="value">' + h(value) + '</div></div>';
  }

  function nodeButton(kind, id, title, status, sub, depth) {
    const selected = viewState.selected?.kind === kind && viewState.selected?.id === id;
    return '<button class="tree-node' + (selected ? ' active' : '') + '" data-kind="' + h(kind) + '" data-id="' + h(id) + '" style="padding-left:' + (7 + depth * 16) + 'px">' +
      '<span class="node-title"><span class="mono">' + h(id) + '</span>' + badge(status) + '<strong>' + h(title || id) + '</strong></span>' +
      '<span class="node-sub">' + h(sub || '') + '</span>' +
      '</button>';
  }

  function renderProblemTree(s, problemId, depth) {
    const problem = s.problems?.[problemId];
    if (!problem) return '';
    let html = nodeButton('problem', problem.id, problem.title, problem.status, 'owner ' + problem.owner_thread_id, depth);
    for (const childId of problem.child_problem_ids || []) {
      html += renderProblemTree(s, childId, depth + 1);
    }
    for (const followupId of problem.followup_problem_ids || []) {
      html += renderProblemTree(s, followupId, depth + 1);
    }
    return html;
  }

  function renderThreadTree(s, threadId, depth) {
    const thread = s.threads?.[threadId];
    if (!thread) return '';
    const next = computeNext(s, threadId);
    let html = nodeButton('thread', thread.id, 'root ' + thread.root_problem_id, next.disposition, thread.status + ' / ' + next.action, depth);
    for (const childId of thread.child_thread_ids || []) {
      html += renderThreadTree(s, childId, depth + 1);
    }
    return html;
  }

  function renderSidebar() {
    const s = state();
    return '<aside class="sidebar">' +
      '<section class="panel"><h2>Problem Tree</h2><div class="panel-body tree">' + renderProblemTree(s, s.root_problem_id, 0) + '</div></section>' +
      '<section class="panel"><h2>Thread Tree</h2><div class="panel-body tree">' + renderThreadTree(s, s.main_thread_id, 0) + '</div></section>' +
      '</aside>';
  }

  function kv(rows) {
    return '<dl class="kv">' + rows.map(function (row) {
      return '<dt>' + h(row[0]) + '</dt><dd>' + row[1] + '</dd>';
    }).join('') + '</dl>';
  }

  function link(kind, id) {
    if (!id) return '<span class="muted">none</span>';
    return '<button class="tree-node" data-kind="' + h(kind) + '" data-id="' + h(id) + '"><span class="mono">' + h(id) + '</span></button>';
  }

  function idList(kind, ids) {
    if (!ids || ids.length === 0) return '<span class="muted">none</span>';
    return ids.map(function (id) { return link(kind, id); }).join('');
  }

  function bodyBlock(body) {
    return '<pre class="body-block">' + h(body || '') + '</pre>';
  }

  function renderProblemDetail(s, problem) {
    const ticket = problem.ticket_id ? s.tickets?.[problem.ticket_id] : null;
    return '<section class="panel"><h2>Problem ' + h(problem.id) + '</h2><div class="panel-body">' +
      kv([
        ['Title', h(problem.title)],
        ['Status', badge(problem.status)],
        ['Owner thread', link('thread', problem.owner_thread_id)],
        ['Parent', problem.parent_id ? link('problem', problem.parent_id) : '<span class="muted">none</span>'],
        ['Ticket', ticket ? link('ticket', ticket.id) : '<span class="muted">none</span>'],
        ['Children', idList('problem', problem.child_problem_ids)],
        ['Follow-ups', idList('problem', problem.followup_problem_ids)],
        ['Results', idList('result', problem.result_ids)],
        ['Checks', idList('check', problem.check_ids)]
      ]) + '</div></section>' +
      '<section class="panel"><h2>Problem Body</h2><div class="panel-body">' + bodyBlock(problem.body) + '</div></section>';
  }

  function renderThreadDetail(s, thread) {
    const next = computeNext(s, thread.id);
    return '<section class="panel"><h2>Thread ' + h(thread.id) + '</h2><div class="panel-body">' +
      kv([
        ['Status', badge(thread.status)],
        ['Next disposition', badge(next.disposition)],
        ['Next action', '<span class="mono">' + h(next.action) + '</span>'],
        ['Root problem', link('problem', thread.root_problem_id)],
        ['Parent thread', thread.parent_thread_id ? link('thread', thread.parent_thread_id) : '<span class="muted">none</span>'],
        ['Children', idList('thread', thread.child_thread_ids)],
        ['Actor session', '<span class="mono">' + h(thread.actor_session_id || 'none') + '</span>'],
        ['Codex thread', '<span class="mono">' + h(thread.codex_thread_id || 'none') + '</span>'],
        ['Joined at', h(thread.joined_at || 'not joined')]
      ]) + '</div></section>' +
      '<section class="panel"><h2>Context Fork</h2><div class="panel-body">' + bodyBlock(JSON.stringify(thread.context_fork || {}, null, 2)) + '</div></section>';
  }

  function renderTicketDetail(ticket) {
    return '<section class="panel"><h2>Ticket ' + h(ticket.id) + '</h2><div class="panel-body">' +
      kv([
        ['Title', h(ticket.title)],
        ['Status', badge(ticket.status)],
        ['Classification', badge(ticket.classification || 'unclassified')],
        ['Problem', link('problem', ticket.problem_id)],
        ['Result', ticket.result_id ? link('result', ticket.result_id) : '<span class="muted">none</span>'],
        ['Reason', h(ticket.classification_reason || '')]
      ]) + '</div></section><section class="panel"><h2>Ticket Body</h2><div class="panel-body">' + bodyBlock(ticket.body) + '</div></section>';
  }

  function renderResultDetail(result) {
    return '<section class="panel"><h2>Result ' + h(result.id) + '</h2><div class="panel-body">' +
      kv([
        ['Title', h(result.title)],
        ['Problem', link('problem', result.problem_id)],
        ['Ticket', link('ticket', result.ticket_id)],
        ['Created', h(result.created_at)]
      ]) + '</div></section><section class="panel"><h2>Result Body</h2><div class="panel-body">' + bodyBlock(result.body) + '</div></section>';
  }

  function renderCheckDetail(check) {
    return '<section class="panel"><h2>Check ' + h(check.id) + '</h2><div class="panel-body">' +
      kv([
        ['Title', h(check.title)],
        ['Status', badge(check.status)],
        ['Problem', link('problem', check.problem_id)],
        ['Results', idList('result', check.result_ids)],
        ['Follow-up', check.followup_problem_id ? link('problem', check.followup_problem_id) : '<span class="muted">none</span>'],
        ['Created', h(check.created_at)]
      ]) + '</div></section><section class="panel"><h2>Check Body</h2><div class="panel-body">' + bodyBlock(check.body) + '</div></section>';
  }

  function renderGenericDetail(kind, item) {
    return '<section class="panel"><h2>' + h(kind) + '</h2><div class="panel-body">' + bodyBlock(JSON.stringify(item, null, 2)) + '</div></section>';
  }

  function renderDetail() {
    const s = state();
    if (!viewState.selected) {
      viewState.selected = { kind: 'thread', id: s.main_thread_id };
    }
    const item = entity(viewState.selected.kind, viewState.selected.id);
    if (!item) {
      return '<main class="detail"><section class="panel"><h2>Selection</h2><div class="empty">No entity selected.</div></section></main>';
    }
    let html = '';
    if (viewState.selected.kind === 'problem') html = renderProblemDetail(s, item);
    else if (viewState.selected.kind === 'thread') html = renderThreadDetail(s, item);
    else if (viewState.selected.kind === 'ticket') html = renderTicketDetail(item);
    else if (viewState.selected.kind === 'result') html = renderResultDetail(item);
    else if (viewState.selected.kind === 'check') html = renderCheckDetail(item);
    else html = renderGenericDetail(viewState.selected.kind, item);
    return '<main class="detail">' + html + '</main>';
  }

  function renderScheduler() {
    const s = state();
    const threadId = selectedThreadId();
    const next = computeNext(s, threadId);
    return '<aside class="scheduler">' +
      '<section class="panel"><h2>Next Scheduler</h2><div class="panel-body scheduler-step">' +
      '<div class="big-disposition"><div><div class="metric">Disposition</div><strong>' + h(next.disposition) + '</strong></div>' + badge(next.action) + '</div>' +
      kv([
        ['Thread', link('thread', next.thread_id)],
        ['Target thread', next.target_thread_id ? link('thread', next.target_thread_id) : '<span class="muted">none</span>'],
        ['Problem', next.problem_id ? link('problem', next.problem_id) : '<span class="muted">none</span>'],
        ['Ticket', next.ticket_id ? link('ticket', next.ticket_id) : '<span class="muted">none</span>'],
        ['Reason', h(next.reason)]
      ]) +
      '<div class="instruction">' + h(next.instruction) + '</div>' +
      '</div></section>' +
      '<section class="panel"><h2>Scheduler Meaning</h2><div class="panel-body">' +
      '<p><strong>runnable</strong>: LLM CPU can execute the frontier.</p>' +
      '<p><strong>io_wait</strong>: runtime should park; do not poll with LLM tokens.</p>' +
      '<p><strong>terminal</strong>: this thread subtree is closed.</p>' +
      '</div></section>' +
      '</aside>';
  }

  function table(headers, rows) {
    if (rows.length === 0) return '<div class="empty">No rows.</div>';
    return '<table><thead><tr>' + headers.map(function (header) { return '<th>' + h(header) + '</th>'; }).join('') + '</tr></thead><tbody>' + rows.join('') + '</tbody></table>';
  }

  function row(kind, id, cells) {
    return '<tr class="clickable" data-kind="' + h(kind) + '" data-id="' + h(id) + '">' + cells.map(function (cell) { return '<td>' + cell + '</td>'; }).join('') + '</tr>';
  }

  function diagnostics(s) {
    const issues = [];
    for (const problem of values(s.problems)) {
      if (problem.ticket_id && !s.tickets?.[problem.ticket_id]) issues.push('Problem ' + problem.id + ' points to missing ticket ' + problem.ticket_id);
      for (const childId of problem.child_problem_ids || []) if (!s.problems?.[childId]) issues.push('Problem ' + problem.id + ' points to missing child ' + childId);
      for (const resultId of problem.result_ids || []) if (!s.results?.[resultId]) issues.push('Problem ' + problem.id + ' points to missing result ' + resultId);
      for (const checkId of problem.check_ids || []) if (!s.checks?.[checkId]) issues.push('Problem ' + problem.id + ' points to missing check ' + checkId);
    }
    for (const thread of values(s.threads)) {
      if (!s.problems?.[thread.root_problem_id]) issues.push('Thread ' + thread.id + ' points to missing root problem ' + thread.root_problem_id);
      for (const childId of thread.child_thread_ids || []) if (!s.threads?.[childId]) issues.push('Thread ' + thread.id + ' points to missing child thread ' + childId);
    }
    for (const handle of values(s.handles)) {
      if (handle.target_thread_id && !s.threads?.[handle.target_thread_id]) issues.push('Handle ' + handle.id + ' points to missing thread ' + handle.target_thread_id);
    }
    for (const wait of values(s.waits)) {
      for (const handleId of wait.handle_ids || []) if (!s.handles?.[handleId]) issues.push('Wait ' + wait.id + ' points to missing handle ' + handleId);
    }
    if (issues.length === 0) issues.push('No client-side reference issues detected.');
    return issues;
  }

  function renderTabs() {
    const s = state();
    const tabNames = ['threads', 'handles', 'waits', 'events', 'checks', 'diagnostics'];
    let content = '';
    if (viewState.tab === 'threads') {
      content = table(['thread', 'status', 'next', 'root', 'actor'], values(s.threads).map(function (thread) {
        const next = computeNext(s, thread.id);
        return row('thread', thread.id, [h(thread.id), badge(thread.status), badge(next.disposition) + ' <span class="mono">' + h(next.action) + '</span>', link('problem', thread.root_problem_id), h(thread.actor_session_id || 'none')]);
      }));
    } else if (viewState.tab === 'handles') {
      content = table(['handle', 'kind', 'status', 'owner', 'target'], values(s.handles).map(function (handle) {
        return row('handle', handle.id, [h(handle.id), h(handle.kind), badge(handle.status), link('thread', handle.owner_thread_id), handle.target_thread_id ? link('thread', handle.target_thread_id) : '<span class="muted">none</span>']);
      }));
    } else if (viewState.tab === 'waits') {
      content = table(['wait', 'thread', 'mode', 'status', 'handles'], values(s.waits).map(function (wait) {
        return row('wait', wait.id, [h(wait.id), link('thread', wait.thread_id), h(wait.mode), badge(wait.status), idList('handle', wait.handle_ids)]);
      }));
    } else if (viewState.tab === 'events') {
      const events = normalizedEvents(s);
      content = table(['time', 'source', 'kind', 'entity', 'actor', 'message'], events.map(function (event) {
        return row('event', event.id, [h(event.created_at), h(event.event_source), h(event.kind), h(event.entity_id || event.thread_id || event.handle_id || ''), h(event.actor_thread_id || ''), h(event.message || '')]);
      }));
    } else if (viewState.tab === 'checks') {
      content = table(['check', 'status', 'problem', 'results', 'follow-up'], values(s.checks).map(function (check) {
        return row('check', check.id, [h(check.id), badge(check.status), link('problem', check.problem_id), idList('result', check.result_ids), check.followup_problem_id ? link('problem', check.followup_problem_id) : '<span class="muted">none</span>']);
      }));
    } else {
      const rows = diagnostics(s).map(function (issue, index) {
        return '<tr><td>' + h(index + 1) + '</td><td>' + h(issue) + '</td></tr>';
      });
      content = table(['#', 'diagnostic'], rows);
    }
    const buttons = tabNames.map(function (tab) {
      return '<button class="' + (viewState.tab === tab ? 'active' : '') + '" data-tab="' + h(tab) + '">' + h(tab) + '</button>';
    }).join('');
    return '<section class="tabs"><div class="tab-buttons">' + buttons + '</div><div class="tab-content">' + content + '</div></section>';
  }

  function render() {
    if (!payload.processes.length) {
      app.innerHTML = '<div class="empty">No Qunux process states found.</div>';
      return;
    }
    const s = state();
    if (!viewState.selected || !entity(viewState.selected.kind, viewState.selected.id)) {
      viewState.selected = { kind: 'thread', id: s.main_thread_id };
    }
    app.innerHTML = renderHeader() + '<div class="layout">' + renderSidebar() + renderDetail() + renderScheduler() + '</div>' + renderTabs();
    bindEvents();
  }

  function bindEvents() {
    const select = document.getElementById('process-select');
    if (select) {
      select.addEventListener('change', function (event) {
        viewState.processIndex = Number(event.target.value);
        viewState.selected = null;
        render();
      });
    }
    for (const element of app.querySelectorAll('[data-kind][data-id]')) {
      element.addEventListener('click', function (event) {
        event.stopPropagation();
        viewState.selected = {
          kind: element.getAttribute('data-kind'),
          id: element.getAttribute('data-id')
        };
        render();
      });
    }
    for (const button of app.querySelectorAll('[data-tab]')) {
      button.addEventListener('click', function () {
        viewState.tab = button.getAttribute('data-tab');
        render();
      });
    }
  }

  render();
}());
</script>
</body>
</html>`;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    process.stdout.write(USAGE);
    return;
  }
  const processes = loadProcesses(args.workspace, args.processId);
  const payload = {
    generatedAt: new Date().toISOString(),
    workspace: args.workspace,
    processes,
  };
  const html = renderDashboard(payload);
  fs.mkdirSync(path.dirname(args.output), { recursive: true });
  fs.writeFileSync(args.output, html);
  process.stdout.write(`${args.output}\n`);
}

try {
  main();
} catch (error) {
  process.stderr.write(`render-dashboard: ${error.message}\n`);
  process.exitCode = 1;
}
