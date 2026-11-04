import type { System } from './index';

/** A click on a window whose program asks for a locked pointer takes the
 *  browser's pointer lock. Events: `lock`, `unlock`. */
export declare class PointerLock extends EventTarget {
  constructor(sys: System);
  get locked(): boolean;
  release(): void;
  close(): void;
}

/** Text copied and pasted between the page and the guest's programs.
 *  Events: `copy` (`text`). */
export declare class Clipboard extends EventTarget {
  /** `sync` (default true): with the browser's clipboard too. */
  constructor(sys: System, options?: { sync?: boolean });
  get text(): string;
  setText(text: string): void;
  close(): void;
}

/** The browser's game controllers as `/dev/input/event0` to `event3`.
 *  Events: `connect` (`slot`, `gamepad`), `disconnect` (`slot`, `gamepad`: `{ id, mapping }`). */
export declare class Gamepads extends EventTarget {
  /** `source` replaces `navigator.getGamepads`. */
  constructor(sys: System, options?: { source?: () => ArrayLike<Gamepad | null> });
  /** For `spawn`'s environment: `SDL_JOYSTICK_DISABLE_UDEV=1`. */
  get env(): Record<string, string>;
  get pads(): { slot: number; id: string; mapping: string }[];
  /** Look at the controllers now (done every frame anyway). */
  poll(): void;
  close(): void;
}

/** Wine's game controller bus (winebus), which a Wine install with
 *  controllers starts on demand only, started (`sc start winebus` with
 *  `wine` and `env`) the first time `gamepads` has a controller, once per
 *  system. Events: `start` (`code`: 0 when the driver started). */
export declare class WineControllers extends EventTarget {
  constructor(sys: System, gamepads: Gamepads, options: { wine: string; env?: Record<string, string> | string[] });
  get state(): 'idle' | 'starting' | 'started';
  close(): void;
}
