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

async function init() {
  await loadSessionMeta();
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
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
