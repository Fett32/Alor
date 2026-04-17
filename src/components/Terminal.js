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

  // Capture phase so we win the race against our own mouse-forwarder
  // below, which stops propagation to xterm.js (and would otherwise skip
  // these bubble-phase window listeners too).
  window.addEventListener("mousedown", () => { mouseHeld = true; }, { capture: true });
  window.addEventListener("mouseup", () => {
    mouseHeld = false;
    if (resizePending) {
      resizePending = false;
      fitAndReport();
    }
  }, { capture: true });

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
  // Helper: convert a mouse event's pixel coords to a 1-based tmux cell
  // column and row.  Shared by the wheel and click forwarders.
  // -----------------------------------------------------------------------
  function cellFromEvent(e) {
    const rect = $container.getBoundingClientRect();
    const cols = term.cols || 80;
    const rows = term.rows || 24;
    const cellW = rect.width / cols;
    const cellH = rect.height / rows;
    const col = Math.max(1, Math.min(cols, Math.floor((e.clientX - rect.left) / cellW) + 1));
    const row = Math.max(1, Math.min(rows, Math.floor((e.clientY - rect.top) / cellH) + 1));
    return { col, row };
  }

  // -----------------------------------------------------------------------
  // Left/right click forwarding -> tmux SGR mouse escape.
  // Same rationale as the wheel handler: xterm.js isn't reliably enabling
  // mouse tracking, so clicks never reach tmux and `select-pane -t =`
  // never fires.  Without that, clicking another pane doesn't focus it
  // for typing.  We synthesise button press (M) + release (m) events in
  // SGR mouse mode 1006 so tmux can route them properly.
  //
  // Middle-click (button 1) is handled separately above — it reads X11
  // PRIMARY and pastes, we do NOT want it to also forward as a tmux
  // select-pane.
  // -----------------------------------------------------------------------
  // Track the currently-held button so mousemove can forward drag events
  // in SGR motion form (base button + 32). Only one button tracked at a
  // time; tmux doesn't need chord drags.
  let heldButton = -1;
  let lastCol = 0;
  let lastRow = 0;

  $container.addEventListener("mousedown", (e) => {
    if (e.button === 1) return; // handled by middle-click paste
    if (!term) return;
    // xterm.js hosts a hidden textarea that receives keyboard input; the
    // browser focuses it on a real click. Since we swallow the click
    // below (stopImmediatePropagation), xterm never focuses itself and
    // typing stops working. Focus it explicitly.
    term.focus();
    const { col, row } = cellFromEvent(e);
    const button = e.button === 2 ? 2 : 0; // 0 = left, 2 = right
    heldButton = button;
    lastCol = col;
    lastRow = row;
    const seq = `\x1b[<${button};${col};${row}M`;
    invoke("terminal_send_keys", { keys: seq }).catch(() => {});
    e.preventDefault();
    e.stopImmediatePropagation();
  }, { capture: true });

  $container.addEventListener("mousemove", (e) => {
    if (heldButton < 0) return;
    if (!term) return;
    const { col, row } = cellFromEvent(e);
    if (col === lastCol && row === lastRow) return; // still in same cell
    lastCol = col;
    lastRow = row;
    // SGR motion: base button + 32 mask.
    const seq = `\x1b[<${heldButton + 32};${col};${row}M`;
    invoke("terminal_send_keys", { keys: seq }).catch(() => {});
    e.preventDefault();
    e.stopImmediatePropagation();
  }, { capture: true });

  // mouseup fires on window (not just container) in case the user drags
  // outside and releases there — otherwise the button looks stuck held.
  window.addEventListener("mouseup", (e) => {
    if (e.button === 1) return;
    if (!term) return;
    if (heldButton < 0) return;
    const { col, row } = cellFromEvent(e);
    const button = heldButton;
    heldButton = -1;
    const seq = `\x1b[<${button};${col};${row}m`;
    invoke("terminal_send_keys", { keys: seq }).catch(() => {});
  }, { capture: true });

  // Suppress the browser's default right-click context menu; tmux has
  // its own MouseDown3Pane menu that we want to show instead.
  $container.addEventListener("contextmenu", (e) => {
    e.preventDefault();
  });

  // -----------------------------------------------------------------------
  // xterm.js selection -> X11 PRIMARY selection.
  // xterm.js's own drag-to-select survives our mouse-forwarder (its
  // internal listeners are on the canvas, not the outer $container) and
  // shows a persistent highlight the user can see while choosing text.
  // We mirror that selection into PRIMARY so middle-click paste (and
  // middle-click in any other app) gets exactly what's visibly selected.
  // -----------------------------------------------------------------------
  term.onSelectionChange(() => {
    const sel = term.getSelection?.() ?? "";
    if (!sel) return;
    invoke("terminal_set_primary", { text: sel }).catch((err) => {
      console.warn("[Terminal] set_primary failed:", err);
    });
  });

  // -----------------------------------------------------------------------
  // Wheel events -> tmux SGR mouse escape.
  // xterm.js by default scrolls its own internal buffer on wheel. With
  // multiple tmux panes rendered inside one xterm, that shows historical
  // frames of the WHOLE alor-main terminal (both panes combined), which
  // is useless. tmux itself handles per-pane scrollback via copy-mode
  // when it receives mouse wheel escape sequences, so we translate the
  // wheel event into an SGR mouse report (mode 1006) aimed at the cell
  // under the cursor and inject it into the PTY.
  //
  // `preventDefault` + `stopImmediatePropagation` keep xterm.js from
  // also scrolling its own buffer on the same event.
  // -----------------------------------------------------------------------
  $container.addEventListener("wheel", (e) => {
    if (!term) return;
    const { col, row } = cellFromEvent(e);
    // SGR mouse mode 1006: CSI < button ; col ; row M (press) or m (release).
    // Wheel up = button 64, wheel down = 65. Scroll events only have press.
    const button = e.deltaY < 0 ? 64 : 65;
    const seq = `\x1b[<${button};${col};${row}M`;
    invoke("terminal_send_keys", { keys: seq }).catch((err) => {
      console.warn("[Terminal] wheel forward failed:", err);
    });
    e.preventDefault();
    e.stopImmediatePropagation();
  }, { capture: true, passive: false });

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
