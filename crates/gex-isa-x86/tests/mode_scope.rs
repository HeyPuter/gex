//! A thread's mode (`set_thread_mode`, `ModeScope`) is its own: two threads
//! decoding code of different classes at once each see theirs, and a scope
//! restores what the thread had.

use gex_isa_x86::{decode, mode, set_mode, set_thread_mode, Mode, ModeScope};

/// `48 89 c3`: `mov rbx, rax` in long mode (3 bytes), `dec eax` in
/// compatibility mode (1 byte).
const BYTES: [u8; 3] = [0x48, 0x89, 0xc3];

fn len_now() -> u8 {
    decode(&BYTES, 0x1000).len
}

#[test]
fn two_threads_decode_in_their_own_modes_at_once() {
    set_mode(Mode::Long);
    let go = std::sync::Barrier::new(2);
    std::thread::scope(|s| {
        for (m, want) in [(Mode::Compat, 1u8), (Mode::Long, 3u8)] {
            let go = &go;
            s.spawn(move || {
                set_thread_mode(m);
                go.wait();
                for _ in 0..20_000 {
                    assert_eq!(len_now(), want, "{m:?}");
                }
            });
        }
    });
}

#[test]
fn a_scope_nests_and_restores() {
    set_mode(Mode::Long);
    assert_eq!(mode(), Mode::Long);
    {
        let _a = ModeScope::enter(Mode::Compat);
        assert_eq!(len_now(), 1);
        {
            let _b = ModeScope::enter(Mode::Long);
            assert_eq!(len_now(), 3);
        }
        assert_eq!(len_now(), 1);
    }
    assert_eq!(len_now(), 3);
    set_thread_mode(Mode::Compat);
    assert_eq!(mode(), Mode::Compat);
    // The process default, set on this thread, clears the thread's own.
    set_mode(Mode::Long);
    assert_eq!(mode(), Mode::Long);
}
