/**
 * TaskList — renders and manages the task list in the main panel.
 *
 * Communicates with the Rust backend via Tauri IPC:
 *   invoke("get_tasks")                                 → Task[]
 *   invoke("assign_task", { title, description, agentId }) → Task
 *   invoke("cancel_task", { id })                       → Task
 *
 * TaskState values (SCREAMING_SNAKE_CASE, as serialised by the Rust backend):
 *   ASSIGNED | ACCEPTED | COMPLETED | BLOCKED | CANCELLED |
 *   REJECTED | TIMED_OUT | INTERRUPTED | RECOVERING | STALE
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/** @type {Task[]} */
let tasks = [];

/**
 * Active filter string. Semantics:
 *   - "default" → hide {COMPLETED, CANCELLED, REJECTED, TIMED_OUT}
 *     (i.e. show in-flight + stale work only). This is the boot state —
 *     matches the "Default" <option> in index.html.
 *   - ""        → show everything ("All").
 *   - Any SCREAMING_SNAKE_CASE state name → exact-match filter.
 */
let filterState = "default";

/** Active search string (lower-cased). */
let filterSearch = "";

/** Event unlisten handle. */
let unlistenTasks = null;

// ---------------------------------------------------------------------------
// Chunked rendering
// ---------------------------------------------------------------------------

/**
 * How many cards we append per chunk. DOM-only cost — no token/context
 * implications (the CLI/MCP surface has its own, tighter cap; see
 * src-tauri/src/wrapper/protocol.rs::DEFAULT_TASK_LIST_LIMIT). 50 keeps
 * initial render snappy even with thousands of retained tasks.
 */
const PAGE_SIZE = 50;

/** Number of cards currently rendered for the active filter/search. */
let rendered = PAGE_SIZE;

/** IntersectionObserver that drives "scroll to reveal more". */
let chunkObserver = null;

/**
 * States we hide from the "Default" view. Explicit list (rather than
 * reusing `is_terminal()` semantics from the Rust side) because the
 * brief wants STALE visible by default even though the Rust enum
 * treats it as terminal. Keep this in sync with index.html's dropdown
 * comment.
 */
const DEFAULT_HIDDEN_STATES = new Set([
  "COMPLETED", "CANCELLED", "REJECTED", "TIMED_OUT",
]);

/**
 * EXTENSION HOOK: "archive-archive" for long-tail tasks.
 *
 * When a category grows to thousands of entries, filter items older
 * than this threshold into a separate "Archived" bucket (or move them
 * to a cold-storage file on disk and serve on demand). The UI already
 * paginates, so the visible cost is bounded — but keeping every task
 * forever in `state.json` has a filesystem / JSON-parse cost we'll
 * want to cap eventually.
 *
 * Not implemented here; this constant is just the obvious landing
 * site for the next pass.
 */
// eslint-disable-next-line no-unused-vars
const ARCHIVE_AFTER_DAYS = 30;

// ---------------------------------------------------------------------------
// DOM refs
// ---------------------------------------------------------------------------

let $scroll;
let $search;
let $filterSelect;
let $overlay;
let $titleInput;
let $descInput;
let $agentSelect;
let $btnNew;
let $btnClose;
let $btnSubmit;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/**
 * Initialise the task list.  Must be called after the DOM is ready.
 */
export function initTaskList() {
  try {
    $scroll       = document.getElementById("task-list-scroll");
    $search       = document.getElementById("task-search");
    $filterSelect = document.getElementById("task-filter-state");
    $overlay      = document.getElementById("modal-overlay");
    $titleInput   = document.getElementById("modal-title-input");
    $descInput    = document.getElementById("modal-desc-input");
    $agentSelect  = document.getElementById("modal-agent-select");
    $btnNew       = document.getElementById("btn-new-task");
    $btnClose     = document.getElementById("btn-close-modal");
    $btnSubmit    = document.getElementById("btn-submit-task");

    if (!$btnNew) console.error("[TaskList] btn-new-task not found");

    if ($btnNew) $btnNew.addEventListener("click", openModal);
    if ($btnClose) $btnClose.addEventListener("click", closeModal);
    if ($btnSubmit) $btnSubmit.addEventListener("click", handleSubmit);

    // Close modal on backdrop click.
    if ($overlay) {
      $overlay.addEventListener("click", (e) => {
        if (e.target === $overlay) closeModal();
      });
    }

    // Keyboard: Escape closes modal.
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && $overlay?.classList.contains("open")) closeModal();
    });

    if ($search) {
      $search.addEventListener("input", () => {
        filterSearch = $search.value.toLowerCase();
        // Reset chunk cursor: the filtered set changed, old offset is
        // meaningless. Without this, filtering to a smaller result set
        // would still try to render from `rendered` — showing nothing.
        rendered = PAGE_SIZE;
        renderTasks();
      });
    }

    if ($filterSelect) {
      // Sync initial value with `filterState`'s default ("default"). The
      // <option selected> attribute in index.html already does this for
      // the browser, but reading it here makes the invariant explicit.
      $filterSelect.value = filterState;
      $filterSelect.addEventListener("change", () => {
        filterState = $filterSelect.value;
        rendered = PAGE_SIZE;
        renderTasks();
      });
    }

    fetchTasks();
    listen("tasks-changed", () => fetchTasks()).then((fn) => {
      unlistenTasks = fn;
    });
  } catch (err) {
    console.error("[TaskList] init failed:", err);
  }
}

/**
 * Stop polling.  Call when tearing down the UI.
 */
export function destroyTaskList() {
  if (unlistenTasks) unlistenTasks();
}

// ---------------------------------------------------------------------------
// IPC
// ---------------------------------------------------------------------------

async function fetchTasks() {
  try {
    tasks = await invoke("get_tasks");
    renderTasks();
  } catch (err) {
    console.error("[TaskList] get_tasks failed:", err);
  }
}

async function handleSubmit() {
  const title       = $titleInput.value.trim();
  const description = $descInput.value.trim();
  const agentId     = $agentSelect.value || null;

  if (!title) {
    $titleInput.focus();
    return;
  }

  try {
    const task = await invoke("assign_task", {
      title,
      description,
      agentId,
    });

    // Optimistic: push before next poll.
    tasks.push(task);
    renderTasks();
    closeModal();
  } catch (err) {
    console.error("[TaskList] assign_task failed:", err);
    alert(`Failed to create task: ${err}`);
  }
}

async function cancelTask(id) {
  try {
    const updated = await invoke("cancel_task", { id });
    const idx = tasks.findIndex((t) => t.id === id);
    if (idx >= 0) tasks[idx] = updated;
    renderTasks();
  } catch (err) {
    console.error("[TaskList] cancel_task failed:", err);
    alert(`Failed to cancel task: ${err}`);
  }
}

async function approveTask(id) {
  try {
    const updated = await invoke("approve_task", { id });
    const idx = tasks.findIndex((t) => t.id === id);
    if (idx >= 0) tasks[idx] = updated;
    renderTasks();
    closeProposalModal();
  } catch (err) {
    console.error("[TaskList] approve_task failed:", err);
    alert(`Failed to approve task: ${err}`);
  }
}

// ---------------------------------------------------------------------------
// Modal helpers
// ---------------------------------------------------------------------------

function openModal() {
  $titleInput.value = "";
  $descInput.value  = "";
  $overlay.classList.add("open");
  $titleInput.focus();
}

function closeModal() {
  $overlay.classList.remove("open");
}

function showProposal(task) {
  const $title = document.getElementById("proposal-title");
  const $brief = document.getElementById("proposal-brief");
  const $diff  = document.getElementById("proposal-diff");
  const $btnApprove = document.getElementById("btn-approve-task");
  const $modal = document.getElementById("proposal-overlay");

  if ($title) $title.textContent = task.title;
  if ($brief) $brief.textContent = task.proposal_brief || "(No logic brief provided)";
  if ($diff)  $diff.textContent  = task.proposal_diff  || "(No diff provided)";

  if ($btnApprove) {
    // Remove old listeners
    const newBtn = $btnApprove.cloneNode(true);
    $btnApprove.parentNode.replaceChild(newBtn, $btnApprove);
    newBtn.addEventListener("click", () => approveTask(task.id));
  }

  const $btnClose = document.getElementById("btn-close-proposal");
  if ($btnClose) {
    $btnClose.addEventListener("click", closeProposalModal);
  }

  $modal.classList.add("open");
}

function closeProposalModal() {
  document.getElementById("proposal-overlay").classList.remove("open");
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function renderTasks() {
  let visible = tasks;

  // State filter. "" = All (no filter). "default" = hide the terminal
  // states DEFAULT_HIDDEN_STATES. Anything else = exact state match.
  if (filterState === "default") {
    visible = visible.filter((t) => !DEFAULT_HIDDEN_STATES.has(t.state));
  } else if (filterState) {
    visible = visible.filter((t) => t.state === filterState);
  }

  if (filterSearch) {
    visible = visible.filter(
      (t) =>
        t.title.toLowerCase().includes(filterSearch) ||
        t.description.toLowerCase().includes(filterSearch) ||
        t.id.toLowerCase().includes(filterSearch)
    );
  }

  // Sort: non-terminal first, then by updated_at descending. Note this
  // uses the broader "terminal" set (includes STALE) so stale records
  // sink below live work in the Default view, even though the Default
  // filter itself keeps STALE visible.
  const terminalStates = new Set([
    "COMPLETED", "CANCELLED", "REJECTED", "TIMED_OUT", "STALE",
  ]);

  visible = [...visible].sort((a, b) => {
    const aTerm = terminalStates.has(a.state);
    const bTerm = terminalStates.has(b.state);
    if (aTerm !== bTerm) return aTerm ? 1 : -1;
    return new Date(b.updated_at) - new Date(a.updated_at);
  });

  // Tear down the previous observer, if any. We re-attach below if the
  // new result set exceeds one chunk. Without this, filter changes
  // would leak observers that still reference stale sentinels.
  if (chunkObserver) {
    chunkObserver.disconnect();
    chunkObserver = null;
  }

  $scroll.innerHTML = "";

  if (visible.length === 0) {
    const empty = document.createElement("p");
    empty.className = "task-empty";
    empty.textContent =
      tasks.length === 0
        ? "No tasks yet. Click + New task to get started."
        : "No tasks match the current filter.";
    $scroll.appendChild(empty);
    return;
  }

  // Clamp the chunk cursor — a filter change may have left `rendered`
  // pointing past the new visible.length.
  const limit = Math.min(rendered, visible.length);
  for (let i = 0; i < limit; i++) {
    $scroll.appendChild(buildTaskCard(visible[i]));
  }

  // Scroll-load sentinel: when it enters the viewport, bump the cursor
  // and re-render. IntersectionObserver is used instead of a scroll
  // handler so we don't fire on every pixel during user scroll.
  if (limit < visible.length) {
    const sentinel = document.createElement("div");
    sentinel.className = "task-load-sentinel";
    sentinel.setAttribute("aria-hidden", "true");
    $scroll.appendChild(sentinel);

    chunkObserver = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          rendered += PAGE_SIZE;
          renderTasks();
          break;
        }
      },
      { root: $scroll, rootMargin: "200px" },
    );
    chunkObserver.observe(sentinel);
  }
}

/**
 * Build a single task card element.
 * @param {Task} task
 * @returns {HTMLElement}
 */
function buildTaskCard(task) {
  const card = document.createElement("div");
  card.className = "task-card";
  card.dataset.taskId = task.id;

  // Title
  const titleEl = document.createElement("div");
  titleEl.className = "task-title";
  titleEl.textContent = task.title;

  // Meta: state badge + agent + time
  const metaEl = document.createElement("div");
  metaEl.className = "task-meta";

  const badge = document.createElement("span");
  badge.className = "state-badge";
  badge.dataset.state = task.state;
  badge.textContent = task.state;

  const agentSpan = document.createElement("span");
  agentSpan.textContent = task.assigned_to || "unassigned";

  const timeSpan = document.createElement("span");
  timeSpan.textContent = formatRelative(task.updated_at);

  metaEl.append(badge, agentSpan, timeSpan);

  // Actions: Cancel for active tasks, Review for PROPOSED
  const terminalStates = new Set([
    "COMPLETED", "CANCELLED", "REJECTED", "TIMED_OUT",
  ]);

  if (!terminalStates.has(task.state)) {
    const actions = document.createElement("div");
    actions.className = "task-actions";

    if (task.state === "PROPOSED") {
      const btnReview = document.createElement("button");
      btnReview.className = "review";
      btnReview.textContent = "Review";
      btnReview.addEventListener("click", () => showProposal(task));
      actions.appendChild(btnReview);
    }

    const btnCancel = document.createElement("button");
    btnCancel.className = "cancel";
    btnCancel.textContent = "Cancel";
    btnCancel.addEventListener("click", () => cancelTask(task.id));
    actions.appendChild(btnCancel);
    card.append(titleEl, metaEl, actions);
  } else {
    card.append(titleEl, metaEl);
  }

  return card;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Format an ISO-8601 timestamp as a human-readable relative string.
 * @param {string} iso
 * @returns {string}
 */
function formatRelative(iso) {
  const diff = Date.now() - new Date(iso).getTime();
  if (diff < 60_000)  return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return new Date(iso).toLocaleDateString();
}
