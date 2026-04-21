/**
 * AgentPanel — manages the left sidebar that shows registered agents.
 *
 * Communicates with the Rust backend via Tauri IPC:
 *   invoke("get_agents")           → Agent[]
 *   invoke("register_agent", {...}) → Agent
 *
 * Emits a custom DOM event "agent-selected" on #agent-panel whenever the user
 * clicks an agent row. Other components (e.g. Terminal) can listen for it to
 * focus the relevant tmux session.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { toastIpcError } from "../toast.js";

// ---------------------------------------------------------------------------
// Data model (mirrors Rust Agent struct via serde JSON)
// ---------------------------------------------------------------------------
// Agent {
//   id:             string
//   name:           string
//   tmux_session:   string | null
//   socket_path:    string | null
//   connected:      boolean
//   registered_at:  string   (ISO-8601)
// }

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/** @type {import('../types.js').Agent[]} */
let agents = [];

/** ID of the currently selected agent, or null. */
let selectedId = null;

/** Event unlisten handles. */
let unlistenAgents = null;
let unlistenPaneAddFailed = null;

// ---------------------------------------------------------------------------
// DOM refs (resolved once after DOMContentLoaded)
// ---------------------------------------------------------------------------

let $list;
let $form;
let $btnShowRegister;
let $regId;
let $regName;
let $regTmux;
let $panel;
/** Host for pane-add-failure banner (created on first use). */
let $paneFailBanner = null;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/**
 * Initialise the agent panel.  Must be called after the DOM is ready.
 */
export function initAgentPanel() {
  try {
    $panel          = document.getElementById("sidebar");
    $list           = document.getElementById("agent-list");
    $form           = document.getElementById("register-form");
    $btnShowRegister = document.getElementById("btn-show-register");
    $regId          = document.getElementById("reg-id");
    $regName        = document.getElementById("reg-name");

    if (!$btnShowRegister) console.error("[AgentPanel] btn-show-register not found");
    if (!$form) console.error("[AgentPanel] register-form not found");

    $btnShowRegister.addEventListener("click", () => {
      console.log("[AgentPanel] toggle spawn form clicked");
      $form.classList.toggle("hidden");
      if (!$form.classList.contains("hidden")) $regId.focus();
    });

    $form.addEventListener("submit", async (e) => {
      e.preventDefault();
      const kind = $regId.value.trim();
      const role = $regName.value.trim();
      
      console.log(`[AgentPanel] spawning ${kind} as ${role}`);
      try {
        await invoke("spawn_agent", { agent: kind, role: role });
        $form.reset();
        $form.classList.add("hidden");
      } catch (err) {
        console.error("[AgentPanel] spawn_agent failed:", err);
        toastIpcError("Failed to spawn agent", err);
      }
    });

    // Initial fetch + listen for push updates from backend.
    fetchAgents();
    // Safety-net: ask the daemon to reconcile alor-main panes on boot.
    // Addresses the class of bug where an agent is connected in state
    // but its pane is missing from the main terminal view — sidebar
    // row shows green + kill works, but the agent's pane never made
    // it into alor-main (e.g. wrapper.register's add_agent_pane
    // failed silently during an earlier daemon session). The daemon
    // also runs this at startup; calling from the UI covers the case
    // where the frontend is re-launched against an already-running
    // daemon. See src-tauri/src/terminal/pane_manager.rs::reconcile_panes.
    invoke("pane_reconcile").catch((err) => {
      console.warn("[AgentPanel] pane_reconcile failed:", err);
    });

    listen("agents-changed", () => fetchAgents()).then((fn) => {
      unlistenAgents = fn;
    });
    // On a fresh connection, run a reconcile pass after the refresh
    // lands — catches the exact repro from task 1ed52762 where a
    // newly-spawned cursor agent reaches connected=true in state but
    // doesn't show up in the main terminal view.
    listen("agent.connected", () => {
      fetchAgents();
      invoke("pane_reconcile").catch((err) => {
        console.warn("[AgentPanel] pane_reconcile after connect failed:", err);
      });
    });
    listen("agent.disconnected", () => fetchAgents());

    listen("agent.pane_add_failed", (event) => {
      const p = event.payload;
      const agentId = p?.agent_id ?? "(unknown)";
      const tmuxSession = p?.tmux_session ?? "";
      const errText = p?.error ?? String(p);
      showPaneAddFailedBanner({ agentId, tmuxSession, errText });
    }).then((fn) => {
      unlistenPaneAddFailed = fn;
    });
  } catch (err) {
    console.error("[AgentPanel] init failed:", err);
  }
}

/**
 * Stop polling.  Call when tearing down the UI.
 */
export function destroyAgentPanel() {
  if (unlistenAgents) unlistenAgents();
  if (unlistenPaneAddFailed) unlistenPaneAddFailed();
}

/**
 * Return a copy of the current agent list.
 * @returns {import('../types.js').Agent[]}
 */
export function getAgents() {
  return [...agents];
}

// ---------------------------------------------------------------------------
// Pane add failure banner
// ---------------------------------------------------------------------------

/**
 * @param {{ agentId: string, tmuxSession: string, errText: string }} detail
 */
function showPaneAddFailedBanner({ agentId, tmuxSession, errText }) {
  if (!$panel) return;

  if (!$paneFailBanner) {
    $paneFailBanner = document.createElement("div");
    $paneFailBanner.id = "agent-pane-fail-banner";
    $paneFailBanner.className = "agent-pane-fail-banner";
    $paneFailBanner.setAttribute("role", "alert");
    $panel.insertBefore($paneFailBanner, $panel.firstChild);
  }

  $paneFailBanner.replaceChildren();

  const inner = document.createElement("div");
  inner.className = "agent-pane-fail-inner";

  const title = document.createElement("div");
  title.className = "agent-pane-fail-title";
  title.append(
    document.createTextNode("Could not add pane for agent "),
    Object.assign(document.createElement("strong"), { textContent: agentId }),
  );

  const meta = document.createElement("div");
  meta.className = "agent-pane-fail-meta";
  meta.textContent = tmuxSession
    ? `tmux session: ${tmuxSession}`
    : "tmux session: —";

  const errEl = document.createElement("div");
  errEl.className = "agent-pane-fail-err";
  errEl.textContent = errText;

  const actions = document.createElement("div");
  actions.className = "agent-pane-fail-actions";

  const btnRetry = document.createElement("button");
  btnRetry.type = "button";
  btnRetry.className = "btn-pane-fail-retry";
  btnRetry.textContent = "Retry";
  btnRetry.addEventListener("click", () => {
    btnRetry.disabled = true;
    invoke("pane_reconcile")
      .then(() => fetchAgents())
      .catch((err) => {
        console.warn("[AgentPanel] pane_reconcile (banner) failed:", err);
      })
      .finally(() => {
        btnRetry.disabled = false;
      });
  });

  const btnDismiss = document.createElement("button");
  btnDismiss.type = "button";
  btnDismiss.className = "btn-pane-fail-dismiss";
  btnDismiss.textContent = "Dismiss";
  btnDismiss.addEventListener("click", () => {
    $paneFailBanner?.remove();
    $paneFailBanner = null;
  });

  actions.append(btnRetry, btnDismiss);
  inner.append(title, meta, errEl, actions);
  $paneFailBanner.appendChild(inner);
}

// ---------------------------------------------------------------------------
// IPC
// ---------------------------------------------------------------------------

async function fetchAgents() {
  try {
    const raw = await invoke("get_agents");
    console.log("[AgentPanel] fetched agents:", raw);
    agents = raw;
    renderList();
    syncModalAgentSelect();
  } catch (err) {
    console.error("[AgentPanel] get_agents failed:", err);
  }
}

async function handleRegister() {
  const id   = $regId.value.trim();
  const name = $regName.value.trim();
  const tmux = $regTmux.value.trim() || null;

  if (!id || !name) return;

  try {
    const agent = await invoke("register_agent", {
      id,
      name,
      tmuxSession: tmux,
    });

    // Optimistic update — the next poll will confirm.
    const existing = agents.findIndex((a) => a.id === agent.id);
    if (existing >= 0) {
      agents[existing] = agent;
    } else {
      agents.push(agent);
    }

    renderList();
    syncModalAgentSelect();

    // Reset + hide form.
    $form.reset();
    $form.classList.add("hidden");
  } catch (err) {
    console.error("[AgentPanel] register_agent failed:", err);
    toastIpcError("Failed to register agent", err);
  }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function renderList() {
  if (agents.length === 0) {
    $list.innerHTML = '<p class="agent-empty">No agents registered.</p>';
    return;
  }

  $list.innerHTML = "";

  // Sort: connected first, then alphabetically by name.
  const sorted = [...agents].sort((a, b) => {
    if (a.connected !== b.connected) return b.connected ? 1 : -1;
    return a.name.localeCompare(b.name);
  });

  for (const agent of sorted) {
    const item = buildAgentItem(agent);
    $list.appendChild(item);
  }
}

/**
 * Build a single agent row element.
 * @param {import('../types.js').Agent} agent
 * @returns {HTMLElement}
 */
function buildAgentItem(agent) {
  const item = document.createElement("div");
  item.className = "agent-item" + (agent.id === selectedId ? " selected" : "");
  item.dataset.agentId = agent.id;

  const dot = document.createElement("span");
  dot.className = "agent-dot " + (agent.connected ? "connected" : "disconnected");
  dot.title = agent.connected ? "Connected" : "Disconnected";

  const info = document.createElement("div");
  info.className = "agent-info";

  const nameEl = document.createElement("div");
  nameEl.className = "agent-name";
  nameEl.textContent = agent.name;

  if (agent.project) {
    const projectTag = document.createElement("span");
    projectTag.className = "agent-project-tag";
    projectTag.textContent = agent.project;
    projectTag.title = `Slot bound to project: ${agent.project}`;
    nameEl.appendChild(projectTag);
  }

  const metaEl = document.createElement("div");
  metaEl.className = "agent-meta";
  const parts = [agent.id];
  if (agent.tier && agent.tier !== "mid") parts.push(agent.tier);
  if (typeof agent.max_concurrent === "number" && agent.max_concurrent > 1) {
    parts.push(`x${agent.max_concurrent}`);
  }
  if (agent.tmux_session) parts.push(agent.tmux_session);
  metaEl.textContent = parts.join(" · ");

  info.append(nameEl, metaEl);
  item.append(dot, info);

  // Add "Start Brain" CTA for disconnected orchestrator
  if (agent.id === "orchestrator" && !agent.connected) {
    const btnStart = document.createElement("button");
    btnStart.className = "agent-start-cta";
    btnStart.textContent = "Start Brain";
    btnStart.addEventListener("click", (e) => {
      e.stopPropagation();
      if (btnStart.disabled) return;
      btnStart.disabled = true;
      btnStart.textContent = "Starting...";
      console.log("[AgentPanel] starting orchestrator brain...");
      invoke("spawn_agent", { agent: "orchestrator", role: "orchestrator" })
        .catch((err) => {
          toastIpcError("Failed to start orchestrator", err);
          btnStart.disabled = false;
          btnStart.textContent = "Start Brain";
        });
    });
    item.appendChild(btnStart);
  }

  // Add "Kill" button for connected agents
  if (agent.connected) {
    const btnKill = document.createElement("button");
    btnKill.className = "agent-kill-cta";
    btnKill.textContent = "Kill";
    btnKill.title = `Stop ${agent.id} session`;
    btnKill.addEventListener("click", (e) => {
      e.stopPropagation();
      if (confirm(`Kill ${agent.id} session?`)) {
        invoke("kill_agent", { id: agent.id }).catch((err) =>
          toastIpcError("Failed to kill agent", err),
        );
      }
    });
    item.appendChild(btnKill);
  }

  // Add "Delete" button for template-spawned instances only.
  // Core yaml slots (claude, claude-alor, codex, cursor, etc.) get Kill only —
  // they're the permanent roster. Template instances (claude-mandaspace, etc.)
  // get Kill + Delete so Fett can tombstone a stale row.
  if (agent.template) {
    const btnDelete = document.createElement("button");
    btnDelete.className = "agent-delete-cta";
    btnDelete.textContent = "Delete";
    btnDelete.title = `Permanently remove ${agent.id} from state`;
    btnDelete.addEventListener("click", (e) => {
      e.stopPropagation();
      if (confirm(`Permanently delete ${agent.id}? This cannot be undone.`)) {
        invoke("delete_agent", { id: agent.id }).catch((err) =>
          toastIpcError("Failed to delete agent", err),
        );
      }
    });
    item.appendChild(btnDelete);
  }

  item.addEventListener("click", () => selectAgent(agent.id));

  return item;
}

function selectAgent(id) {
  selectedId = id;
  renderList();

  const agent = agents.find((a) => a.id === id) ?? null;
  $panel.dispatchEvent(
    new CustomEvent("agent-selected", {
      bubbles: true,
      detail: { agent },
    })
  );
}

// ---------------------------------------------------------------------------
// Keep the modal's agent <select> in sync with the current agent list.
// ---------------------------------------------------------------------------

function syncModalAgentSelect() {
  const $select = document.getElementById("modal-agent-select");
  if (!$select) return;

  // Preserve current selection.
  const prev = $select.value;

  // Keep the first placeholder option, rebuild the rest.
  while ($select.options.length > 1) $select.remove(1);

  for (const agent of agents) {
    const opt = document.createElement("option");
    opt.value = agent.id;
    opt.textContent = `${agent.name} (${agent.id})`;
    $select.appendChild(opt);
  }

  if (prev) $select.value = prev;
}
