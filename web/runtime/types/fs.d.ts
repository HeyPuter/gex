import type { System, Source } from './index';

export type FileType = 'file' | 'directory' | 'symlink' | 'chardev' | 'fifo' | 'socket';

export interface Stat {
  type: FileType;
  mode: number;
  size: number;
  ino: number;
  /** ms since the epoch. */
  mtime: number;
  nlink: number;
}

export interface WatchEvent {
  type: 'create' | 'change' | 'attrib' | 'delete' | 'move-out' | 'move-in' | 'gone';
  path: string;
  /** The two halves of one rename share it. */
  cookie?: number;
}

/** A directory the page serves. Paths are inside it (`/`, `/a/b`). Errors
 *  carry a `code` ('ENOENT', ...) or an `errno`. */
export interface MountProvider {
  stat(path: string): Promise<{ type: 'file' | 'directory' | 'symlink'; size: number; mode?: number; mtime: number | Date; ino?: number }>;
  readdir(path: string): Promise<({ name: string; type: string } | string)[]>;
  open(path: string, flags: { read?: boolean; write?: boolean; create?: boolean; truncate?: boolean; exclusive?: boolean; append?: boolean; mode?: number }): Promise<unknown>;
  read(handle: unknown, offset: number, length: number): Promise<Uint8Array>;
  write(handle: unknown, offset: number, bytes: Uint8Array): Promise<number>;
  close(handle: unknown): Promise<void>;
  mkdir(path: string, mode: number): Promise<void>;
  unlink(path: string): Promise<void>;
  rename(from: string, to: string): Promise<void>;
  lstat?(path: string): ReturnType<MountProvider['stat']>;
  rmdir?(path: string): Promise<void>;
  truncate?(path: string, length: number, handle?: unknown): Promise<void>;
  chmod?(path: string, mode: number): Promise<void>;
  utimes?(path: string, mtime: number): Promise<void>;
  readlink?(path: string): Promise<string>;
  symlink?(target: string, path: string): Promise<void>;
}

/** Events: `unmounted`. */
export declare class Mount extends EventTarget {
  readonly path: string;
  readonly provider: MountProvider;
  /** Resolves once programs see it. */
  readonly ready: Promise<Mount>;
  unmount(): Promise<void>;
}

/** Files fetched when first read: `[[path, size, url, mode?, mtime?], ...]`, an ejected site's
 *  lazy.json (`{ base, files: [[path, size, url, mtime?, mode?], ...] }`, each url relative to
 *  `base`), or the URL of either as JSON. */
export type LazyList =
  | [string, number, string, number?, number?][]
  | { base?: string; files: [string, number, string, (number | null)?, (number | null)?, ...unknown[]][] }
  | string
  | URL;

/** Abstract: a page makes a MemoryFilesystem, an OPFSFilesystem, or a
 *  subclass of its own that implements the store's methods (`new
 *  Filesystem(sys)` throws). */
export declare abstract class Filesystem extends EventTarget {
  /** `cache`: MiB of the store held in the system's memory (default 256, 16 to 4096);
   *  `writeback`: ms a written block waits (default 5000); `exclude`: more
   *  absolute paths the store does not keep across loads. */
  constructor(sys: System, options?: { cache?: number; writeback?: number; exclude?: string[] });
  /** The store, which a subclass implements: one array of bytes, the root a
   *  load before kept. Optional: ready it before boot (empty it to start over). */
  open(): Promise<void>;
  /** `length` bytes at `offset`; shorter past the end (zeros). */
  read(offset: number, length: number): Promise<Uint8Array>;
  write(offset: number, bytes: Uint8Array): Promise<void>;
  size(): Promise<number>;
  /** An error with `code: 'ENOSPC'` when the store is full. */
  truncate(length: number): Promise<void>;
  flush(): Promise<void>;
  addTar(src: Source | null, options?: { at?: string; lazy?: LazyList }): Promise<unknown>;
  addFiles(files: Record<string, Source>, options?: { mode?: number }): Promise<unknown>;
  mount(path: string, provider: MountProvider): Mount;
  unmount(path: string): Promise<void>;
  mounts(): Mount[];
  readFile(path: string): Promise<Uint8Array>;
  readFile(path: string, options: { encoding: string }): Promise<string>;
  writeFile(path: string, data: string | Uint8Array, options?: { mode?: number; append?: boolean; exclusive?: boolean }): Promise<void>;
  stat(path: string, options?: { follow?: boolean }): Promise<Stat>;
  readdir(path: string): Promise<{ name: string; type: FileType }[]>;
  mkdir(path: string, options?: { recursive?: boolean; mode?: number }): Promise<void>;
  rm(path: string, options?: { recursive?: boolean; force?: boolean }): Promise<void>;
  rename(from: string, to: string): Promise<void>;
  symlink(target: string, path: string): Promise<void>;
  readlink(path: string): Promise<string>;
  chmod(path: string, mode: number): Promise<void>;
  /** Returns a function that stops watching. */
  watch(path: string, fn: (e: WatchEvent) => void): () => void;
}

/** File data in the system's memory; writes past `max` bytes fail with ENOSPC. */
export declare class MemoryFilesystem extends Filesystem {
  constructor(sys: System, options?: { max?: number });
  read(offset: number, length: number): Promise<Uint8Array>;
  write(offset: number, bytes: Uint8Array): Promise<void>;
  size(): Promise<number>;
  truncate(length: number): Promise<void>;
  flush(): Promise<void>;
}

/** The root kept in the page's OPFS, in the store `name`, with a cache of `cache` MiB in memory (default 256). */
export declare class OPFSFilesystem extends Filesystem {
  constructor(sys: System, options: { name: string; cache?: number; writeback?: number; exclude?: string[] });
  /** Forget what the store `name` keeps (not while a system holds it). */
  static erase(name: string): Promise<void>;
  /** Forget what this filesystem's store keeps: before boot, or once its system stopped. */
  erase(): Promise<void>;
  read(offset: number, length: number): Promise<Uint8Array>;
  write(offset: number, bytes: Uint8Array): Promise<void>;
  size(): Promise<number>;
  truncate(length: number): Promise<void>;
  flush(): Promise<void>;
}
