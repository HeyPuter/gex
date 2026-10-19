//! OSS audio devices, `/dev/dsp` and `/dev/mixer`: a null output for now.
//!
//! Native Wine's audio (W3.5, docs/wine-native.md) is mmdevapi over
//! wineoss.drv, the OSSv4 driver, and this is the device it talks to: one
//! playback engine that takes 16-, 24- and 32-bit and float PCM at any rate,
//! and plays it into nothing at the rate it was set to. Its buffer drains
//! with the wall clock, so a program that paces itself by the device (as
//! mmdevapi's clients do, by the padding it reports) runs at the right
//! speed. No capture. A device that plays (the browser's WebAudio, the
//! host's) replaces the drain, not the interface: while a player is taking
//! this stream's frames ([`set_audio_pending`]), what is queued is what it
//! has not played yet, so writers pace to the player's clock (the audio
//! hardware's) and the two clocks never drift apart.
//!
//! The requests are Linux's OSSv4 numbers (`_IOR`/`_IOWR` of Linux's
//! `sys/ioctl.h`), and the structs `oss_sysinfo` (1248 bytes),
//! `oss_audioinfo` (1180) and `audio_buf_info` (16) are laid out as OSSv4's
//! `sys/soundcard.h` has them: ints and char arrays only, the same on every
//! ABI gex runs.

use super::{FileHandle, FileType, Metadata};
use crate::errno::{Errno, SysResult};
use std::sync::Mutex;

/// Which node a handle was opened as.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OssNode {
    Dsp,
    Mixer,
}

// Requests (Linux encoding).
const SNDCTL_SYSINFO: u32 = 0x84e0_5801;
const SNDCTL_AUDIOINFO: u32 = 0xc49c_5807;
const SNDCTL_ENGINEINFO: u32 = 0xc49c_580c;
const SNDCTL_AUDIOINFO_EX: u32 = 0xc49c_580d;
const SNDCTL_DSP_RESET: u32 = 0x0000_5000;
const SNDCTL_DSP_SYNC: u32 = 0x0000_5001;
const SNDCTL_DSP_SPEED: u32 = 0xc004_5002;
const SNDCTL_DSP_SETFMT: u32 = 0xc004_5005;
const SNDCTL_DSP_CHANNELS: u32 = 0xc004_5006;
const SNDCTL_DSP_SETFRAGMENT: u32 = 0xc004_500a;
const SNDCTL_DSP_GETFMTS: u32 = 0x8004_500b;
const SNDCTL_DSP_GETOSPACE: u32 = 0x8010_500c;
const SNDCTL_DSP_GETISPACE: u32 = 0x8010_500d;
const SNDCTL_DSP_GETCAPS: u32 = 0x8004_500f;
const SNDCTL_DSP_GETODELAY: u32 = 0x8004_5017;
/// `SOUND_MIXER_READ_*` and `_WRITE_*`: `_IOR('M', n, int)`, `_IOWR('M', n, int)`.
const MIXER_READ: u32 = 0x8004_4d00;
const MIXER_WRITE: u32 = 0xc004_4d00;

// Formats.
const AFMT_U8: u32 = 0x0000_0008;
const AFMT_S16_LE: u32 = 0x0000_0010;
const AFMT_S32_LE: u32 = 0x0000_1000;
const AFMT_S24_LE: u32 = 0x0001_0000;
const AFMT_S24_PACKED: u32 = 0x0004_0000;
const AFMT_F32_LE: u32 = 0x1000_0000;
const FORMATS: u32 = AFMT_U8 | AFMT_S16_LE | AFMT_S24_LE | AFMT_S24_PACKED | AFMT_S32_LE | AFMT_F32_LE;
const PCM_CAP_OUTPUT: u32 = 0x0002_0000;
const PCM_CAP_TRIGGER: u32 = 0x0000_1000;

/// The playback buffer: 16 fragments of 2 KiB (about 190 ms of 44.1 kHz
/// stereo 16-bit).
const FRAGSIZE: u64 = 2048;
const FRAGMENTS: u64 = 16;
const BUFSIZE: u64 = FRAGSIZE * FRAGMENTS;

#[derive(Debug)]
struct Stream {
    format: u32,
    rate: u32,
    channels: u32,
    /// Bytes written and not yet played.
    queued: u64,
    /// When `queued` was last brought up to date, in nanoseconds of the
    /// host's clock (`tmpfs::now_ns`: the browser has no `std::time`).
    at: u64,
    /// The open's number ([`OssHandle`]'s), for the player.
    id: u32,
    /// Whether the player paced the last drain.
    played_by_player: bool,
}

impl Stream {
    fn frame_bytes(&self) -> u64 {
        self.channels as u64 * sample_size(self.format) as u64
    }

    fn bytes_per_second(&self) -> u64 {
        self.rate as u64 * self.frame_bytes()
    }

    /// Play what the player has played since the last look, or (no player
    /// taking this stream's frames) what the time since then has.
    fn drain(&mut self) {
        let now = super::tmpfs::now_ns();
        if let Some(pending) = AUDIO_PENDING.get().and_then(|f| f(self.id, (now / 1_000_000) as u32)) {
            // Only ever less: what was written and is not played yet.
            self.queued = self.queued.min(pending as u64 * self.frame_bytes());
            self.at = now;
            self.played_by_player = true;
            return;
        }
        self.played_by_player = false;
        let ns = now.saturating_sub(self.at);
        let played = (ns as u128 * self.bytes_per_second() as u128 / 1_000_000_000) as u64;
        if played > 0 || self.queued == 0 {
            self.queued = self.queued.saturating_sub(played);
            self.at = now;
        }
    }
}

/// Bytes in one sample of `format`.
fn sample_size(format: u32) -> usize {
    match format {
        AFMT_U8 => 1,
        AFMT_S16_LE => 2,
        AFMT_S24_PACKED => 3,
        _ => 4,
    }
}

fn sample(format: u32, b: &[u8]) -> f32 {
    match format {
        AFMT_U8 => (b[0] as f32 - 128.0) / 128.0,
        AFMT_S16_LE => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
        AFMT_S24_PACKED => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
        AFMT_S24_LE => ((i32::from_le_bytes([b[0], b[1], b[2], b[3]]) << 8) >> 8) as f32 / 8_388_608.0,
        AFMT_S32_LE => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        AFMT_F32_LE => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        _ => 0.0,
    }
}

/// Convert whole frames of `pcm` (`channels` interleaved samples of
/// `format`) to stereo `f32` into `out`, what the page's player plays
/// (shims/kernel `audio.rs`); how many frames. Mono goes to both sides,
/// more than two channels keep the first two.
pub fn to_stereo_f32(pcm: &[u8], format: u32, channels: u32, out: &mut Vec<[f32; 2]>) -> usize {
    let sb = sample_size(format);
    let ch = channels.max(1) as usize;
    let fb = sb * ch;
    let n = pcm.len() / fb;
    for f in 0..n {
        let at = f * fb;
        let l = sample(format, &pcm[at..at + sb]);
        let r = if ch > 1 { sample(format, &pcm[at + sb..at + 2 * sb]) } else { l };
        out.push([l, r]);
    }
    n
}

/// Where what `/dev/dsp` accepts is also played, when the host has a
/// player (the page's AudioWorklet, through the global kernel's ring:
/// shims/kernel `audio.rs`): `(stream, bytes, format, rate, channels, ms)`,
/// `ms` the device's clock.
/// Never blocks; the device keeps its own clock either way.
pub type AudioOut = fn(u32, &[u8], u32, u32, u32, u32);

static AUDIO_OUT: std::sync::OnceLock<AudioOut> = std::sync::OnceLock::new();

/// Set the host's player (once; later calls are ignored).
pub fn set_audio_out(f: AudioOut) {
    let _ = AUDIO_OUT.set(f);
}

/// The player's clock: `(stream, ms)` -> the frames of `stream` written to
/// the player and not played yet, or `None` when the player is not taking
/// that stream's frames now (another stream has it, or it has not played
/// for a while: no gesture yet, a background tab). `ms` the device's clock.
pub type AudioPending = fn(u32, u32) -> Option<u32>;

static AUDIO_PENDING: std::sync::OnceLock<AudioPending> = std::sync::OnceLock::new();

/// Set the player's clock (once; later calls are ignored).
pub fn set_audio_pending(f: AudioPending) {
    let _ = AUDIO_PENDING.set(f);
}

/// How often a writer waiting on a full buffer the player drains looks
/// again: one render quantum of the page's AudioWorklet (128 frames).
const PLAYER_QUANTUM_FRAMES: u64 = 128;

/// Each open `/dev/dsp`'s number, for the player to tell streams apart.
static NEXT_STREAM: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// An open `/dev/dsp` or `/dev/mixer`.
#[derive(Debug)]
pub struct OssHandle {
    node: OssNode,
    stream: Mutex<Stream>,
    id: u32,
}

impl OssHandle {
    pub fn new(node: OssNode) -> OssHandle {
        OssHandle {
            node,
            id: NEXT_STREAM.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            stream: Mutex::new(Stream { format: AFMT_U8, rate: 8000, channels: 1, queued: 0, at: super::tmpfs::now_ns(), id: 0, played_by_player: false }),
        }
        .numbered()
    }

    fn numbered(self) -> OssHandle {
        self.stream.lock().unwrap_or_else(|e| e.into_inner()).id = self.id;
        self
    }
}

fn put_str(buf: &mut [u8], at: usize, s: &str) {
    buf[at..at + s.len()].copy_from_slice(s.as_bytes());
}

fn put_i32(buf: &mut [u8], at: usize, v: i32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// `oss_sysinfo`: one card, one mixer, one playback engine.
fn sysinfo() -> Vec<u8> {
    let mut b = vec![0u8; 1248];
    put_str(&mut b, 0, "gex");
    put_str(&mut b, 32, "4.2");
    put_i32(&mut b, 64, 0x040200); // versionnum
    put_i32(&mut b, 196, 1); // numaudios
    put_i32(&mut b, 244, 1); // nummixers
    put_i32(&mut b, 280, 1); // numcards
    put_i32(&mut b, 284, 1); // numaudioengines
    put_str(&mut b, 288, "BSD");
    b
}

/// `oss_audioinfo` of the one engine.
fn audioinfo() -> Vec<u8> {
    let mut b = vec![0u8; 1180];
    put_i32(&mut b, 0, 0); // dev
    put_str(&mut b, 4, "gex null output");
    put_i32(&mut b, 76, (PCM_CAP_OUTPUT | PCM_CAP_TRIGGER) as i32); // caps
    put_i32(&mut b, 84, FORMATS as i32); // oformats
    put_i32(&mut b, 172, 1); // enabled
    put_i32(&mut b, 180, 8000); // min_rate
    put_i32(&mut b, 184, 192000); // max_rate
    put_i32(&mut b, 188, 1); // min_channels
    put_i32(&mut b, 192, 2); // max_channels: the mix format is stereo
    put_i32(&mut b, 400, -1); // latency: unknown
    put_str(&mut b, 404, "/dev/dsp"); // devnode
    b
}

impl FileHandle for OssHandle {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        // No capture: a reader waits for nothing.
        Err(Errno::EAGAIN)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        if self.node != OssNode::Dsp {
            return Err(Errno::EINVAL);
        }
        let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        s.drain();
        let room = BUFSIZE - s.queued.min(BUFSIZE);
        if room == 0 {
            return Err(Errno::EAGAIN);
        }
        // Whole frames when not all of it fits: what is played next starts
        // on a frame (the player reads frames from each write).
        let mut n = (buf.len() as u64).min(room);
        if n < buf.len() as u64 {
            n -= n % s.frame_bytes();
            if n == 0 {
                return Err(Errno::EAGAIN);
            }
        }
        s.queued += n;
        if let Some(out) = AUDIO_OUT.get() {
            out(self.id, &buf[..n as usize], s.format, s.rate, s.channels, (s.at / 1_000_000) as u32);
        }
        Ok(n)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::CharDevice,
            size: 0,
            mode: 0o666,
            ino: 1,
            mtime: 0,
            nlink: 1,
            // 14:3 and 14:0, Linux's sound major.
            rdev: if self.node == OssNode::Dsp { 0xe03 } else { 0xe00 },
            times: Default::default(),
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll_writable(&self) -> bool {
        let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        s.drain();
        s.queued < BUFSIZE
    }

    fn poll_readable(&self) -> bool {
        false
    }

    /// A full buffer drains by the clock, which no notify reports: a writer
    /// waiting for room is ready when one byte has played.
    fn ready_in_ns(&self) -> Option<u64> {
        if self.node != OssNode::Dsp {
            return None;
        }
        let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        s.drain();
        if s.queued < BUFSIZE {
            // Room now: a writer is ready, and a reader never is.
            return None;
        }
        if s.played_by_player {
            // The player takes a quantum at a time.
            return Some((PLAYER_QUANTUM_FRAMES * 1_000_000_000 / s.rate.max(1) as u64).max(1));
        }
        let bps = s.bytes_per_second().max(1);
        Some((1_000_000_000 / bps).max(1))
    }

    fn ioctl(&self, m: &mut crate::GuestMem<'_>, request: u64, arg: u64) -> Option<SysResult> {
        let request = request as u32;
        // An int argument read and written back.
        let mut int = |m: &mut crate::GuestMem<'_>, f: &mut dyn FnMut(i32) -> i32| -> SysResult {
            let v = m.read_u32(arg)? as i32;
            m.write_u32(arg, f(v) as u32)?;
            Ok(0)
        };
        let r = match request {
            SNDCTL_SYSINFO => m.write(arg, &sysinfo()).map(|_| 0),
            SNDCTL_AUDIOINFO | SNDCTL_AUDIOINFO_EX | SNDCTL_ENGINEINFO => {
                // Device -1 is "this one" (on /dev/dsp) or the default.
                match m.read_u32(arg) {
                    Ok(d) if d == 0 || d == u32::MAX => m.write(arg, &audioinfo()).map(|_| 0),
                    Ok(_) => Err(Errno::ENXIO),
                    Err(e) => Err(e),
                }
            }
            _ if self.node == OssNode::Mixer => match request & 0xffff_ff00 {
                // Volumes: full, both channels; what is written is accepted.
                MIXER_READ => m.write_u32(arg, 0x6464).map(|_| 0),
                MIXER_WRITE => Ok(0),
                _ => return None,
            },
            SNDCTL_DSP_SETFMT => {
                let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
                int(m, &mut |v| {
                    let v = v as u32;
                    if v != 0 {
                        s.drain();
                        s.format = if v.count_ones() == 1 && FORMATS & v != 0 { v } else { AFMT_S16_LE };
                    }
                    s.format as i32
                })
            }
            SNDCTL_DSP_SPEED => {
                let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
                int(m, &mut |v| {
                    s.drain();
                    s.rate = (v.max(8000) as u32).min(192000);
                    s.rate as i32
                })
            }
            SNDCTL_DSP_CHANNELS => {
                let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
                int(m, &mut |v| {
                    s.drain();
                    s.channels = v.clamp(1, 8) as u32;
                    s.channels as i32
                })
            }
            SNDCTL_DSP_GETFMTS => m.write_u32(arg, FORMATS).map(|_| 0),
            SNDCTL_DSP_GETCAPS => m.write_u32(arg, PCM_CAP_OUTPUT | PCM_CAP_TRIGGER).map(|_| 0),
            // The fragment layout is fixed; the request is answered as made.
            SNDCTL_DSP_SETFRAGMENT => Ok(0),
            SNDCTL_DSP_GETOSPACE => {
                let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
                s.drain();
                let free = BUFSIZE - s.queued.min(BUFSIZE);
                let mut b = [0u8; 16];
                put_i32(&mut b, 0, (free / FRAGSIZE) as i32);
                put_i32(&mut b, 4, FRAGMENTS as i32);
                put_i32(&mut b, 8, FRAGSIZE as i32);
                put_i32(&mut b, 12, free as i32);
                m.write(arg, &b).map(|_| 0)
            }
            SNDCTL_DSP_GETISPACE => {
                let mut b = [0u8; 16];
                put_i32(&mut b, 4, FRAGMENTS as i32);
                put_i32(&mut b, 8, FRAGSIZE as i32);
                m.write(arg, &b).map(|_| 0)
            }
            SNDCTL_DSP_GETODELAY => {
                let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
                s.drain();
                m.write_u32(arg, s.queued as u32).map(|_| 0)
            }
            SNDCTL_DSP_RESET | SNDCTL_DSP_SYNC => {
                let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
                s.queued = 0;
                s.at = super::tmpfs::now_ns();
                Ok(0)
            }
            _ => return None,
        };
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_convert_to_stereo_floats() {
        let mut out = Vec::new();
        // S16 mono: one frame, both sides.
        assert_eq!(to_stereo_f32(&16384i16.to_le_bytes(), AFMT_S16_LE, 1, &mut out), 1);
        assert_eq!(out[0], [0.5, 0.5]);
        out.clear();
        // F32 stereo, and a trailing partial frame left alone.
        let mut b = Vec::new();
        b.extend_from_slice(&0.25f32.to_le_bytes());
        b.extend_from_slice(&(-1.0f32).to_le_bytes());
        b.push(7);
        assert_eq!(to_stereo_f32(&b, AFMT_F32_LE, 2, &mut out), 1);
        assert_eq!(out[0], [0.25, -1.0]);
        out.clear();
        // U8 silence, 3 channels: the first two kept.
        assert_eq!(to_stereo_f32(&[128, 128, 0], AFMT_U8, 3, &mut out), 1);
        assert_eq!(out[0], [0.0, 0.0]);
        out.clear();
        // S24 packed and S32.
        to_stereo_f32(&[0, 0, 0x40], AFMT_S24_PACKED, 1, &mut out);
        assert_eq!(out[0][0], 0.5);
        out.clear();
        to_stereo_f32(&(i32::MIN).to_le_bytes(), AFMT_S32_LE, 1, &mut out);
        assert_eq!(out[0][0], -1.0);
    }
    use std::time::Instant;

    #[test]
    fn the_buffer_drains_with_the_clock() {
        let h = OssHandle::new(OssNode::Dsp);
        {
            let mut s = h.stream.lock().unwrap();
            s.format = AFMT_S16_LE;
            s.rate = 48000;
            s.channels = 2;
        }
        // 192000 bytes a second: the 32 KiB buffer fills, then drains in
        // about 171 ms.
        let start = Instant::now();
        assert_eq!(h.write_at(0, &vec![0u8; 100_000]).unwrap(), BUFSIZE);
        assert_eq!(h.write_at(0, &[0u8; 4]), Err(Errno::EAGAIN));
        std::thread::sleep(std::time::Duration::from_millis(60));
        let n = h.write_at(0, &vec![0u8; 100_000]).unwrap();
        // Against the clock as it actually ran (a loaded machine sleeps
        // longer than asked): at least the 60 ms asked for has played,
        // no more than the time that passed, and while that was short of
        // the whole buffer's 171 ms, less than all of it.
        let played = start.elapsed().as_micros() as u64 * 192_000 / 1_000_000;
        assert!(n >= 11_520 && n <= played.min(BUFSIZE) + 4, "{n} (played {played})");
        if played + 4 < BUFSIZE {
            assert!(n < BUFSIZE, "{n} (played {played})");
        }
    }

    #[test]
    fn a_write_that_does_not_fit_takes_whole_frames() {
        let h = OssHandle::new(OssNode::Dsp);
        {
            let mut s = h.stream.lock().unwrap();
            s.format = AFMT_S16_LE;
            s.channels = 2;
        }
        // All of a write that fits, even part of a frame.
        assert_eq!(h.write_at(0, &vec![0u8; BUFSIZE as usize - 6]).unwrap(), BUFSIZE - 6);
        // The clock stopped: nothing plays from here on.
        h.stream.lock().unwrap().at = u64::MAX;
        // 6 bytes of room: one 4-byte frame of the 8 offered, then none.
        assert_eq!(h.write_at(0, &[0u8; 8]).unwrap(), 4);
        assert_eq!(h.write_at(0, &[0u8; 8]), Err(Errno::EAGAIN));
    }

    /// The test player: what it has not played, by stream (absent: not
    /// taking that stream). Other tests' streams stay on the clock.
    static PLAYER: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

    fn test_player(stream: u32, _ms: u32) -> Option<u32> {
        PLAYER.lock().unwrap().iter().find(|e| e.0 == stream).map(|e| e.1)
    }

    #[test]
    fn a_player_taking_the_stream_is_its_clock() {
        set_audio_pending(test_player);
        let h = OssHandle::new(OssNode::Dsp);
        {
            let mut s = h.stream.lock().unwrap();
            s.format = AFMT_S16_LE;
            s.rate = 48000;
            s.channels = 2;
        }
        // Full, and the player has played none of it: however long that
        // takes, no room, and a waiter looks again in a quantum (128
        // frames at 48 kHz, 2.67 ms), not a byte's time.
        PLAYER.lock().unwrap().push((h.id, (BUFSIZE / 4) as u32));
        assert_eq!(h.write_at(0, &vec![0u8; 100_000]).unwrap(), BUFSIZE);
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(h.write_at(0, &[0u8; 4]), Err(Errno::EAGAIN));
        assert_eq!(h.ready_in_ns(), Some(128 * 1_000_000_000 / 48000));
        // It played 100 frames: 400 bytes of room, exactly.
        PLAYER.lock().unwrap()[0].1 = (BUFSIZE / 4) as u32 - 100;
        assert_eq!(h.write_at(0, &vec![0u8; 1000]).unwrap(), 400);
        // It lets the stream go (another took the device, or it stopped):
        // back on the clock, which drains it.
        PLAYER.lock().unwrap().clear();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(h.write_at(0, &vec![0u8; 1000]).unwrap() > 0);
    }

    #[test]
    fn the_engine_describes_itself_as_osv4_output() {
        let b = audioinfo();
        assert_eq!(i32::from_le_bytes(b[76..80].try_into().unwrap()) as u32 & PCM_CAP_OUTPUT, PCM_CAP_OUTPUT);
        assert_eq!(&b[404..412], b"/dev/dsp");
        let s = sysinfo();
        assert_eq!(s[32], b'4');
        assert_eq!(i32::from_le_bytes(s[196..200].try_into().unwrap()), 1);
    }
}
