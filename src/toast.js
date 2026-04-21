/**
 * Lightweight stacked toasts — non-blocking, dismissible, matches app chrome.
 */

/** @type {HTMLDivElement | null} */
let $stack = null;

/**
 * Normalise Tauri / JS errors for display.
 * @param {unknown} err
 * @returns {string}
 */
export function formatIpcError(err) {
  if (err == null) return "Unknown error";
  if (typeof err === "string") return err;
  if (typeof err === "object") {
    const o = /** @type {Record<string, unknown>} */ (err);
    if (typeof o.message === "string" && o.message) return o.message;
  }
  try {
    return String(err);
  } catch {
    return "Unknown error";
  }
}

function ensureStack() {
  if ($stack) return $stack;
  $stack = document.createElement("div");
  $stack.id = "toast-stack";
  $stack.className = "toast-stack";
  $stack.setAttribute("aria-live", "polite");
  $stack.setAttribute("aria-relevant", "additions text");
  document.body.appendChild($stack);
  return $stack;
}

/**
 * @param {string} message
 * @param {{ variant?: "error" | "info", duration?: number, dismissible?: boolean }} [opts]
 * @returns {{ dismiss: () => void }}
 */
export function showToast(message, opts = {}) {
  const {
    variant = "info",
    duration = variant === "error" ? 12_000 : 8_000,
    dismissible = true,
  } = opts;

  const stack = ensureStack();
  const el = document.createElement("div");
  el.className = `toast toast-${variant}`;
  el.setAttribute("role", variant === "error" ? "alert" : "status");

  const body = document.createElement("div");
  body.className = "toast-body";
  body.textContent = message;
  el.appendChild(body);

  let hideTimer = /** @type {ReturnType<typeof setTimeout> | null} */ (null);

  const remove = () => {
    if (hideTimer != null) {
      clearTimeout(hideTimer);
      hideTimer = null;
    }
    if (!el.isConnected) return;
    el.classList.add("toast-out");
    let gone = false;
    const finish = () => {
      if (gone) return;
      gone = true;
      el.remove();
    };
    el.addEventListener("transitionend", finish, { once: true });
    setTimeout(finish, 400);
  };

  if (dismissible) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "toast-dismiss";
    btn.setAttribute("aria-label", "Dismiss notification");
    btn.textContent = "\u00d7";
    btn.addEventListener("click", remove);
    el.appendChild(btn);
  }

  // Newest near the top so stacks stay readable.
  stack.prepend(el);

  if (duration > 0) {
    hideTimer = setTimeout(remove, duration);
  }

  return { dismiss: remove };
}

/**
 * @param {string} prefix  Human-readable context (no trailing colon).
 * @param {unknown} err
 */
export function toastIpcError(prefix, err) {
  const detail = formatIpcError(err);
  showToast(`${prefix}: ${detail}`, { variant: "error" });
}
