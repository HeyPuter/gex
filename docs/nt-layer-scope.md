# NT / win32u layer: scope measurement

Stream S8 of the pewasm → gex merge (`merge-plan.md` §1, §3, §6 M6).
Written 2026-09-30. Research only: nothing here is implemented.

**Question.** How much of Wine's syscall boundary does gex-kernel have to
implement natively, so that Wine's PE DLLs (ntdll.dll, kernel32, kernelbase,
user32, gdi32, …) run unchanged on top? Natively means the NT syscalls that
PE `ntdll.dll` issues, the object/handle/wait/message-queue model that
wineserver provides today, and the win32u `NtUser*`/`NtGdi*` subset.

## Headline numbers

| | Wine 11.15 implements | winemine + small GUI + Noita need | Noita alone | all 11 runs |
| --- | ---: | ---: | ---: | ---: |
| NT syscalls (`ntdll` table) | 270 | **68** | 67 | 69 |
| win32u syscalls (`NtUser*`/`NtGdi*`) | 484 (of 1540 numbered) | **98** | 73 | 155 |
| `NtUserCall*`/`NtUserMessageCall` sub-codes | ~85 | 23 | 20 | 41 |
| user-mode callbacks (user32 `KeUserModeCallback` ids) | 25 | 9 | 8 | 10 |
| wineserver requests (proxy for the object model) | 308 | **122** | 112 | 131 |

Wine source lines behind the calls in the middle column. Only function
bodies are counted, from Wine's unix `ntdll`, unix `win32u` and wineserver.
"Direct" counts the entry points only. "Reachable" adds everything the entry
points reference transitively, which is an upper bound.

| | direct | reachable (upper bound) | whole component |
| --- | ---: | ---: | ---: |
| `dlls/ntdll/unix` | 5.7k | 24.7k | 36.6k |
| `dlls/win32u` (with `dibdrv`) | 5.5k | 60.8k | 79.6k |
| `server/` | 3.8k | 24.0k | 38.3k |
| `dlls/wow64*` thunks for these calls (32-bit ABI) | 3.7k | — | 13.9k |
| **total** | **18.7k** | **~110k** | **168k** |

**Estimated size of a gex-native implementation, in new Rust:**

- **Noita's path: ~21–29k lines.** That is the NT core, the 32-bit ABI,
  USER, and minimal GDI.
- **Winemine and notepad as well: ~31–42k lines.** They add the GDI
  rasterizer, fonts, menus and non-client painting.
- gex-kernel is 17.5k lines today, so the second figure more than doubles
  it.

The per-subsystem table is in §5. Agent-time: about 5–7 days for Noita
reaching its menu on the NT layer, and 4–6 more for winemine and notepad. M6
budgets 5–8 days, which also covers the PE loader and the per-DLL redirects,
so the win32u part of M6 is under-budgeted unless it is cut to Noita's needs
first (§8).

**Main findings**

1. **Noita is NT-heavy and USER-light.**
   - It uses 67 of the 68 NT calls in the union, but only 73 win32u calls,
     most of them message pumping and input.
   - Its GL traffic never crosses the syscall boundary. WGL goes through
     `opengl32`'s unixlib, which the planned opengl32 → gex GL redirect
     replaces anyway.
   - winemine is where GDI drawing, fonts and menus come in.
2. **The hot path is five calls.** Over ~506k NT calls in the two Noita
   runs:
   - `NtQueryPerformanceCounter`: 44%;
   - `NtWaitForAlertByThreadId` / `NtAlertThreadByThreadId`: 33%. These
     are Wine's futex, used by SRW locks, condition variables, critical
     sections and `WaitOnAddress`;
   - `NtReadFile`: 7%;
   - `NtDelayExecution`: 4%.

   These must be fast paths: futex, the clock, and VFS reads. They must
   not go through a general object dispatcher.
3. **Callbacks into the guest from inside a syscall are required.**
   - win32u calls user32 back 9 different ways, `NtUserCallWinProc` most
     often: 914 times across the union.
   - The call happens in the middle of `NtUserCreateWindowEx`,
     `NtUserMessageCall`, `NtUserDispatchMessage` and others.
   - gex-kernel today returns to the guest only by redirecting the PC
     (signal frames plus `rt_sigreturn`). The NT layer needs resumable
     syscalls or a nested guest loop. This is the one architectural
     decision to make before writing win32u code (§7).
4. **Wine's PE side reads kernel-maintained shared memory.**
   - gdi32 reads the GDI handle table (`PEB->GdiSharedHandleTable`) and
     the per-DC `DC_ATTR` directly.
   - kernelbase and ntdll read `KUSER_SHARED_DATA`.
   - The NT layer must keep these structures in guest memory with Wine's
     exact layouts. They explain why so few `NtGdi*` calls appear:
     `SetTextColor`, `MoveToEx` and friends never leave gdi32.
5. **A lot of what Wine's unix side does never shows up as a syscall.**
   Examples:
   - process bootstrap (PEB/TEB, process parameters, mapping ntdll and the
     exe, NLS);
   - the win32u font cache, which is ~23k registry requests per GUI
     process;
   - unixlib calls (GL, sockets and TLS for Noita's update check, DNS).

   The layer needs a native equivalent or a stub for each.

---

## 1. Method, versions, limitations

**Versions.**
- Host Wine `wine-11.15` (Arch package) in WoW64 mode: there is no
  `i386-unix`, and 32-bit PE runs through `wow64.dll`.
- The Wine source is a shallow clone of tag `wine-11.15` from
  gitlab.winehq.org, used for the counts and line measurements.
- MinGW gcc for the test programs. Xvfb for display.

**Isolation.**
- A scratch prefix in the session scratchpad, created with
  `WINEDLLOVERRIDES="mscoree,mshtml="` (no Mono or Gecko).
- Xvfb on `:147` (1280×720×24), with `winex11.drv` and `WAYLAND_DISPLAY`
  unset. No window reached the user's desktop.
- `FONTCONFIG_FILE` points at a 12-font config (`/usr/share/fonts/liberation`
  only). Without it, every Wine process spends ~3 s scanning the host's
  7.7k fonts.
- All runs used `taskset -c 0-7 nice -n 10`.
- Noita was copied into the scratchpad, so the tree in the repository was
  never written.

**Instrumentation.**
- `WINEDEBUG=+pid,+syscall,+loaddll`. In Wine 11, `+syscall` makes the
  syscall dispatcher log every `SysCall`/`SysRet` for both service tables
  (ntdll = table 0, win32u = table 1), plus every `UserCall`/`UserRet`, which
  are the `KeUserModeCallback`s into user32.
  - The dispatcher is the real boundary that PE code crosses, so this is
    exactly the call set the gex layer must serve.
  - For 32-bit programs the trace shows the 64-bit calls that `wow64.dll`
    makes after thunking. They are nearly 1:1 with the 32-bit calls, plus
    wow64's own start-up queries.
- `wineserver -f -d1` traces every server request. Requests are attributed
  to a program by thread id, counting only lines after the warm-up offset.
- `NtUserCall{NoParam,OneParam,TwoParam,Hwnd,HwndParam}` and
  `NtUserMessageCall` are multiplexers. Their sub-code argument is decoded
  against the enums in `include/ntuser.h`.

**Per-run procedure** (`run.sh`):
1. Kill the server and start `wineserver -d1`.
2. Warm up with a 64-bit GUI program that exits after five timer ticks.
   This brings up explorer (the desktop) and the services, so they are not
   counted.
3. Run the target traced, with a timeout.
4. Drive input with `xdotool` against Xvfb, where the run needs it.

**Aggregation.**
- `analyze.py` builds per-process census JSON. `report.py` builds the
  matrices and unions.
- `size.py` and `filegroups.py` do the line counts. They build a regex call
  graph over function bodies and ops or driver tables in `dlls/ntdll/unix`
  (x86-64 only; cdrom, tape and serial dropped), `dlls/win32u` (with
  `dibdrv`) and `server/` (without `trace.c`, `mach.c` and `ptrace.c`).
- The scripts, logs and JSON are in the session scratchpad, which is not
  kept.

**Limitations.**
- **One run per program.** Counts for message-loop and idle calls depend on
  timing, and on how long the program ran before the timeout or its own
  exit. The *sets* of calls are stable; the *counts* are indicative.
- **Noita** reached the release-notes screen, the main menu, and "New Game →
  first mode": the intro and world-generation screen. It did not reach
  controllable gameplay within the trace budget: llvmpipe plus `+syscall`
  is slow. Gameplay may add calls: more input, and perhaps XInput or
  DirectInput device polling, which goes through hid/`winebus` in
  `winedevice.exe`. Noita never exited cleanly, because the runs were
  killed by timeout, so its exit path was not traced.
  - Noita made a network update check: the "NEW UPDATE AVAILABLE" banner.
    That brought in `ws2_32`, `secur32`/`schannel`, `crypt32`, `dnsapi`,
    `kerberos` and one `NtDeviceIoControlFile` family (AFD sockets).
- **`/dev/ntsync` exists on this host**, so Wine 11 waits on events,
  mutexes and semaphores are in-process ioctls. The trace therefore shows
  `get_inproc_sync_fd` instead of most `select` requests. The NT-level wait
  semantics are fully visible in the syscall trace; only the server-side
  view is thinner.
- **Wine 11 keeps window, queue and desktop state in session shared
  memory**, which unix win32u reads without requests. Server request counts
  therefore understate what the USER model tracks.
- **Unixlib calls (`__wine_unix_call`) are not traced**: WGL and GL, socket
  helpers, TLS, DNS, and winex11 driver calls. Loaded DLLs with a unix side
  are listed per program instead.
- **Process start-up done inside unix ntdll is not a syscall**, and so is
  not in the tables. It is covered as its own line in §5.
- **`input32`** exited with code 9: DirectInput's `Acquire` fails without a
  foreground window on a WM-less Xvfb. It still covers window creation,
  fonts, glyph outlines and DirectInput set-up.
- **`present32`** exited with 3, because `wglGetProcAddress("wglSwapBuffers")`
  is NULL on Wine. That is a pewasm-host-specific check. It still created a
  WGL context and a pixel format.
- **The line counts are regex-based.**
  - "Reachable" over-approximates. Any identifier that names a function or
    ops table counts as a reference, so hub functions such as
    `send_message`, `NtUserCallOneParam`'s switch and the driver tables
    pull in most of win32u.
  - It also under-counts in one place: `dibdrv/primitives.c` is reached
    through `primitive_funcs` tables that the regex does not index.
  - Treat "direct" as a floor and "reachable" as a ceiling. §5 gives a
    judgement between them.

## 2. Programs and per-program summary

| program | built from | how it ended |
| --- | --- | --- |
| `con64` / `con32` | `con.c`: printf; 1 MiB malloc; file write, read and delete; 4 threads with `WaitForMultipleObjects`; critical section; event; registry read; `Sleep`; `FindFirstFile`; `VirtualAlloc`/`Protect`/`Free`. MinGW x86-64 and i686. | exit 0 |
| `cmd_echo` | `cmd /c echo hello` (Wine builtin, x64) | exit 0 |
| `gui64` / `gui32` | `gui.c`: `RegisterClass`, `CreateWindow`, 16 ms `SetTimer`; `WM_PAINT` with `FillRect`, `TextOut`, `StretchDIBits`, `LineTo`; mouse and key input via xdotool; `DestroyWindow` after 150 ticks | exit 0 |
| `input32` | `pewasm/examples/input32.c` (no CRT, `-e _entry`) | exit 9 (see §1) |
| `present32` | `pewasm/examples/present32.c` (WGL) | exit 3 (see §1) |
| `winemine` | Wine builtin (x64). Six left clicks, one right click, F2, Alt+F4. | exit 0 |
| `notepad` | Wine builtin (x64). Typing, Ctrl+A/C/V, Alt+O menu, Alt+F4, "don't save". | exit 0 |
| `noita_play` | `noita.exe` (PE32, msvcr120), 40 s timeout, clicked "Ok" to reach the main menu | killed at main menu |
| `noita_game` | same, 120 s: menu → New Game → first mode → intro and world generation | killed during intro |

`start.exe`, the launcher Wine uses for `wine foo.exe`, is excluded. So is
the `explorer.exe` desktop process: in gex the desktop window belongs to the
native layer.

| program | run | threads | NT distinct | NT calls | win32u distinct | win32u calls | callbacks distinct | server req. distinct | server req. calls |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| console, MinGW x64 | `con64` | 5 | 44 | 262 | 0 | 0 | 0 | 34 | 775 |
| console, MinGW i686 | `con32` | 5 | 46 | 365 | 0 | 0 | 0 | 34 | 792 |
| cmd /c echo (x64) | `cmd_echo` | 1 | 33 | 312 | 2 | 2 | 0 | 34 | 28610 |
| small GUI x64 | `gui64` | 1 | 34 | 747 | 54 | 5328 | 6 | 90 | 31658 |
| small GUI i686 | `gui32` | 1 | 37 | 2230 | 54 | 5328 | 6 | 87 | 31641 |
| pewasm input32 (i686, DirectInput) | `input32` | 1 | 38 | 846 | 41 | 3398 | 4 | 73 | 29306 |
| pewasm present32 (i686, WGL) | `present32` | 1 | 36 | 548 | 28 | 323 | 4 | 61 | 29084 |
| winemine (x64) | `winemine` | 1 | 34 | 639 | 65 | 2193 | 7 | 82 | 29691 |
| notepad (x64) | `notepad` | 1 | 34 | 1212 | 131 | 16044 | 8 | 90 | 32580 |
| Noita to main menu (i686) | `noita_play` | 34 | 65 | 74243 | 72 | 2021 | 7 | 110 | 37301 |
| Noita menu + new game intro (i686) | `noita_game` | 34 | 66 | 432061 | 73 | 3690 | 8 | 110 | 51786 |

- **"server req. calls" is dominated by registry traffic.** ≈23.4k of the
  ~29k requests per GUI process are `enum_key_value`, `get_key_value`,
  `enum_key`, `create_key`, `set_key_value` and the matching
  `close_handle`s. They come from unix win32u loading its font cache from
  `HKCU\Software\Wine\Fonts` at start-up. That is an internal detail of
  Wine: the program itself issued only 35–70 registry syscalls (Noita
  ~3.1–3.6k). Without registry requests, a small GUI process makes ~5–9k
  requests, and Noita's intro run ~25k.
- **gui32 vs gui64:** the same 54 win32u calls, with identical counts. The
  NT difference (2230 vs 747 calls) is wow64 start-up and thunking
  overhead.
- **Noita's DLL closure** has 62 modules. Those with a unix side are
  `opengl32`, `winex11.drv`, `ws2_32`, `secur32`, `crypt32`, `bcrypt`,
  `dnsapi`, `kerberos`, `msv1_0` and `netapi32`. It also loads `dinput8`,
  `hid`, `xinput1_4`, `setupapi`, `cfgmgr32` (SDL's device enumeration),
  `mmdevapi` and `winmm`. FMOD opened no audio device: `winepulse.drv` was
  never loaded.

Full per-program matrices of every call, with counts, are in the appendix
(A1–A5).

## 3. The union for "winemine + small GUI + Noita"

Runs: `winemine`, `gui64`, `gui32`, `noita_play`, `noita_game`.

| set | NT syscalls | win32u calls | NtUserCall*/MessageCall sub-codes | user callbacks | server requests |
| --- | ---: | ---: | ---: | ---: | ---: |
| winemine+gui+noita | 68 | 98 | 23 | 9 | 122 |
| noita only | 67 | 73 | 20 | 8 | 112 |
| all programs | 69 | 155 | 41 | 10 | 131 |

The "all programs" row adds notepad, input32, present32 and the console
programs.

- **What notepad adds:** clipboard (open, empty, get and set data, formats),
  carets, `SetScrollInfo`, font selection and glyph queries, regions and
  clipping, `DrawIconEx`, window placement, and dialog info. That is 57
  more win32u calls.
- **What winemine and gui add over Noita:** GDI drawing (`BitBlt`,
  `PatBlt`, `LineTo`, pens and brushes, `ExtTextOutW`, text metrics),
  `BeginPaint`/`EndPaint`/`RedrawWindow`, menus and accelerators, capture,
  `SetTimer`, `PostQuitMessage`, and `NtTerminateProcess`. Noita was
  killed, so it never terminated itself. That is 25 win32u calls.

**NT syscalls (68)**

| subsystem | n | calls |
| --- | ---: | --- |
| threads / processes / context | 13 | `NtAlertThreadByThreadId`, `NtCallbackReturn`, `NtContinue`, `NtCreateThreadEx`, `NtGetNextThread`, `NtOpenProcess`, `NtQueryInformationProcess`, `NtQueryInformationThread`, `NtRaiseException`, `NtResumeThread`, `NtSetInformationProcess`, `NtSetInformationThread`, `NtTerminateProcess` |
| files / I/O | 11 | `NtCreateFile`, `NtFsControlFile`, `NtNotifyChangeDirectoryFile`, `NtOpenFile`, `NtQueryAttributesFile`, `NtQueryDirectoryFile`, `NtQueryInformationFile`, `NtQueryVolumeInformationFile`, `NtReadFile`, `NtSetInformationFile`, `NtWriteFile` |
| sync / waits | 11 | `NtCreateEvent`, `NtCreateMutant`, `NtCreateSemaphore`, `NtDelayExecution`, `NtReleaseMutant`, `NtReleaseSemaphore`, `NtSetEvent`, `NtTestAlert`, `NtWaitForAlertByThreadId`, `NtWaitForMultipleObjects`, `NtWaitForSingleObject` |
| registry | 10 | `NtCreateKey`, `NtEnumerateKey`, `NtEnumerateValueKey`, `NtNotifyChangeKey`, `NtOpenKey`, `NtOpenKeyEx`, `NtQueryKey`, `NtQueryValueKey`, `NtRenameKey`, `NtSetValueKey` |
| system info / time / NLS | 6 | `NtInitializeNlsFiles`, `NtQueryDefaultLocale`, `NtQueryPerformanceCounter`, `NtQuerySystemInformation`, `NtQuerySystemInformationEx`, `NtQuerySystemTime` |
| sections / image mapping | 6 | `NtCreateSection`, `NtGetNlsSectionPtr`, `NtMapViewOfSection`, `NtOpenSection`, `NtQuerySection`, `NtUnmapViewOfSection` |
| virtual memory | 5 | `NtAllocateVirtualMemory`, `NtFlushInstructionCache`, `NtFreeVirtualMemory`, `NtProtectVirtualMemory`, `NtQueryVirtualMemory` |
| objects / handles / namespace | 3 | `NtClose`, `NtDuplicateObject`, `NtOpenDirectoryObject` |
| security / tokens | 1 | `NtQueryInformationToken` |
| atoms | 1 | `NtAddAtom` |
| sockets / device I/O | 1 | `NtDeviceIoControlFile` |

**win32u calls (98)**

| subsystem | n | calls |
| --- | ---: | --- |
| windows / window manager | 17 | `U.CallHwnd`, `U.CallHwndParam`, `U.CallOneParam`, `U.CallTwoParam`, `U.CreateAcceleratorTable`, `U.CreateWindowEx`, `U.DestroyWindow`, `U.GetProp`, `U.MoveWindow`, `U.QueryWindow`, `U.ReleaseCapture`, `U.SetProp`, `U.SetWindowLong`, `U.SetWindowLongPtr`, `U.SetWindowPos`, `U.ShowWindow`, `U.TranslateAccelerator` |
| GDI: DCs / objects / bitmaps | 15 | `G.CreateBitmap`, `G.CreateCompatibleBitmap`, `G.CreateCompatibleDC`, `G.CreatePatternBrushInternal`, `G.CreatePen`, `G.CreateSolidBrush`, `G.DeleteObjectApp`, `G.ExtGetObjectW`, `G.GetAndSetDCDword`, `G.GetDCDword`, `G.GetDeviceCaps`, `G.OpenDCW`, `G.SelectBitmap`, `G.SelectBrush`, `G.SelectPen` |
| keyboard / mouse / raw input | 11 | `U.ClipCursor`, `U.GetAsyncKeyState`, `U.GetCursorPos`, `U.GetDoubleClickTime`, `U.GetKeyState`, `U.GetKeyboardLayout`, `U.GetKeyboardState`, `U.MapVirtualKeyEx`, `U.SetCapture`, `U.ToUnicodeEx`, `U.TrackMouseEvent` |
| messages / queues / timers | 10 | `U.DispatchMessage`, `U.GetMessage`, `U.GetThreadState`, `U.MessageCall`, `U.MsgWaitForMultipleObjectsEx`, `U.PeekMessage`, `U.PostQuitMessage`, `U.RegisterWindowMessage`, `U.SetTimer`, `U.TranslateMessage` |
| display / monitors / sysparams / DPI | 9 | `U.EnumDisplayDevices`, `U.EnumDisplayMonitors`, `U.EnumDisplaySettings`, `U.GetProcessDefaultLayout`, `U.GetProcessDpiAwarenessContext`, `U.GetSystemDpiForProcess`, `U.SetProcessDefaultLayout`, `U.SetProcessDpiAwarenessContext`, `U.SystemParametersInfo` |
| GDI: blits / DIBs / drawing | 8 | `G.BitBlt`, `G.CreateDIBSection`, `G.GetDIBitsInternal`, `G.LineTo`, `G.MoveTo`, `G.PatBlt`, `G.StretchBlt`, `G.StretchDIBitsInternal` |
| painting / DCE / update regions | 8 | `U.BeginPaint`, `U.EndPaint`, `U.GetDC`, `U.GetUpdateRect`, `U.InvalidateRect`, `U.RedrawWindow`, `U.ReleaseDC`, `U.ValidateRect` |
| menus | 4 | `U.CheckMenuItem`, `U.CreateMenu`, `U.CreatePopupMenu`, `U.ThunkedMenuItemInfo` |
| IME / input context | 4 | `U.AssociateInputContext`, `U.BuildHimcList`, `U.QueryInputContext`, `U.UpdateInputContext` |
| cursors / icons | 3 | `U.FindExistingCursorIcon`, `U.SetCursor`, `U.SetCursorIconData` |
| GDI: fonts / text | 3 | `G.ExtTextOutW`, `G.GetTextExtentExW`, `G.GetTextMetricsW` |
| window classes / atoms | 2 | `U.GetClassInfoEx`, `U.RegisterClassExWOW` |
| hooks / events | 2 | `U.CallNextHookEx`, `U.SetWindowsHookEx` |
| window stations / desktops | 1 | `U.InitializeClientPfnArrays` |
| clipboard | 1 | `U.GetClipboardSequenceNumber` |

**wineserver requests (122)**

| subsystem | n | calls |
| --- | ---: | --- |
| message queues / input | 25 | `accept_hardware_message`, `finish_hook_chain`, `get_hook_info`, `get_key_state`, `get_message`, `get_message_reply`, `get_msg_queue`, `get_msg_queue_handle`, `get_thread_input`, `kill_win_timer`, `post_quit_message`, `send_hardware_message`, `send_message`, `set_active_window`, `set_capture_window`, `set_caret_info`, `set_cursor`, `set_focus_window`, `set_foreground_window`, `set_hook`, `set_key_state`, `set_queue_fd`, `set_queue_mask`, `set_win_timer`, `start_hook_chain` |
| windows / window tree | 22 | `create_window`, `destroy_window`, `get_desktop_window`, `get_update_region`, `get_visible_region`, `get_window_children_from_point`, `get_window_info`, `get_window_layered_info`, `get_window_list`, `get_window_property`, `get_window_rectangles`, `get_window_region`, `get_window_tree`, `init_window_info`, `redraw_window`, `remove_window_property`, `set_parent`, `set_window_info`, `set_window_pos`, `set_window_property`, `set_window_text`, `update_window_zorder` |
| threads / processes | 21 | `get_new_process_info`, `get_next_thread`, `get_process_debug_info`, `get_process_info`, `get_process_winstation`, `get_startup_info`, `get_thread_context`, `get_thread_desktop`, `get_thread_info`, `get_thread_times`, `init_first_thread`, `init_process_done`, `init_thread`, `new_process`, `new_thread`, `open_process`, `resume_thread`, `set_process_winstation`, `set_thread_desktop`, `set_thread_info`, `terminate_process` |
| files / fds / async I/O / sockets | 16 | `alloc_file_handle`, `create_file`, `get_async_result`, `get_directory_cache_entry`, `get_handle_fd`, `ioctl`, `open_file_object`, `read`, `read_directory_changes`, `recv_socket`, `send_socket`, `set_async_direct_result`, `set_fd_eof_info`, `set_fd_name_info`, `set_named_pipe_info`, `write` |
| USER: classes / atoms / winstations / clipboard / handles | 11 | `add_atom`, `add_user_atom`, `alloc_user_handle`, `create_class`, `create_desktop`, `create_winstation`, `free_user_handle`, `get_atom_information`, `get_clipboard_info`, `set_user_object_info`, `set_winstation_monitors` |
| registry | 8 | `create_key`, `enum_key`, `enum_key_value`, `get_key_value`, `open_key`, `rename_key`, `set_key_value`, `set_registry_notification` |
| sync objects | 8 | `create_event`, `create_keyed_event`, `create_mutex`, `create_semaphore`, `get_inproc_alert_fd`, `get_inproc_sync_fd`, `get_process_idle_event`, `select` |
| sections / mappings | 7 | `create_mapping`, `get_image_map_address`, `get_mapping_info`, `map_image_view`, `map_view`, `open_mapping`, `unmap_view` |
| objects / handles / namespace | 3 | `close_handle`, `dup_handle`, `open_directory` |
| security / tokens | 1 | `get_token_sid` |

`U.` = `NtUser`, `G.` = `NtGdi`. The multiplexer sub-codes used by the
union are:
- `MessageCall`: `GetDispatchParams`, `DefWindowProc`, `CallWindowProc`,
  `SendMessage`, `SpyGetMsgName`;
- `CallHwnd`: `IsCurrentProcessWindow`, `GetWindowInputContext`,
  `IsWindow`, `IsWindowUnicode`;
- `CallHwndParam`: `GetClientRect`, `GetWindowLongW`, `GetWindowLongPtrW`,
  `GetWindowLongA`, `ClientToScreen`, `ScreenToClient`, `GetWindowRect`;
- `CallOneParam`: `GetSysColor`, `GetSystemMetrics`, `CreateCursorIcon`;
- `CallTwoParam`: `AdjustWindowRect`, `GetMonitorInfo`, `MonitorFromRect`,
  `GetMenuInfo`.

The user-mode callbacks are:
- `NtUserCallWinProc` (914 calls);
- `NtUserImmProcessKey` (140);
- `NtUserLoadImage` (67);
- `NtUserDrawText` (10);
- `NtUserLoadDriver`, `NtUserInitBuiltinClasses`, `NtUserLoadSysMenu`,
  `NtUserCallEnumDisplayMonitor`, `NtUserCallWindowsHook`.

## 4. Noita's hot path

NT calls summed over `noita_play` and `noita_game` (≈506k in total). Under
`+syscall` the game runs far below real time, so absolute rates are not
meaningful. The ranking is.

| call | count | share | what it is | gex mapping |
| --- | ---: | ---: | --- | --- |
| `NtQueryPerformanceCounter` | 222,090 | 43.9% | `QueryPerformanceCounter`. In WoW64 it is a real syscall. The 64-bit programs read `KUSER_SHARED_DATA` instead (con64: 1 call). | Serve it from the shared-data page or a trivially cheap syscall path. It must not take a lock. |
| `NtWaitForAlertByThreadId` | 83,274 | 16.4% | Wine's futex wait: SRW locks, condition variables, `RtlWaitOnAddress`, critical-section contention | `futex.rs` wait keyed by the thread id. `Atomics.wait` in the browser. |
| `NtAlertThreadByThreadId` | 83,241 | 16.4% | futex wake | futex wake |
| `NtReadFile` | 33,645 | 6.6% | data/asset reads, mostly small, plus `FilePositionInformation` seeks through `NtSetInformationFile` (~2k) | VFS read. Synchronous files need no async machinery. |
| `NtDelayExecution` | 20,692 | 4.1% | `Sleep(0/1)` in worker loops | `sched_yield`/`nanosleep` |
| `NtWaitForSingleObject` | 15,993 | 3.2% | job-system semaphores and events | the generic wait path |
| `NtClose`, `NtDuplicateObject` | 9,767, 6,577 | 3.2% | duplicating the `GetCurrentThread()` pseudo-handle in a loop | the handle table |
| `NtAllocateVirtualMemory`, `NtReleaseSemaphore` | 5,843, 5,184 | 2.2% | heap growth, the job system | `mem.rs`; the semaphore object |

Everything else is below 1%. On the win32u side Noita is light. The intro
run made ~850 `PeekMessage`, ~340 `TranslateMessage` and ~230
`DispatchMessage` calls, plus keyboard-layout and keyboard-state queries.
It created 8 windows: SDL's top-level window, helper windows and the IME
default windows.

## 5. Size estimate per subsystem

Wine lines are counted for the union of winemine, small GUI and Noita. For
`reachable`, each file is assigned to one subsystem, and the regex closure
from all needed entry points is split across those files. The gex estimate
is new Rust for the observed subset. Where noted, it maps onto existing
gex-kernel pieces.

### 5.1 NT side (ntdll unix + the matching wineserver parts)

| subsystem | calls needed | Wine direct (entry points + server handlers) | Wine reachable / file total | gex estimate | notes |
| --- | ---: | ---: | --- | ---: | --- |
| Process bootstrap (not syscalls) | — | — | part of 8.3k below | 1.0–1.5k | PEB, TEB, `RTL_USER_PROCESS_PARAMETERS`, environment, `KUSER_SHARED_DATA`, mapping ntdll and the exe (the gex-ld PE loader from S5), NLS files, entry through `LdrInitializeThunk` |
| Virtual memory + sections + NLS | 11 | 740 | 5.9k / 7.5k | 1.5–2k | reserve/commit, 64 KiB granularity, `MEMORY_BASIC_INFORMATION`, image and anonymous sections, `NtGetNlsSectionPtr` |
| Files / I/O | 11 | 2.1k | 11.7k / 16.6k | 2–3k | NT path → VFS path (`\??\C:\`, `Z:`, case-insensitive lookup), ~6 information classes, `NtQueryDirectoryFile` with wildcards, volume info, `NtFsControlFile` (few codes), change notification as a stub |
| Sockets (AFD over `NtDeviceIoControlFile`) | 1 | 45 | 3.1k / 6.3k | 0 now (fail it). ~1.5k later. | Only Noita's update check uses it. Later it maps onto `net.rs`. |
| Sync / waits | 11 | 560 | 2.1k / 4.1k | 1–1.5k | events, mutants, semaphores, keyed events; wait-any, wait-all, alertable waits and timeouts; futex-style alert-by-thread-id; must also wait on message queues (`MsgWaitForMultipleObjectsEx`) |
| Threads / processes / exceptions / APCs / callbacks | 13 | 2.3k | 8.3k / 15.0k | 2.5–3.5k | `NtCreateThreadEx`, TEB and stack; ~15 information classes; `NtContinue`/`NtRaiseException` with the `KiUserExceptionDispatcher` frame; APC delivery; the `KiUserCallbackDispatcher` + `NtCallbackReturn` pair (§7) |
| Registry | 10 | 420 | 1.7k / 2.8k | 0.8–1.2k | An in-kernel key tree loaded from Wine-format `system.reg`/`user.reg` (a baked default prefix). `NtNotifyChangeKey` is needed: Noita uses it 15 times. |
| System info / time / NLS | 6 | 970 | 3.3k / 4.2k | 0.5–0.8k | ~8 `SystemInformationClass` values (Noita polls class 3, time-of-day, 249 times). Locale is fixed. |
| Objects / handles / namespace / security / atoms | 5 | 550 | 2.7k / 5.2k | 0.8–1.2k | Per-process handle table with cross-process duplicate. Object directory (`\BaseNamedObjects`, `\KnownDlls`, `\Sessions\1`). A fixed token (`NtQueryInformationToken`: user SID, groups, elevation). Global atoms. |
| Server IPC plumbing | — | — | 1.5k / 3.1k | 0 | Goes away: requests become direct calls |
| 32-bit syscall ABI (the wow64 equivalent) | all | 3.7k (wow64 C) | — / 13.9k | 2–3k | Struct and pointer thunking for 68 NT + 98 win32u calls. The alternative is to run Wine's `wow64.dll`/`wow64win.dll`/`wow64cpu.dll` PE unchanged; that needs mixed x86-64 and i386 code in one gex process (far-call CS switches). |
| **NT subtotal** | 68 | 7.7k + 3.7k wow64 | 40.4k / 64.7k (+13.9k wow64) | **12–18k** | |

### 5.2 win32u side (win32u unix + wineserver's USER parts)

| subsystem | calls needed | Wine direct | Wine reachable / file total | gex estimate (Noita / + winemine) | notes |
| --- | ---: | ---: | --- | ---: | --- |
| Window manager: tree, styles, rectangles, z-order, show/position, properties, classes, window stations and desktops | 20 | 2.0k | 14.2k / 16.2k | 2.5–3.5k / +1k | Top-level windows become gex-wayland surfaces. Generated messages (`WM_NCCREATE`…`WM_WINDOWPOSCHANGED`). `DefWindowProc` basics. |
| Messages / queues / timers / input / hooks / IME | 27 | 2.0k | 11.6k / 13.2k | 3–4k / +0.5k | Per-thread queues, posted and sent messages (cross-thread send with reply), filters, synthesized `WM_PAINT`/`WM_TIMER`, a hardware input queue fed by the gex compositor, key-state tables, `ToUnicodeEx`/`MapVirtualKeyEx` with a fixed US layout, 2 hook calls, IME contexts as stubs |
| Painting / DCE / regions | 8 | 190 | 4.9k / 5.3k | 0.5k / +1.5k | `GetDC`/`ReleaseDC` only for Noita. For winemine: update regions, `BeginPaint`, visible regions (clip children and siblings), region algebra. |
| Sysparams / display / monitors / DPI / cursors / clipboard | 13 | 1.3k | 6.1k / 8.9k | 1–1.5k / +0.3k | One fixed monitor and mode, SPI values, system metrics and colours, cursor icons as data |
| Menus | 4 | 120 | 4.0k / 4.2k | 0 / 2–3k | Menu bar drawing, tracking and accelerators are all in win32u |
| GDI: DCs and objects | 15 | 680 | 6.0k / 8.4k | 1.5k / +1k | Must maintain `GDI_SHARED_MEMORY` (the handle table gdi32 reads) and `DC_ATTR` in guest memory. Bitmaps, DIB sections, brushes, pens. |
| GDI: DIB rasterizer | 8 | 530 | 5.3k (+ some of `primitives.c`'s 7.7k) / 13.0k | 0.3k / 2–3k | Noita only needs `StretchDIBits`/`StretchBlt` for cursors. A 32 bpp-only rasterizer suffices, or Wine's `dibdrv` C can be compiled to wasm. |
| GDI: fonts and text | 3 (+`NtUserDrawText` callback) | 460 | 8.6k / 9.1k | 0 / 1.5–2.5k + FreeType | Needed for winemine's and notepad's text, captions and menus. FreeType as C → wasm. |
| GPU: WGL, Vulkan, D3DKMT | 0 syscalls | — | 6.5k / 8.0k | 0 | Reached through `opengl32`'s unixlib, not syscalls. Replaced by the M6 opengl32 → gex GL redirect. |
| Callback machinery | 9 callback ids | — | — | 0.3–0.5k | Depends on the §7 decision |
| **win32u subtotal** | 98 | 7.3k | 69.2k / 89.8k (with the server's USER files) | **9–12k / +9–13k** | |

**Totals.**
- **Noita:** NT 12–18k + win32u 9–12k ≈ **21–29k**.
- **Winemine and notepad as well:** **~31–42k**.
- The main uncertainties are three. Menus and non-client painting could be
  borrowed rather than rewritten. The fonts and rasterizer could be reused
  as C. The 32-bit ABI could be reused through wow64 PE.

**A reuse option worth pricing before M6 starts.** A large part of win32u
(`window.c`, `message.c`, `defwnd.c`, `menu.c`, `dibdrv`, `font.c`/
`freetype.c`) is portable C. It talks to the outside world only through
wineserver requests, session shared memory and the `user_driver` table. If
gex implemented the ~60–70 USER-side requests natively (roughly
`queue.c` + `window.c` + `winstation.c`, about 6–7k Wine lines, maybe 3–4k
Rust), win32u could be compiled to wasm and linked as a native module, the
way pewasm links SDL and Lua. That would replace most of §5.2's +9–13k with
the request layer plus a gex `user_driver`. It trades clean ownership for
Wine-sized C inside the runtime. The plan's decision is "native layer, no
wholesale port", so this is only flagged here, not recommended.

## 6. What maps onto gex-kernel, and what is new

**Maps naturally** (gex-kernel today: `mem.rs`, `futex.rs`, `vfs/`, `file.rs`,
`proc.rs`, `signal.rs`, `net.rs`, `timerfd.rs`, `eventfd.rs`):

| NT concept | gex-kernel piece | gap |
| --- | --- | --- |
| `Nt*VirtualMemory` | `mem.rs` mmap, mprotect, munmap | reserve vs commit, 64 KiB allocation granularity, Windows region queries (`AllocationBase`, `State`, `Type`), guard pages |
| `Nt*Section`, `NtMapViewOfSection` | mmap of files and shared anonymous memory; gex-ld PE images (S5) | image-section semantics (relocation on conflict, per-section protections), and shared views across processes |
| `NtCreateFile`, `NtReadFile`, `NtWriteFile`, … | `vfs/` (hostfs, tmpfs), `file.rs` | NT paths and DOS devices, case-insensitive lookup, information classes, `FILE_SHARE_*` checks, overlapped I/O with `IO_STATUS_BLOCK` + event + APC completion |
| `NtWaitForAlertByThreadId` / `NtAlertThreadByThreadId` | `futex.rs` | none: keyed by thread instead of address |
| Events, semaphores, mutants, waits | futex + `eventfd.rs`-style objects | wait-all across heterogeneous objects, abandoned mutants, alertable waits, waits that include a message queue |
| `NtCreateThreadEx`, terminate, suspend, resume | `proc.rs` threads (Worker per thread) | TEB and stack set-up, suspend counts, thread information classes |
| Exceptions, `NtContinue`, APCs | `signal.rs` frame set-up + sigreturn redirect | Windows `CONTEXT` layouts for x86-64 and i386, `EXCEPTION_RECORD`, `KiUser*Dispatcher` entry points, first-chance dispatch into PE ntdll |
| `NtDelayExecution`, time, QPC | `nanosleep`, `system_clock` | keeping `KUSER_SHARED_DATA` time fields updated |
| AFD sockets | `net.rs` | the AFD ioctl protocol (later) |

**New** (no Linux analogue in gex-kernel):
- **The NT object manager.** Typed objects, a per-process handle table with
  access masks and cross-process duplicate, a named object directory,
  symbolic links, security descriptors as mostly-stub data, and tokens.
- **The registry.** A hierarchical key/value store with notification,
  persisted to Wine's `.reg` format.
- **The USER window manager.** The window tree, z-order, activation, focus,
  foreground, capture, window rectangles and visible regions, window
  properties, classes, window stations and desktops. Mapping top-level
  windows to gex-wayland surfaces is the only part that touches existing
  gex code (`gex-wayland`, `gex-xwm`).
- **Message queues.** Posted, sent and hardware queues, cross-thread sends
  with replies, timers, synthesized messages, hooks, key state, and
  integration with waits.
- **GDI.** Object handles with the shared handle table, DCs with `DC_ATTR`,
  bitmaps and DIB sections, regions, a software rasterizer and fonts.
- **User-mode callbacks.** A syscall that calls back into the guest several
  times before it returns (§7).
- **The 32-bit NT ABI.** Thunking for i386 guests.

## 7. Architectural issues found

1. **Nested callbacks (`KeUserModeCallback`).** In Wine, win32u calls
   user32 in the middle of a syscall. For example, `NtUserCreateWindowEx`
   sends `WM_NCCREATE`, `WM_CREATE` and so on through `NtUserCallWinProc`.
   The guest code handles each one and returns with `NtCallbackReturn`, and
   the original syscall then continues. Callbacks nest arbitrarily:
   `SendMessage` inside a window procedure inside `DispatchMessage`. The
   options:
   - **(a) A nested guest loop.** The kernel handler re-enters the gex
     execution loop recursively on the host stack. Windows has no
     `fork`/`execve`, so the flat-stack invariants gex needs for those
     (merge-plan §4) are not at risk. Signals and thread migration inside a
     callback still need care.
   - **(b) Resumable handlers.** Every win32u path that calls back is
     written as a continuation or state machine. This is expensive: every
     message-sending path calls back.
   - **(c) Stack switching.** Run win32u handlers on their own host
     coroutine (a native thread or a JSPI or stack-switching continuation
     in the browser).

   (a) is the cheapest, and it matches how the function tier already nests
   guest calls on the Wasm stack. Decide before M6's win32u work starts.
2. **Shared memory the PE side reads.** `KUSER_SHARED_DATA` at 0x7ffe0000
   (time, tick count, processor features, NT version), the GDI shared handle
   table, and per-DC `DC_ATTR`. These are ABI with Wine's PE DLLs and must
   match Wine 11's layouts exactly. Pin the Wine version that the PE DLLs
   come from.
3. **Wine version coupling.**
   - win32u's `NtUserCall*` sub-codes are "not compatible with Windows"
     (`ntuser.h`), and change between Wine releases.
   - So do the callback ids and the `KeUserModeCallback` parameter structs.
   - The gex layer is therefore tied to one Wine release's PE DLLs.
     Vendor them.
4. **Helper processes.** Wine normally runs `services.exe`, `winedevice.exe`
   (hid/`winebus`, `nsiproxy`), `plugplay.exe`, `rpcss.exe` and
   `explorer.exe /desktop`.
   - Noita's closure loads `setupapi`, `cfgmgr32`, `hid`, `xinput1_4`,
     `dinput8` and `iphlpapi`/`nsi`, all of which expect those services.
   - With the M6 redirects (dinput/xinput → Gamepad API) and stubs
     (`nsi`, device enumeration returns nothing), none of the helpers is
     needed.
   - The desktop window and its window procedure should be native, not an
     `explorer.exe` process.
5. **Unixlibs are a second boundary.** Some PE DLLs reach their unix side
   through `__wine_unix_call`, which does not go through the NT layer: for
   Noita, `opengl32`, `ws2_32`, `secur32`, `crypt32`, `bcrypt`, `dnsapi`,
   `kerberos`, `msv1_0` and `netapi32`. Each needs a per-DLL redirect or a
   stub that returns "unavailable". For Noita, only `opengl32` has to work.
6. **WoW64.** Noita is PE32. Wine 11's 32-bit PE DLLs, from
   `/usr/lib/wine/i386-windows`, issue 32-bit syscalls. gex either
   implements the 32-bit NT ABI (the thunks: ~2–3k Rust) or supports
   running Wine's x86-64 wow64 DLLs next to i386 code in one process.
   Implementing the thunks natively is simpler, and avoids wow64's extra
   calls (gui32 makes 3× the NT calls of gui64).

## 8. Recommended implementation order

Each step ends with programs from this census running, and with their
recorded call sets as the regression list. That needs a gex `--strace`
equivalent for NT calls.

1. **x86-64 console (con64, `cmd /c echo`): 44 NT calls.**
   - bootstrap: PEB/TEB/params, `KUSER_SHARED_DATA`, ntdll and the exe
     through gex-ld, NLS;
   - VM and sections;
   - handles and object directory;
   - files over the VFS;
   - registry, read from a baked `system.reg`/`user.reg`;
   - system info and time.
2. **Threads, sync and exceptions.** Thread creation, events, semaphores
   and mutants, waits, alert-by-thread-id on the futex, `NtDelayExecution`,
   `NtContinue`/`NtRaiseException`, APCs. This is con64's thread test, plus
   the core of Noita's hot path, so make it fast here.
3. **The 32-bit NT ABI** (con32). Required before anything Noita does.
4. **The callback mechanism** (§7 option chosen) and a USER core for
   Noita:
   - a fixed window station, desktop and monitor;
   - classes and atoms;
   - the window tree with a single top-level window as a gex-wayland
     surface;
   - message queues: post, send, peek, get, dispatch, translate, and
     `MsgWaitForMultipleObjectsEx`;
   - hardware input from the compositor, with a fixed keyboard layout;
   - sysparams and metrics;
   - cursors and icons as data, IME as stubs;
   - minimal GDI: DC, bitmaps, DIB sections, `StretchDIBits`, and the
     shared handle table and `DC_ATTR`.

   The opengl32 → gex GL redirect lands alongside, outside the NT layer.
   Target: `present32`, `input32`, then **Noita's main menu**.
5. **The rest of Noita.** Registry notifications, a directory-change stub,
   failing AFD sockets and the unixlib networking DLLs, xinput and dinput
   redirects, and exit (`NtTerminateProcess`). Profile against §4's hot
   list.
6. **winemine and gui64/gui32.**
   - painting: `BeginPaint`/`EndPaint`, update and visible regions;
   - the GDI rasterizer (32 bpp);
   - pens and brushes, `BitBlt` and `PatBlt`, `LineTo`;
   - fonts and text: `ExtTextOutW`, metrics, the `NtUserDrawText`
     callback;
   - non-client painting in `DefWindowProc`;
   - menus and accelerators, capture, timers.
7. **notepad**, optional. Clipboard, carets, scroll bars, dialogs, regions
   and clipping, and font selection.

Steps 1–5 are the Noita path (~21–29k lines). Steps 6–7 add ~9–13k.

---

## Appendix: full per-program call matrices

Column key:
- `con64`/`con32`/`cmd`: console programs;
- `gui64`/`gui32`: the small GUI program;
- `inp32`: input32;
- `gl32`: present32;
- `mine`: winemine;
- `npad`: notepad;
- `N-menu`/`N-game`: Noita to the main menu, and Noita through the new-game
  intro.

Empty cell = not called.

### A1. NT syscalls

| subsystem | call | con64 | con32 | cmd | gui64 | gui32 | inp32 | gl32 | mine | npad | N-menu | N-game |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| atoms | `NtAddAtom` |  |  |  | 4 | 4 | 5 | 4 | 5 | 5 | 5 | 5 |
| files / I/O | `NtCreateFile` | 5 | 5 | 3 | 3 | 3 | 3 | 3 | 3 | 3 | 185 | 554 |
|  | `NtFsControlFile` | 2 | 7 | 2 | 6 | 11 | 13 | 11 | 7 | 8 | 41 | 41 |
|  | `NtNotifyChangeDirectoryFile` |  |  |  |  |  |  |  |  |  |  | 1 |
|  | `NtOpenFile` | 5 | 11 | 4 | 10 | 16 | 20 | 16 | 10 | 13 | 75 | 134 |
|  | `NtQueryAttributesFile` | 4 | 4 | 4 | 5 | 5 | 5 | 5 | 5 | 5 | 10 | 11 |
|  | `NtQueryDirectoryFile` | 2 | 2 |  |  |  |  |  | 2 | 4 | 18 | 49 |
|  | `NtQueryInformationFile` |  |  |  |  |  |  |  | 1 | 2 | 627 | 1599 |
|  | `NtQueryVolumeInformationFile` | 9 | 10 | 7 | 7 | 8 | 8 | 8 | 7 | 7 | 107 | 236 |
|  | `NtReadFile` | 2 | 2 |  |  |  |  |  |  |  | 11604 | 22041 |
|  | `NtSetInformationFile` |  |  |  |  |  |  |  |  |  | 817 | 2217 |
|  | `NtWriteFile` | 2 | 2 | 2 |  |  |  |  |  |  | 5 | 86 |
| objects / handles / namespace | `NtClose` | 34 | 50 | 30 | 44 | 60 | 65 | 55 | 50 | 59 | 1547 | 8220 |
|  | `NtDuplicateObject` |  |  |  |  |  |  |  |  |  | 15 | 6562 |
|  | `NtOpenDirectoryObject` | 1 | 2 | 1 | 1 | 2 | 3 | 2 | 1 | 1 | 3 | 3 |
| registry | `NtCreateKey` | 7 | 8 | 7 | 7 | 8 | 8 | 8 | 8 | 8 | 298 | 163 |
|  | `NtEnumerateKey` |  |  |  |  |  |  |  |  |  | 457 | 451 |
|  | `NtEnumerateValueKey` |  |  |  |  |  |  |  |  |  | 1 | 1 |
|  | `NtNotifyChangeKey` |  |  |  |  |  |  |  |  |  | 17 | 15 |
|  | `NtOpenKey` | 6 | 13 | 7 | 7 | 14 | 14 | 14 | 7 | 7 | 38 | 38 |
|  | `NtOpenKeyEx` | 3 | 3 | 2 | 5 | 5 | 5 | 4 | 6 | 6 | 1211 | 1079 |
|  | `NtQueryKey` |  |  |  |  |  |  |  |  |  | 5 | 1 |
|  | `NtQueryValueKey` | 18 | 28 | 17 | 22 | 32 | 32 | 32 | 22 | 22 | 1108 | 1110 |
|  | `NtRenameKey` |  |  |  |  |  |  |  |  |  | 1 |  |
|  | `NtSetValueKey` | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 15 | 27 | 507 | 259 |
| sections / image mapping | `NtCreateSection` | 4 | 7 | 4 | 9 | 12 | 14 | 12 | 11 | 13 | 45 | 45 |
|  | `NtGetNlsSectionPtr` | 6 | 6 | 6 | 6 | 6 | 6 | 6 | 6 | 6 | 6 | 6 |
|  | `NtMapViewOfSection` | 8 | 12 | 15 | 20 | 24 | 28 | 20 | 23 | 29 | 70 | 70 |
|  | `NtOpenSection` | 6 | 7 | 13 | 17 | 18 | 23 | 14 | 18 | 23 | 59 | 59 |
|  | `NtQuerySection` | 8 | 18 | 15 | 20 | 41 | 49 | 33 | 22 | 27 | 121 | 121 |
|  | `NtUnmapViewOfSection` | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 3 | 4 | 8 | 8 |
| security / tokens | `NtQueryInformationToken` | 2 | 3 | 2 | 2 | 3 | 3 | 3 | 2 | 2 | 3 | 3 |
| sockets / device I/O | `NtDeviceIoControlFile` |  |  | 6 |  |  |  |  |  |  | 33 | 33 |
| sync / waits | `NtCreateEvent` | 1 | 1 | 1 |  |  |  |  |  |  | 37 | 37 |
|  | `NtCreateMutant` |  |  |  |  |  | 1 |  |  |  | 6 | 6 |
|  | `NtCreateSemaphore` |  |  |  |  |  |  |  |  |  | 18 | 18 |
|  | `NtDelayExecution` | 1 | 1 |  |  |  |  |  |  |  | 5650 | 15042 |
|  | `NtReleaseMutant` |  |  |  |  |  |  |  |  |  | 8 | 8 |
|  | `NtReleaseSemaphore` |  |  |  |  |  |  |  |  |  | 1357 | 3827 |
|  | `NtSetEvent` | 1 | 1 |  |  |  |  |  |  |  | 1 | 1 |
|  | `NtTestAlert` |  | 5 |  |  | 1 | 1 | 1 |  |  | 34 | 34 |
|  | `NtWaitForAlertByThreadId` |  |  |  |  |  |  |  |  |  | 52 | 83222 |
|  | `NtWaitForMultipleObjects` | 1 | 1 |  |  |  |  |  |  |  |  | 170 |
|  | `NtWaitForSingleObject` | 1 | 1 |  |  |  |  |  |  |  | 4236 | 11757 |
| system info / time / NLS | `NtInitializeNlsFiles` | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `NtQueryDefaultLocale` | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 2 |
|  | `NtQueryPerformanceCounter` | 1 | 1 |  |  |  |  |  |  |  | 40371 | 181719 |
|  | `NtQuerySystemInformation` | 4 | 6 | 3 | 3 | 5 | 5 | 5 | 3 | 3 | 19 | 273 |
|  | `NtQuerySystemInformationEx` |  | 2 |  |  | 2 | 2 | 2 |  |  | 2 | 2 |
|  | `NtQuerySystemTime` | 4 | 4 | 4 | 3 | 3 | 3 | 3 | 4 | 3 | 146 | 272 |
| threads / processes / context | `NtAlertThreadByThreadId` |  |  |  |  |  |  |  |  |  | 30 | 83211 |
|  | `NtCallbackReturn` |  |  |  | 343 | 343 | 52 | 23 | 173 | 634 | 113 | 185 |
|  | `NtContinue` | 5 |  | 1 | 1 | 343 | 53 | 23 | 1 | 1 | 117 | 189 |
|  | `NtCreateThreadEx` | 4 | 4 |  |  |  |  |  |  |  | 33 | 33 |
|  | `NtGetNextThread` | 2 | 2 |  | 2 | 2 |  |  |  |  | 8 | 8 |
|  | `NtOpenProcess` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `NtQueryInformationProcess` | 3 | 4 | 3 | 3 | 4 | 4 | 4 | 3 | 3 | 15 | 17 |
|  | `NtQueryInformationThread` | 5 | 12 |  | 1 | 347 | 57 | 27 |  |  | 157 | 229 |
|  | `NtRaiseException` |  |  |  |  |  | 1 |  |  |  | 4 | 4 |
|  | `NtResumeThread` | 4 | 4 |  |  |  |  |  |  |  | 33 | 33 |
|  | `NtSetInformationProcess` |  |  |  |  |  |  |  |  |  | 11 | 13 |
|  | `NtSetInformationThread` |  | 10 |  |  | 688 | 109 | 48 |  |  | 317 | 461 |
|  | `NtTerminateProcess` | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 2 |  |  |
|  | `NtTerminateThread` | 4 | 4 |  |  |  |  |  |  |  |  |  |
| virtual memory | `NtAllocateVirtualMemory` | 16 | 13 | 11 | 11 | 13 | 13 | 14 | 13 | 13 | 1435 | 4408 |
|  | `NtFlushInstructionCache` |  |  |  |  |  |  |  |  |  | 1 | 2 |
|  | `NtFreeVirtualMemory` | 6 | 5 | 1 | 1 | 3 | 3 | 3 | 2 | 2 | 94 | 473 |
|  | `NtProtectVirtualMemory` | 55 | 72 | 130 | 172 | 191 | 225 | 131 | 198 | 264 | 868 | 1143 |
|  | `NtQueryVirtualMemory` | 1 | 3 | 2 | 3 | 4 | 4 | 5 | 3 | 3 | 18 | 38 |

### A2. win32u syscalls

| subsystem | call | con64 | con32 | cmd | gui64 | gui32 | inp32 | gl32 | mine | npad | N-menu | N-game |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| GDI: DCs / objects / bitmaps | `NtGdiComputeXformCoefficients` |  |  |  |  |  |  |  |  | 272 |  |  |
|  | `NtGdiCreateBitmap` |  |  |  | 4 | 4 | 5 | 2 | 7 | 9 | 8 | 8 |
|  | `NtGdiCreateCompatibleBitmap` |  |  |  | 4 | 4 | 4 | 2 | 8 | 111 | 7 | 7 |
|  | `NtGdiCreateCompatibleDC` |  |  |  | 8 | 8 | 8 | 4 | 126 | 310 | 14 | 14 |
|  | `NtGdiCreatePatternBrushInternal` |  |  |  |  |  | 1 |  | 1 | 1 | 1 | 1 |
|  | `NtGdiCreatePen` |  |  |  |  |  |  |  | 6 | 40 |  |  |
|  | `NtGdiCreateSolidBrush` |  |  |  | 191 | 191 |  |  |  | 2 |  |  |
|  | `NtGdiDeleteObjectApp` |  |  |  | 200 | 200 | 8 | 4 | 136 | 528 | 18 | 18 |
|  | `NtGdiExtCreatePen` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtGdiExtGetObjectW` |  |  |  | 4 | 4 | 4 | 2 | 5 | 313 | 8 | 8 |
|  | `NtGdiGetAndSetDCDword` |  |  |  | 191 | 191 |  |  |  | 62 |  |  |
|  | `NtGdiGetDCDword` |  |  |  |  |  |  | 152 |  |  | 306 | 306 |
|  | `NtGdiGetDCObject` |  |  |  |  |  |  |  |  | 52 |  |  |
|  | `NtGdiGetDeviceCaps` |  |  |  | 16 | 16 | 15 | 12 | 17 | 88 | 19 | 19 |
|  | `NtGdiOpenDCW` |  |  |  | 2 | 2 | 1 | 1 | 2 | 1 | 2 | 2 |
|  | `NtGdiRectVisible` |  |  |  |  |  |  |  |  | 26 |  |  |
|  | `NtGdiSelectBitmap` |  |  |  | 12 | 12 | 12 | 6 | 271 | 613 | 19 | 19 |
|  | `NtGdiSelectBrush` |  |  |  | 382 | 382 |  |  |  | 78 |  |  |
|  | `NtGdiSelectPen` |  |  |  |  |  |  |  | 12 | 82 |  |  |
|  | `NtGdiTransformPoints` |  |  |  |  |  |  |  |  | 6 |  |  |
| GDI: blits / DIBs / drawing | `NtGdiAlphaBlend` |  |  |  |  |  |  |  |  | 287 |  |  |
|  | `NtGdiBitBlt` |  |  |  |  |  |  |  | 378 | 936 |  |  |
|  | `NtGdiCreateDIBSection` |  |  |  | 4 | 4 | 4 | 2 | 5 | 13 | 7 | 7 |
|  | `NtGdiGetDIBitsInternal` |  |  |  |  |  |  |  |  |  | 3 | 3 |
|  | `NtGdiLineTo` |  |  |  | 191 | 191 |  |  | 6 | 38 |  |  |
|  | `NtGdiMoveTo` |  |  |  | 191 | 191 |  |  | 6 | 38 |  |  |
|  | `NtGdiPatBlt` |  |  |  | 191 | 191 |  |  |  | 38 |  |  |
|  | `NtGdiPolyPolyDraw` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtGdiRectangle` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtGdiStretchBlt` |  |  |  |  |  |  |  |  | 936 | 2 | 2 |
|  | `NtGdiStretchDIBitsInternal` |  |  |  | 203 | 203 | 12 | 6 | 19 | 80 | 15 | 15 |
| GDI: fonts / text | `NtGdiExtTextOutW` |  |  |  | 191 | 191 |  |  | 6 | 95 |  |  |
|  | `NtGdiGetCharWidthW` |  |  |  |  |  |  |  |  | 432 |  |  |
|  | `NtGdiGetFontData` |  |  |  |  |  |  |  |  | 205 |  |  |
|  | `NtGdiGetGlyphIndicesW` |  |  |  |  |  |  |  |  | 432 |  |  |
|  | `NtGdiGetGlyphOutline` |  |  |  |  |  | 2 |  |  |  |  |  |
|  | `NtGdiGetOutlineTextMetricsInternalW` |  |  |  |  |  |  |  |  | 49 |  |  |
|  | `NtGdiGetTextExtentExW` |  |  |  |  |  | 1 |  | 22 | 214 |  |  |
|  | `NtGdiGetTextMetricsW` |  |  |  |  |  | 1 |  | 10 | 133 |  |  |
|  | `NtGdiHfontCreate` |  |  |  |  |  | 1 |  |  | 9 |  |  |
|  | `NtGdiSelectFont` |  |  |  |  |  | 1 |  |  | 231 |  |  |
| GDI: regions / clipping | `NtGdiCombineRgn` |  |  |  |  |  |  |  |  | 23 |  |  |
|  | `NtGdiCreateRectRgn` |  |  |  |  |  |  |  |  | 50 |  |  |
|  | `NtGdiExtSelectClipRgn` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtGdiGetAppClipBox` |  |  |  |  |  |  |  |  | 54 |  |  |
|  | `NtGdiGetRandomRgn` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtGdiIntersectClipRect` |  |  |  |  |  |  |  |  | 83 |  |  |
|  | `NtGdiSetRectRgn` |  |  |  |  |  |  |  |  | 23 |  |  |
| IME / input context | `NtUserAssociateInputContext` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `NtUserBuildHimcList` |  |  |  |  |  |  |  |  |  |  | 1 |
|  | `NtUserQueryInputContext` |  |  |  | 52 | 52 | 8 |  | 25 | 378 | 27 | 78 |
|  | `NtUserUpdateInputContext` |  |  |  | 1 | 1 | 1 |  | 1 | 1 | 1 | 1 |
| clipboard | `NtUserCloseClipboard` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtUserEmptyClipboard` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserGetClipboardData` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserGetClipboardFormatName` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserGetClipboardSequenceNumber` |  |  |  |  |  |  |  |  |  | 1 | 1 |
|  | `NtUserIsClipboardFormatAvailable` |  |  |  |  |  |  |  |  | 4 |  |  |
|  | `NtUserOpenClipboard` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtUserSetClipboardData` |  |  |  |  |  |  |  |  | 1 |  |  |
| cursors / icons | `NtUserDrawIconEx` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserFindExistingCursorIcon` |  |  |  | 16 | 16 | 15 | 12 | 17 | 35 | 17 | 17 |
|  | `NtUserGetIconInfo` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtUserSetCursor` |  |  |  |  |  |  |  |  |  | 6 | 19 |
|  | `NtUserSetCursorIconData` |  |  |  | 4 | 4 | 4 | 2 | 5 | 7 | 6 | 6 |
| display / monitors / sysparams / DPI | `NtUserEnumDisplayDevices` |  |  |  |  |  |  |  |  |  | 1 | 1 |
|  | `NtUserEnumDisplayMonitors` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `NtUserEnumDisplaySettings` |  |  |  |  |  |  |  |  |  | 1 | 1 |
|  | `NtUserGetProcessDefaultLayout` |  |  |  | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `NtUserGetProcessDpiAwarenessContext` |  |  |  | 464 | 464 | 2 | 2 | 63 | 134 | 23 | 127 |
|  | `NtUserGetSystemDpiForProcess` |  |  | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `NtUserSetProcessDefaultLayout` |  |  |  | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `NtUserSetProcessDpiAwarenessContext` |  |  |  |  |  |  |  | 1 | 1 | 1 | 1 |
|  | `NtUserSystemParametersInfo` |  |  |  | 6 | 6 | 6 | 6 | 6 | 9 | 6 | 6 |
| hooks / events | `NtUserCallNextHookEx` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `NtUserNotifyWinEvent` |  |  |  |  |  |  |  |  | 28 |  |  |
|  | `NtUserSetWindowsHookEx` |  |  |  |  |  |  |  |  |  | 1 | 1 |
| keyboard / mouse / raw input | `NtUserClipCursor` |  |  |  |  |  |  |  |  |  | 3 | 3 |
|  | `NtUserGetAsyncKeyState` |  |  |  |  |  |  |  |  |  | 5 | 5 |
|  | `NtUserGetCursorPos` |  |  |  |  |  |  |  |  |  | 1 | 1 |
|  | `NtUserGetDoubleClickTime` |  |  |  |  |  |  |  |  |  | 1 | 1 |
|  | `NtUserGetKeyNameText` |  |  |  |  |  | 3072 |  |  |  |  |  |
|  | `NtUserGetKeyState` |  |  |  |  |  |  |  |  | 100 | 4 | 4 |
|  | `NtUserGetKeyboardLayout` |  |  |  | 168 | 168 | 7 |  | 47 | 382 | 119 | 359 |
|  | `NtUserGetKeyboardState` |  |  |  | 40 | 40 |  |  | 13 | 64 | 2 | 49 |
|  | `NtUserMapVirtualKeyEx` |  |  |  |  |  |  |  |  |  | 97 | 192 |
|  | `NtUserSetCapture` |  |  |  |  |  |  |  | 6 | 1 |  |  |
|  | `NtUserSetFocus` |  |  |  |  |  |  |  |  | 6 |  |  |
|  | `NtUserToUnicodeEx` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `NtUserTrackMouseEvent` |  |  |  |  |  |  |  |  |  | 1 | 1 |
| menus | `NtUserCheckMenuItem` |  |  |  |  |  |  |  | 2 | 2 |  |  |
|  | `NtUserCreateMenu` |  |  |  | 1 | 1 |  |  | 2 | 3 | 1 | 1 |
|  | `NtUserCreatePopupMenu` |  |  |  |  |  |  |  | 2 | 5 |  |  |
|  | `NtUserEnableMenuItem` |  |  |  |  |  |  |  |  | 24 |  |  |
|  | `NtUserThunkedMenuItemInfo` |  |  |  | 9 | 9 |  |  | 24 | 53 | 9 | 9 |
| messages / queues / timers | `NtUserCallMsgFilter` |  |  |  |  |  |  |  |  | 7 |  |  |
|  | `NtUserDispatchMessage` |  |  |  | 190 | 190 |  |  |  | 28 | 91 | 229 |
|  | `NtUserGetMessage` |  |  |  | 462 | 462 |  |  | 58 | 116 | 1 | 1 |
|  | `NtUserGetThreadState` |  |  |  | 4 | 4 | 3 |  | 4 | 60 | 11 | 27 |
|  | `NtUserMessageCall` |  |  |  | 407 | 407 | 38 | 9 | 196 | 1379 | 148 | 302 |
|  | `NtUserMsgWaitForMultipleObjectsEx` |  |  |  |  |  |  |  |  |  | 19 | 59 |
|  | `NtUserPeekMessage` |  |  |  |  |  |  |  |  | 7 | 524 | 851 |
|  | `NtUserPostMessage` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserPostQuitMessage` |  |  |  | 1 | 1 |  |  | 1 | 1 |  |  |
|  | `NtUserRegisterWindowMessage` |  |  |  | 7 | 7 | 7 | 7 | 7 | 8 | 7 | 7 |
|  | `NtUserSetTimer` |  |  |  | 1 | 1 |  |  | 1 |  |  |  |
|  | `NtUserTranslateMessage` |  |  |  | 461 | 461 |  |  | 56 | 116 | 95 | 337 |
| painting / DCE / update regions | `NtUserBeginPaint` |  |  |  | 191 | 191 |  |  | 18 | 58 |  |  |
|  | `NtUserEndPaint` |  |  |  | 191 | 191 |  |  | 18 | 58 |  |  |
|  | `NtUserGetDC` |  |  |  |  |  | 1 | 1 | 94 | 30 | 4 | 4 |
|  | `NtUserGetDCEx` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtUserGetUpdateRect` |  |  |  |  |  |  |  |  |  | 1 | 1 |
|  | `NtUserInvalidateRect` |  |  |  | 190 | 190 |  |  |  | 40 | 2 | 2 |
|  | `NtUserInvalidateRgn` |  |  |  |  |  |  |  |  | 23 |  |  |
|  | `NtUserRedrawWindow` |  |  |  | 1 | 1 |  |  | 19 | 32 |  |  |
|  | `NtUserReleaseDC` |  |  |  |  |  |  |  | 94 | 32 | 3 | 3 |
|  | `NtUserValidateRect` |  |  |  |  |  |  |  |  |  | 1 | 1 |
| window classes / atoms | `NtUserGetAtomName` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserGetClassInfoEx` |  |  |  | 3 | 3 | 3 | 1 | 3 | 18 | 8 | 8 |
|  | `NtUserRegisterClassExWOW` |  |  |  | 3 | 3 | 4 |  | 3 | 17 | 7 | 7 |
| window stations / desktops | `NtUserInitializeClientPfnArrays` |  |  | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
| windows / window manager | `NtUserBuildHwndList` |  |  |  |  |  |  |  |  | 34 |  |  |
|  | `NtUserCallHwnd` |  |  |  | 175 | 175 | 38 | 11 | 152 | 2164 | 98 | 170 |
|  | `NtUserCallHwndParam` |  |  |  | 205 | 205 | 8 | 6 | 13 | 1562 | 30 | 32 |
|  | `NtUserCallNoParam` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserCallOneParam` |  |  |  | 75 | 75 | 90 | 64 | 111 | 164 | 102 | 102 |
|  | `NtUserCallTwoParam` |  |  |  |  |  |  |  | 8 | 98 | 10 | 10 |
|  | `NtUserCreateAcceleratorTable` |  |  |  |  |  |  |  | 1 | 1 |  |  |
|  | `NtUserCreateCaret` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtUserCreateWindowEx` |  |  |  | 3 | 3 | 3 | 1 | 3 | 18 | 8 | 8 |
|  | `NtUserDestroyCaret` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtUserDestroyWindow` |  |  |  | 2 | 2 |  |  | 1 | 10 | 2 | 2 |
|  | `NtUserEnableWindow` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtUserGetAncestor` |  |  |  |  |  |  |  |  | 6 |  |  |
|  | `NtUserGetCaretPos` |  |  |  |  |  |  |  |  | 4 |  |  |
|  | `NtUserGetProp` |  |  |  |  |  |  |  |  | 1325 | 62 | 189 |
|  | `NtUserGetWindowPlacement` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserInternalGetWindowText` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserModifyUserStartupInfoFlags` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserMoveWindow` |  |  |  |  |  |  |  | 2 | 3 |  |  |
|  | `NtUserQueryWindow` |  |  |  | 2 | 2 | 1 |  | 2 | 2 | 2 | 2 |
|  | `NtUserReleaseCapture` |  |  |  |  |  |  |  | 6 | 1 |  |  |
|  | `NtUserSetCaretPos` |  |  |  |  |  |  |  |  | 28 |  |  |
|  | `NtUserSetForegroundWindow` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserSetProp` |  |  |  |  |  |  |  |  | 26 | 1 | 1 |
|  | `NtUserSetScrollInfo` |  |  |  |  |  |  |  |  | 32 |  |  |
|  | `NtUserSetWindowContextHelpId` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserSetWindowFNID` |  |  |  |  |  |  | 4 |  | 47 |  |  |
|  | `NtUserSetWindowLong` |  |  |  |  | 2 | 2 |  |  | 4 | 3 | 3 |
|  | `NtUserSetWindowLongPtr` |  |  |  | 2 |  |  |  | 2 | 87 |  |  |
|  | `NtUserSetWindowPlacement` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserSetWindowPos` |  |  |  | 1 | 1 | 1 |  | 1 | 10 | 5 | 5 |
|  | `NtUserShowCaret` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `NtUserShowWindow` |  |  |  | 1 | 1 |  |  | 1 | 3 | 1 | 1 |
|  | `NtUserTranslateAccelerator` |  |  |  |  |  |  |  | 57 | 113 |  |  |

### A3. NtUserCall* / NtUserMessageCall sub-codes

| subsystem | call | con64 | con32 | cmd | gui64 | gui32 | inp32 | gl32 | mine | npad | N-menu | N-game |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| NtUserCallHwnd | `NtUserCallHwnd:GetDialogInfo` |  |  |  |  |  |  |  |  | 67 |  |  |
|  | `NtUserCallHwnd:GetDpiForWindow` |  |  |  |  |  |  |  |  | 39 |  |  |
|  | `NtUserCallHwnd:GetParent` |  |  |  |  |  |  |  |  | 48 |  |  |
|  | `NtUserCallHwnd:GetWindowDpiAwarenessContext` |  |  |  |  |  |  |  |  | 42 |  |  |
|  | `NtUserCallHwnd:GetWindowInputContext` |  |  |  | 43 | 43 | 2 |  | 16 | 117 | 6 | 53 |
|  | `NtUserCallHwnd:IsWindow` |  |  |  | 2 | 2 | 1 | 4 | 2 | 1290 | 4 | 4 |
|  | `NtUserCallHwnd:IsWindowEnabled` |  |  |  |  |  |  |  |  | 36 |  |  |
|  | `NtUserCallHwnd:IsWindowUnicode` |  |  |  |  |  |  |  |  | 1 | 2 | 2 |
|  | `NtUserCallHwnd:IsWindowVisible` |  |  |  |  |  |  |  |  | 28 |  |  |
|  | `NtUserCallHwnd:NtUserGetFullWindowHandle` |  |  |  |  |  |  |  |  | 52 |  |  |
|  | `NtUserCallHwnd:NtUserIsCurrentProcessWindow` |  |  |  | 130 | 130 | 35 | 7 | 134 | 444 | 86 | 111 |
| NtUserCallHwndParam | `NtUserCallHwndParam:ClientToScreen` |  |  |  |  |  |  |  |  | 1 | 5 | 5 |
|  | `NtUserCallHwndParam:GetClientRect` |  |  |  | 191 | 191 |  |  |  | 116 | 3 | 3 |
|  | `NtUserCallHwndParam:GetWindowLongA` |  |  |  |  |  |  |  |  | 11 | 1 | 1 |
|  | `NtUserCallHwndParam:GetWindowLongPtrA` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserCallHwndParam:GetWindowLongPtrW` |  |  |  | 12 |  |  |  | 13 | 1009 |  |  |
|  | `NtUserCallHwndParam:GetWindowLongW` |  |  |  | 2 | 14 | 8 | 6 |  | 366 | 19 | 21 |
|  | `NtUserCallHwndParam:GetWindowRect` |  |  |  |  |  |  |  |  | 5 | 1 | 1 |
|  | `NtUserCallHwndParam:GetWindowRelative` |  |  |  |  |  |  |  |  | 38 |  |  |
|  | `NtUserCallHwndParam:IsChild` |  |  |  |  |  |  |  |  | 8 |  |  |
|  | `NtUserCallHwndParam:MapWindowPoints` |  |  |  |  |  |  |  |  | 4 |  |  |
|  | `NtUserCallHwndParam:MonitorFromWindow` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserCallHwndParam:ScreenToClient` |  |  |  |  |  |  |  |  |  | 1 | 1 |
|  | `NtUserCallHwndParam:SetDialogInfo` |  |  |  |  |  |  |  |  | 2 |  |  |
| NtUserCallNoParam | `NtUserCallNoParam:GetDialogBaseUnits` |  |  |  |  |  |  |  |  | 1 |  |  |
| NtUserCallOneParam | `NtUserCallOneParam:CreateCursorIcon` |  |  |  | 4 | 4 | 4 | 2 | 5 | 7 | 6 | 6 |
|  | `NtUserCallOneParam:GetSysColor` |  |  |  | 31 | 31 | 48 | 34 | 64 | 54 | 48 | 48 |
|  | `NtUserCallOneParam:GetSystemMetrics` |  |  |  | 40 | 40 | 38 | 28 | 42 | 103 | 48 | 48 |
| NtUserCallTwoParam | `NtUserCallTwoParam:AdjustWindowRect` |  |  |  |  |  |  |  | 2 | 1 | 5 | 5 |
|  | `NtUserCallTwoParam:GetDialogProc` |  |  |  |  |  |  |  |  | 56 |  |  |
|  | `NtUserCallTwoParam:GetMenuInfo` |  |  |  |  |  |  |  | 2 | 5 |  |  |
|  | `NtUserCallTwoParam:GetMonitorInfo` |  |  |  |  |  |  |  | 2 | 2 | 5 | 5 |
|  | `NtUserCallTwoParam:MonitorFromRect` |  |  |  |  |  |  |  | 2 | 1 |  |  |
|  | `NtUserCallTwoParam:NtUserAllocWinProc` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `NtUserCallTwoParam:SetIMECompositionRect` |  |  |  |  |  |  |  |  | 32 |  |  |
| NtUserMessageCall | `NtUserMessageCall:CallWindowProc` |  |  |  |  |  |  |  |  | 396 | 52 | 75 |
|  | `NtUserMessageCall:DefWindowProc` |  |  |  | 130 | 130 | 35 | 7 | 134 | 416 | 85 | 110 |
|  | `NtUserMessageCall:GetDispatchParams` |  |  |  | 271 | 271 |  |  | 57 | 88 | 4 | 108 |
|  | `NtUserMessageCall:PopupMenuWndProc` |  |  |  |  |  |  |  |  | 5 |  |  |
|  | `NtUserMessageCall:SendMessage` |  |  |  | 4 | 4 | 2 |  | 4 | 473 | 6 | 8 |
|  | `NtUserMessageCall:SpyGetMsgName` |  |  |  | 2 | 2 | 1 | 2 | 1 | 1 | 1 | 1 |

### A4. User-mode callbacks (KeUserModeCallback → user32)

| subsystem | call | con64 | con32 | cmd | gui64 | gui32 | inp32 | gl32 | mine | npad | N-menu | N-game |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| KeUserModeCallback | `NtUserCallEnumDisplayMonitor` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `NtUserCallWinProc` |  |  |  | 286 | 286 | 37 | 9 | 133 | 480 | 92 | 117 |
|  | `NtUserCallWindowsHook` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `NtUserDrawScrollBar` |  |  |  |  |  |  |  |  | 26 |  |  |
|  | `NtUserDrawText` |  |  |  |  |  |  |  | 10 | 43 |  |  |
|  | `NtUserImmProcessKey` |  |  |  | 40 | 40 |  |  | 13 | 64 |  | 47 |
|  | `NtUserInitBuiltinClasses` |  |  |  | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `NtUserLoadDriver` |  |  |  | 2 | 2 | 1 | 1 | 2 | 2 | 1 | 1 |
|  | `NtUserLoadImage` |  |  |  | 13 | 13 | 13 | 12 | 13 | 16 | 14 | 14 |
|  | `NtUserLoadSysMenu` |  |  |  | 1 | 1 |  |  | 1 | 2 | 1 | 1 |

### A5. wineserver requests (attributed by thread id; includes win32u font-cache registry traffic, see §2)

| subsystem | call | con64 | con32 | cmd | gui64 | gui32 | inp32 | gl32 | mine | npad | N-menu | N-game |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| USER: classes / atoms / winstations / clipboard / handles | `add_atom` |  |  |  | 4 | 4 | 5 | 4 | 5 | 5 | 5 | 5 |
|  | `add_user_atom` |  |  |  | 7 | 7 | 7 | 7 | 7 | 8 | 7 | 7 |
|  | `alloc_user_handle` |  |  |  | 8 | 8 | 6 | 3 | 13 | 20 | 12 | 12 |
|  | `close_clipboard` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `create_class` |  |  | 2 | 17 | 17 | 18 | 14 | 17 | 31 | 21 | 21 |
|  | `create_desktop` |  |  |  | 1 |  |  |  |  |  |  |  |
|  | `create_winstation` |  |  |  | 1 |  |  |  |  |  |  |  |
|  | `empty_clipboard` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `free_user_handle` |  |  |  | 2 | 2 |  |  | 5 | 10 |  |  |
|  | `get_atom_information` |  |  |  | 2 | 2 | 1 | 1 | 2 | 2 | 1 | 1 |
|  | `get_clipboard_data` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `get_clipboard_formats` |  |  |  |  |  |  |  |  | 4 |  |  |
|  | `get_clipboard_info` |  |  |  | 4 | 4 |  |  | 4 | 22 | 4 | 4 |
|  | `open_clipboard` |  |  |  |  |  |  |  |  | 2 |  |  |
|  | `release_clipboard` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `set_clipboard_data` |  |  |  |  |  |  |  |  | 1 |  |  |
|  | `set_user_object_info` |  |  |  | 6 | 5 | 4 | 3 | 4 | 4 | 6 | 6 |
|  | `set_winstation_monitors` |  |  |  | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
| files / fds / async I/O / sockets | `alloc_file_handle` | 3 |  |  | 4 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `create_file` | 21 | 32 | 27 | 37 | 48 | 54 | 42 | 41 | 49 | 227 | 515 |
|  | `get_async_result` |  |  |  |  |  |  |  |  |  | 14 | 14 |
|  | `get_directory_cache_entry` | 2 | 2 | 4 | 4 | 4 | 4 | 4 | 6 | 8 | 22 | 53 |
|  | `get_handle_fd` | 28 | 39 | 36 | 49 | 60 | 65 | 52 | 52 | 61 | 247 | 498 |
|  | `ioctl` |  |  | 6 |  |  |  |  |  |  | 21 | 21 |
|  | `open_file_object` |  |  | 1 |  |  |  |  |  |  | 6 | 6 |
|  | `read` |  |  |  |  |  |  |  |  |  | 7 | 7 |
|  | `read_directory_changes` |  |  |  |  |  |  |  |  |  |  | 1 |
|  | `recv_socket` |  |  |  |  |  |  |  |  |  | 8 | 8 |
|  | `send_socket` |  |  |  |  |  |  |  |  |  | 3 | 3 |
|  | `set_async_direct_result` |  |  |  |  |  |  |  |  |  | 8 | 8 |
|  | `set_fd_eof_info` |  |  |  |  |  |  |  |  |  | 1 |  |
|  | `set_fd_name_info` |  |  |  |  |  |  |  |  |  |  | 34 |
|  | `set_named_pipe_info` |  |  |  |  |  |  |  |  |  | 1 | 1 |
|  | `write` |  |  |  |  |  |  |  |  |  | 3 | 3 |
| message queues / input | `accept_hardware_message` |  |  |  | 60 | 60 |  |  | 42 | 72 | 4 | 62 |
|  | `finish_hook_chain` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `get_hook_info` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `get_key_state` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `get_message` |  |  |  | 704 | 683 |  |  | 86 | 209 | 103 | 347 |
|  | `get_message_reply` |  |  |  | 2 | 2 | 1 | 1 | 2 | 4 | 1 | 1 |
|  | `get_msg_queue` |  |  |  | 1 | 1 | 1 | 1 | 1 | 1 | 3 | 3 |
|  | `get_msg_queue_handle` |  |  |  | 1 | 1 | 1 | 1 | 1 | 1 | 3 | 3 |
|  | `get_thread_input` |  |  |  | 7 | 7 | 7 | 6 | 8 | 8 | 31 | 31 |
|  | `kill_win_timer` |  |  |  |  |  |  |  |  | 29 | 1 | 1 |
|  | `post_quit_message` |  |  |  | 1 | 1 |  |  | 1 | 1 |  |  |
|  | `send_hardware_message` |  |  |  | 60 | 60 |  |  | 36 | 74 | 5 | 68 |
|  | `send_message` |  |  |  | 30 | 30 | 7 | 3 | 22 | 54 | 15 | 61 |
|  | `set_active_window` |  |  |  | 2 | 2 | 1 |  | 2 | 4 | 1 | 1 |
|  | `set_capture_window` |  |  |  |  |  |  |  | 12 | 4 |  |  |
|  | `set_caret_info` |  |  |  | 384 | 384 | 1 |  | 38 | 166 | 1 | 1 |
|  | `set_caret_window` |  |  |  |  |  |  |  |  | 4 |  |  |
|  | `set_cursor` |  |  |  | 20 | 20 |  |  | 21 | 4 | 13 | 26 |
|  | `set_focus_window` |  |  |  | 2 | 2 | 1 |  | 2 | 9 | 1 | 1 |
|  | `set_foreground_window` |  |  |  | 2 | 2 | 1 |  | 2 | 5 | 1 | 1 |
|  | `set_hook` |  |  |  |  |  |  |  |  |  | 1 | 1 |
|  | `set_key_state` |  |  |  |  |  |  |  | 1 |  |  |  |
|  | `set_queue_fd` |  |  |  | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `set_queue_mask` |  |  |  | 316 | 291 | 3 | 2 | 62 | 155 | 84 | 229 |
|  | `set_win_timer` |  |  |  | 1 | 1 |  |  | 1 | 52 | 1 | 1 |
|  | `start_hook_chain` |  |  |  |  |  |  |  |  |  | 2 | 2 |
| objects / handles / namespace | `close_handle` | 151 | 164 | 4799 | 4869 | 4881 | 4892 | 4862 | 4861 | 4882 | 6439 | 13114 |
|  | `dup_handle` |  |  |  |  |  |  |  |  |  | 15 | 6562 |
|  | `open_directory` | 1 | 2 | 1 | 2 | 2 | 3 | 2 | 1 | 1 | 3 | 3 |
| registry | `create_key` | 14 | 11 | 2984 | 2994 | 2991 | 2991 | 2991 | 2991 | 2991 | 3283 | 3148 |
|  | `enum_key` | 41 | 41 | 3378 | 3388 | 3388 | 3388 | 3388 | 3388 | 3388 | 3850 | 3840 |
|  | `enum_key_value` | 58 |  | 7673 | 7731 | 7673 | 7673 | 7673 | 7673 | 7673 | 7674 | 7674 |
|  | `get_key_value` | 211 | 215 | 4798 | 4954 | 4958 | 4965 | 4949 | 4943 | 4955 | 6128 | 6130 |
|  | `open_key` | 104 | 103 | 1778 | 1822 | 1818 | 1816 | 1822 | 1812 | 1815 | 3054 | 2922 |
|  | `rename_key` |  |  |  |  |  |  |  |  |  | 1 |  |
|  | `set_key_value` | 2 | 2 | 2969 | 2969 | 2969 | 2969 | 2969 | 2982 | 2994 | 3474 | 3226 |
|  | `set_registry_notification` |  |  |  |  |  |  |  |  |  | 17 | 15 |
| sections / mappings | `create_mapping` | 16 | 24 | 24 | 33 | 41 | 47 | 36 | 37 | 44 | 109 | 110 |
|  | `get_image_map_address` |  | 10 |  |  | 21 | 25 | 17 | 1 | 3 | 60 | 60 |
|  | `get_mapping_info` | 34 | 53 | 58 | 73 | 103 | 118 | 85 | 80 | 96 | 263 | 264 |
|  | `map_image_view` | 10 | 15 | 17 | 21 | 26 | 30 | 22 | 23 | 28 | 66 | 66 |
|  | `map_view` | 8 | 8 | 10 | 12 | 12 | 11 | 11 | 13 | 14 | 17 | 18 |
|  | `open_mapping` | 13 | 14 | 22 | 27 | 28 | 32 | 23 | 28 | 33 | 68 | 68 |
|  | `unmap_view` | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 3 | 4 | 8 | 8 |
| security / tokens | `get_token_sid` | 9 | 6 | 6 | 11 | 8 | 8 | 9 | 7 | 7 | 9 | 9 |
| sync objects | `create_event` | 2 | 1 | 1 | 1 |  |  |  |  |  | 39 | 39 |
|  | `create_keyed_event` | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `create_mutex` |  |  | 1 | 2 | 2 | 3 | 2 | 2 | 2 | 8 | 8 |
|  | `create_semaphore` |  |  |  |  |  |  |  |  |  | 18 | 18 |
|  | `get_inproc_alert_fd` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `get_inproc_sync_fd` | 6 | 5 | 1 | 8 | 7 | 6 | 3 | 4 | 4 | 41 | 43 |
|  | `get_process_idle_event` |  |  |  | 1 | 1 | 1 |  |  |  | 1 | 1 |
|  | `select` | 9 | 10 | 2 | 1 | 2 | 2 | 2 | 2 | 2 | 98 | 99 |
| threads / processes | `get_new_process_info` |  |  |  | 1 | 1 | 1 |  |  |  | 1 | 1 |
|  | `get_next_thread` | 2 | 2 |  | 2 | 2 |  |  |  |  | 8 | 8 |
|  | `get_process_debug_info` | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `get_process_info` | 1 | 3 | 1 | 1 | 3 | 3 | 3 | 1 | 1 | 3 | 3 |
|  | `get_process_winstation` |  |  | 1 | 5 | 5 | 4 | 4 | 5 | 5 | 6 | 6 |
|  | `get_startup_info` |  | 1 | 1 |  | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `get_thread_context` |  |  |  |  |  | 1 |  |  |  | 4 | 4 |
|  | `get_thread_desktop` |  |  | 1 | 3 | 3 | 3 | 2 | 2 | 2 | 5 | 5 |
|  | `get_thread_info` | 5 | 5 |  | 1 | 1 | 1 | 1 |  |  | 10 | 10 |
|  | `get_thread_times` |  |  |  |  |  |  |  |  |  | 6 | 6 |
|  | `init_first_thread` | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `init_process_done` | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
|  | `init_thread` | 4 | 4 |  |  |  |  |  |  |  | 33 | 33 |
|  | `new_process` |  |  |  | 1 | 1 | 1 |  |  |  | 1 | 1 |
|  | `new_thread` | 4 | 4 |  | 1 | 1 | 1 |  |  |  | 36 | 36 |
|  | `open_process` |  |  |  |  |  |  |  |  |  | 2 | 2 |
|  | `resume_thread` | 4 | 4 |  | 1 | 1 | 1 |  |  |  | 34 | 34 |
|  | `set_process_winstation` |  |  |  | 1 |  |  |  |  |  |  |  |
|  | `set_thread_desktop` |  |  |  | 1 |  |  |  |  |  |  |  |
|  | `set_thread_info` |  |  |  |  |  |  |  |  |  | 15 | 15 |
|  | `terminate_process` | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 2 | 2 |  |  |
|  | `terminate_thread` | 4 | 4 |  |  |  |  |  |  |  |  |  |
| windows / window tree | `create_window` |  |  |  | 4 | 4 | 4 | 2 | 4 | 20 | 10 | 10 |
|  | `destroy_window` |  |  |  | 4 | 4 |  |  | 4 | 20 | 3 | 3 |
|  | `get_desktop_window` |  |  |  | 2 | 2 | 2 | 1 | 1 | 1 | 4 | 4 |
|  | `get_update_region` |  |  |  | 387 | 387 | 3 |  | 61 | 173 | 10 | 10 |
|  | `get_visible_region` |  |  |  | 196 | 196 | 5 | 1 | 38 | 134 | 10 | 10 |
|  | `get_window_children_from_point` |  |  |  | 28 | 28 | 7 | 5 | 31 | 67 | 121 | 270 |
|  | `get_window_info` |  |  |  | 6 | 6 |  |  | 6 | 6 |  |  |
|  | `get_window_layered_info` |  |  |  | 4 | 4 | 4 | 2 | 5 | 8 | 6 | 6 |
|  | `get_window_list` |  |  |  | 17 | 17 | 10 | 3 | 17 | 66 | 24 | 24 |
|  | `get_window_property` |  |  |  | 5 | 5 | 4 | 3 | 5 | 1330 | 72 | 199 |
|  | `get_window_rectangles` |  |  |  | 1 | 1 | 1 | 1 | 1 | 1 | 3 | 3 |
|  | `get_window_region` |  |  |  | 16 | 16 | 15 | 7 | 20 | 116 | 35 | 35 |
|  | `get_window_tree` |  |  |  | 79 | 79 | 31 | 16 | 79 | 179 | 1113 | 1447 |
|  | `init_window_info` |  |  |  | 4 | 4 | 4 | 2 | 4 | 20 | 10 | 10 |
|  | `redraw_window` |  |  |  | 190 | 190 |  |  | 18 | 67 | 4 | 4 |
|  | `remove_window_property` |  |  |  | 3 | 3 |  |  | 3 | 19 | 3 | 3 |
|  | `set_parent` |  |  |  | 4 | 4 |  |  | 4 | 20 | 3 | 3 |
|  | `set_window_info` |  |  |  | 2 | 2 | 2 |  | 3 | 130 | 3 | 3 |
|  | `set_window_pos` |  |  |  | 11 | 11 | 10 | 5 | 13 | 101 | 29 | 29 |
|  | `set_window_property` |  |  |  | 4 | 4 | 4 | 2 | 4 | 33 | 8 | 8 |
|  | `set_window_text` |  |  |  | 4 | 4 | 4 | 2 | 4 | 23 | 11 | 11 |
|  | `update_window_zorder` |  |  |  | 1 | 1 |  |  | 1 | 4 | 1 | 1 |
