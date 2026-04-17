/**
 * Terminal — single xterm.js instance with PTY relay to alor-main.
 *
 * Architecture:
 *   The Rust backend spawns `tmux attach -t alor-main` inside a real PTY.
 *   PTY output streams to xterm.js as incremental data (no polling, no
 *   clear+rewrite). User input goes back through the PTY to tmux.
 *   tmux handles tiling and input routing to the active pane.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let $container;
let term = null;
let fitAddon = null;
let XTerm = null;
let FitAddon = null;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/**
 * Initialise the terminal. Must be called after DOM is ready.
 */
export async function initTerminal() {
  $container = document.getElementById("terminal-container");

  await loadXterm();
  createTerminal();

  // Listen for terminal output from PTY relay (incremental — just append).
  listen("terminal-output", (event) => {
    const { data } = event.payload;
    if (!data || !term) return;
    term.write(data);
  });
}

// ---------------------------------------------------------------------------
// xterm.js setup
// ---------------------------------------------------------------------------

async function loadXterm() {
  try {
    const mod = await import("@xterm/xterm");
    XTerm = mod.Terminal;
    try {
      const fitMod = await import("@xterm/addon-fit");
      FitAddon = fitMod.FitAddon;
    } catch { /* fit addon optional */ }
  } catch {
    console.warn(
      "[Terminal] @xterm/xterm not installed — run `npm install @xterm/xterm @xterm/addon-fit`"
    );
  }
}

function createTerminal() {
  if (!XTerm) {
    $container.innerHTML =
      `<div id="terminal-fallback">` +
      `<span style="color:#7c6ff7">Alor</span> — xterm.js not installed.\n` +
      `<span style="color:#55556a">Run: npm install @xterm/xterm @xterm/addon-fit</span>` +
      `</div>`;
    return;
  }

  term = new XTerm({
    theme: {
      background:  "#0a0a0c",
      foreground:  "#e0e0f0",
      cursor:      "#7c6ff7",
      selectionBackground: "rgba(124,111,247,0.3)",
      black:       "#15151a",
      red:         "#d06060",
      green:       "#4ec94e",
      yellow:      "#f0c040",
      blue:        "#7c6ff7",
      magenta:     "#b06ab0",
      cyan:        "#5bc8af",
      white:       "#e0e0f0",
      brightBlack: "#55556a",
    },
    fontFamily: "'JetBrains Mono', 'Cascadia Code', 'Fira Code', monospace",
    fontSize: 13,
    lineHeight: 1.4,
    cursorBlink: true,
    allowProposedApi: true,
    scrollback: 10000,
  });

  if (FitAddon) {
    fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
  }

  function reportSize() {
    if (!term) return;
    invoke("terminal_resize", { cols: term.cols, rows: term.rows }).catch(() => {});
  }

  function fitAndReport() {
    if (fitAddon) fitAddon.fit();
    reportSize();
  }

  // -----------------------------------------------------------------------
  // Suppress PTY resizes while the mouse is held down.
  // Dragging tmux pane borders triggers ResizeObserver → PTY resize → tmux
  // redraw, which kills the drag. Instead: do nothing until mouseup, then
  // fit once. Listeners are on window (bubble phase) so they don't
  // interfere with xterm.js mouse event forwarding to tmux.
  // -----------------------------------------------------------------------
  let mouseHeld = false;
  let resizePending = false;

  window.addEventListener("mousedown", () => { mouseHeld = true; });
  window.addEventListener("mouseup", () => {
    mouseHeld = false;
    if (resizePending) {
      resizePending = false;
      fitAndReport();
    }
  });

  term.open($container);
  // First layout pass: flex/grid may not have final sizes yet; fit twice on rAF.
  fitAndReport();
  requestAnimationFrame(() => requestAnimationFrame(fitAndReport));

  // -----------------------------------------------------------------------
  // Clipboard: Ctrl+Shift+C to copy, Ctrl+Shift+V to paste.
  // Tauri webview doesn't wire up terminal clipboard by default, so we
  // intercept the key combos before xterm.js processes them.
  // -----------------------------------------------------------------------
  term.attachCustomKeyEventHandler((event) => {
    if (event.type !== "keydown") return true;

    // Ctrl+Shift+C → copy selection to clipboard
    if (event.ctrlKey && event.shiftKey && event.code === "KeyC") {
      const selection = term.select?.getSelection?.() ?? term.getSelection?.();
      if (selection) {
        navigator.clipboard.writeText(selection).catch(() => {});
      }
      return false;
    }

    // Ctrl+Shift+V → paste from clipboard into PTY
    if (event.ctrlKey && event.shiftKey && event.code === "KeyV") {
      navigator.clipboard.readText().then((text) => {
        if (text) {
          invoke("terminal_send_keys", { keys: text }).catch(() => {});
        }
      }).catch(() => {});
      return false;
    }

    // Ctrl+Shift+L → reset manual-layout lock so rebalance_layout re-engages
    // the next time an agent is added or removed. After a user drag-resizes
    // a pane border, the backend auto-locks the layout; this is the escape
    // hatch back to automatic tiling.
    if (event.ctrlKey && event.shiftKey && event.code === "KeyL") {
      invoke("pane_set_layout_mode", { manual: false }).catch((err) => {
        console.warn("[Terminal] pane_set_layout_mode failed:", err);
      });
      return false;
    }

    return true;
  });

  // -----------------------------------------------------------------------
  // Middle-click paste (X11 PRIMARY selection).
  // xterm.js forwards mouse events to tmux (which has `mouse on`), so by the
  // time a normal listener runs, tmux has already swallowed the click and
  // the webview's own "paste PRIMARY on middle-click" behaviour never fires
  // (it only works on contenteditable/textarea targets anyway).
  //
  // Fix: capture-phase listener on the container intercepts BEFORE xterm.js.
  // We stop propagation so tmux never sees the click, read X11 PRIMARY in
  // Rust (xclip / wl-paste), and inject the text into the PTY.
  // -----------------------------------------------------------------------
  // One listener, not two. Middle-click fires both `mousedown` and
  // `auxclick` — hooking both would paste twice (or thrice with browser
  // re-dispatch quirks). `mousedown` is responsive and canonical.
  const handleMiddleClick = (e) => {
    if (e.button !== 1) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    invoke("terminal_paste_primary").catch((err) => {
      console.warn("[Terminal] paste_primary failed:", err);
    });
  };
  $container.addEventListener("mousedown", handleMiddleClick, { capture: true });
  // Still swallow `auxclick` so the browser's default middle-click paste
  // (for contenteditable targets, rare here) never double-fires.
  $container.addEventListener("auxclick", (e) => {
    if (e.button === 1) {
      e.preventDefault();
      e.stopImmediatePropagation();
    }
  }, { capture: true });

  // -----------------------------------------------------------------------
  // Key send queue — serialise IPC calls so rapid keypresses are never
  // reordered by the async Tauri bridge. Each send waits for the previous
  // one to complete before writing to the PTY.
  // -----------------------------------------------------------------------
  let sendQueue = Promise.resolve();

  term.onData((data) => {
    sendQueue = sendQueue.then(() =>
      invoke("terminal_send_keys", { keys: data })
    ).catch((e) => {
      console.warn("[Terminal] send_keys failed:", e);
    });
  });

  // Resize on container resize — suppressed during mouse drag.
  const ro = new ResizeObserver(() => {
    if (mouseHeld) { resizePending = true; console.log("[Terminal] resize suppressed (mouse held)"); return; }
    console.log("[Terminal] ResizeObserver → fitAndReport");
    fitAndReport();
  });
  ro.observe($container);

  // Also report on xterm resize event — same guard.
  term.onResize(() => {
    if (mouseHeld) { resizePending = true; return; }
    reportSize();
  });
}

// ---------------------------------------------------------------------------
// Public write API (for other modules)
// ---------------------------------------------------------------------------

/**
 * Write text to the terminal.
 * @param {string} text  May contain ANSI escape codes.
 */
export function writeToTerminal(text) {
  if (term) {
    term.write(text);
  }
}
