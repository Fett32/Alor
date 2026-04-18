/**
 * Alor — frontend entry point.
 *
 * Boot order:
 *   1. Load session metadata from Rust backend (header bar).
 *   2. Init AgentPanel (left sidebar + register form).
 *   3. Init TaskList  (main panel + new-task modal).
 *   4. Init Terminal  (bottom panel, lazy-loads xterm.js).
 */

import { invoke } from "@tauri-apps/api/core";
import { initAgentPanel } from "./components/AgentPanel.js";
import { initTaskList }   from "./components/TaskList.js";
import { initTerminal }   from "./components/Terminal.js";

// ---------------------------------------------------------------------------
// Session header
// ---------------------------------------------------------------------------

async function loadSessionMeta() {
  const $meta = document.getElementById("session-meta");
  if (!$meta) return;

  try {
    const info = await invoke("get_session_info");
    // info: { started_at: string, pid: number, version: string }
    const started = new Date(info.started_at).toLocaleTimeString([], {
      hour:   "2-digit",
      minute: "2-digit",
    });
    $meta.textContent = `v${info.version}  ·  pid ${info.pid}  ·  started ${started}`;
  } catch (err) {
    console.warn("[main] get_session_info failed:", err);
    $meta.textContent = "session unavailable";
  }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Spawn-workspace setting
// ---------------------------------------------------------------------------

/**
 * Wire up the header "Spawn ws" input. Loads the current value from the
 * Rust settings, saves on blur or Enter. Takes effect on next Alor
 * launch — a Sway `assign [app_id="alor"] workspace <value>` rule is
 * registered at startup so the window appears directly on the target
 * workspace without flicker or focus steal.
 *
 * Empty value = falls through to default Tauri/Wayland behavior.
 */
async function wireSpawnWorkspace() {
  const $input = document.getElementById("spawn-workspace");
  if (!$input) return;

  try {
    const s = await invoke("get_settings");
    $input.value = s?.spawn_workspace ?? "";
  } catch (err) {
    console.warn("[settings] load failed:", err);
  }

  const save = async () => {
    const value = $input.value.trim();
    try {
      await invoke("set_settings", {
        settings: { spawn_workspace: value || null },
      });
    } catch (err) {
      console.error("[settings] save failed:", err);
    }
  };

  $input.addEventListener("blur", save);
  $input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      save();
      $input.blur();
    }
  });
}

async function init() {
  await loadSessionMeta();
  await wireSpawnWorkspace();
  initAgentPanel();
  initTaskList();
  await initTerminal();

  // Kill all sessions button
  const $btnKillAll = document.getElementById("btn-kill-all");
  if ($btnKillAll) {
    $btnKillAll.addEventListener("click", async () => {
      if (confirm("Kill all background agent sessions and wrappers?")) {
        try {
          await invoke("kill_all_agents");
          console.log("[main] kill_all_agents successful");
        } catch (err) {
          console.error("[main] kill_all_agents failed:", err);
          alert(`Failed to kill sessions: ${err}`);
        }
      }
    });
  }

  // Balance button — force a pane re-tile in alor-main.
  const $btnBalance = document.getElementById("btn-balance");
  if ($btnBalance) {
    $btnBalance.addEventListener("click", async () => {
      try {
        await invoke("pane_rebalance");
        console.log("[main] pane_rebalance successful");
      } catch (err) {
        console.error("[main] pane_rebalance failed:", err);
        alert(`Failed to rebalance: ${err}`);
      }
    });
  }
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
