// The page's input in the terms Wayland carries, for both desktops
// (compositor.js and dom-compositor.js).

/** `KeyboardEvent.code` -> Linux evdev keycode. */
export const EVDEV = {
  Escape: 1, Digit1: 2, Digit2: 3, Digit3: 4, Digit4: 5, Digit5: 6, Digit6: 7,
  Digit7: 8, Digit8: 9, Digit9: 10, Digit0: 11, Minus: 12, Equal: 13,
  Backspace: 14, Tab: 15, KeyQ: 16, KeyW: 17, KeyE: 18, KeyR: 19, KeyT: 20,
  KeyY: 21, KeyU: 22, KeyI: 23, KeyO: 24, KeyP: 25, BracketLeft: 26,
  BracketRight: 27, Enter: 28, ControlLeft: 29, KeyA: 30, KeyS: 31, KeyD: 32,
  KeyF: 33, KeyG: 34, KeyH: 35, KeyJ: 36, KeyK: 37, KeyL: 38, Semicolon: 39,
  Quote: 40, Backquote: 41, ShiftLeft: 42, Backslash: 43, KeyZ: 44, KeyX: 45,
  KeyC: 46, KeyV: 47, KeyB: 48, KeyN: 49, KeyM: 50, Comma: 51, Period: 52,
  Slash: 53, ShiftRight: 54, NumpadMultiply: 55, AltLeft: 56, Space: 57,
  CapsLock: 58, F1: 59, F2: 60, F3: 61, F4: 62, F5: 63, F6: 64, F7: 65, F8: 66,
  F9: 67, F10: 68, F11: 87, F12: 88, ControlRight: 97, AltRight: 100,
  Home: 102, ArrowUp: 103, PageUp: 104, ArrowLeft: 105, ArrowRight: 106,
  End: 107, ArrowDown: 108, PageDown: 109, Insert: 110, Delete: 111,
  MetaLeft: 125, MetaRight: 126,
};

/** xkb modifier-mask bits, matching the "complete" types the keymap includes. */
export const MOD_SHIFT = 1;
export const MOD_LOCK = 2; // Caps
export const MOD_CONTROL = 4;
export const MOD_MOD1 = 8; // Alt
export const MOD_MOD4 = 64; // Super

/** A keyboard or pointer event's modifiers as an xkb mask. */
export const modsOf = (e) =>
  (e.shiftKey ? MOD_SHIFT : 0) | (e.ctrlKey ? MOD_CONTROL : 0) |
  (e.altKey ? MOD_MOD1 : 0) | (e.metaKey ? MOD_MOD4 : 0) |
  (e.getModifierState?.('CapsLock') ? MOD_LOCK : 0);

/** Browser mouse buttons (0 left, 1 middle, 2 right, 3 back, 4 forward) to
 *  Linux `BTN_*` codes. A button not here is not sent: it is no left click. */
export const BTN = { 0: 0x110, 1: 0x112, 2: 0x111, 3: 0x113, 4: 0x114 };
/** The same buttons' bits in a pointer event's `buttons`. */
export const BIT = { 0: 1, 1: 4, 2: 2, 3: 8, 4: 16 };
