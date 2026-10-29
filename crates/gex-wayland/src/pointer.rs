//! Locked pointers and relative motion: `zwp_pointer_constraints_v1` and
//! `zwp_relative_pointer_manager_v1`, what a game (or Xwayland, for an X
//! client that grabs the pointer and hides it) uses for mouse look.
//!
//! A client asks to lock the pointer to a surface; the surface's scene
//! record says so ([`SurfaceInfo::pointer_lock`](crate::SurfaceInfo)), and
//! the host decides when the lock holds (in a browser, the page's pointer
//! lock, which takes a click). While it holds, [`Client::pointer_locked`]
//! has said `locked`, and the host's movement arrives as
//! [`Client::pointer_relative`]: `relative_motion` to the client's relative
//! pointers, with no absolute motion. A confined pointer is accepted and
//! told it is confined; the host keeps the pointer in the window anyway.

use crate::wire::{Fixed, Reader, Writer};
use crate::{Client, Object};

/// `zwp_pointer_constraints_v1.lifetime`.
const ONESHOT: u32 = 1;

/// A `zwp_locked_pointer_v1` or `zwp_confined_pointer_v1`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Constraint {
    pub surface: u32,
    pub lock: bool,
    pub persistent: bool,
    /// The host says it holds (`locked`/`confined` was sent).
    pub active: bool,
    /// A oneshot lock that was released: it never holds again.
    pub defunct: bool,
}

impl Client {
    /// `zwp_relative_pointer_manager_v1`: destroy(0),
    /// get_relative_pointer(1: new_id, pointer).
    pub(crate) fn relative_pointer_manager(&mut self, object: u32, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            0 => {
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            1 => {
                let (Some(id), Some(_pointer)) = (r.new_id(), r.object()) else {
                    return false;
                };
                self.objects.insert(id, Object::RelativePointer);
                self.relative_pointers.push(id);
                true
            }
            _ => false,
        }
    }

    /// `zwp_relative_pointer_v1`: destroy(0).
    pub(crate) fn relative_pointer(&mut self, object: u32, opcode: u16) -> bool {
        if opcode != 0 {
            return false;
        }
        self.objects.remove(&object);
        self.relative_pointers.retain(|&p| p != object);
        self.delete_id(object);
        true
    }

    /// `zwp_pointer_constraints_v1`: destroy(0), lock_pointer(1) and
    /// confine_pointer(2), each (new_id, surface, pointer, region, lifetime).
    pub(crate) fn pointer_constraints(&mut self, object: u32, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            0 => {
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            1 | 2 => {
                let (Some(id), Some(surface), Some(_pointer), Some(_region), Some(lifetime)) =
                    (r.new_id(), r.object(), r.object(), r.object(), r.uint())
                else {
                    return false;
                };
                let lock = opcode == 1;
                self.objects.insert(id, if lock { Object::LockedPointer } else { Object::ConfinedPointer });
                self.constraints.insert(
                    id,
                    Constraint { surface, lock, persistent: lifetime != ONESHOT, active: false, defunct: false },
                );
                if lock {
                    self.dirty = true;
                } else {
                    // Confined: the host keeps the pointer inside the window
                    // already. confined(0).
                    if let Some(c) = self.constraints.get_mut(&id) {
                        c.active = true;
                    }
                    self.send(Writer::new(id, 0));
                }
                true
            }
            _ => false,
        }
    }

    /// `zwp_locked_pointer_v1`: destroy(0), set_cursor_position_hint(1),
    /// set_region(2); `zwp_confined_pointer_v1`: destroy(0), set_region(1).
    pub(crate) fn constrained_pointer(&mut self, object: u32, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            0 => {
                if let Some(c) = self.constraints.remove(&object) {
                    if c.lock {
                        self.dirty = true;
                    }
                }
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            // The cursor position hint or the region: nothing here uses them.
            1 | 2 => {
                let _ = r;
                true
            }
            _ => false,
        }
    }

    /// Whether some lock on `surface` would take the pointer.
    pub(crate) fn wants_lock(&self, surface: u32) -> bool {
        self.constraints.values().any(|c| c.lock && c.surface == surface && !c.defunct)
    }

    /// The host's lock on `surface` began (`on`) or ended: `locked` or
    /// `unlocked` to the client's locks on it. A oneshot lock that ends is
    /// done for good.
    pub fn pointer_locked(&mut self, surface: u32, on: bool) {
        let ids: Vec<u32> = self
            .constraints
            .iter()
            .filter(|(_, c)| c.lock && c.surface == surface && !c.defunct && c.active != on)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            let Some(c) = self.constraints.get_mut(&id) else { continue };
            c.active = on;
            if !on && !c.persistent {
                c.defunct = true;
                self.dirty = true;
            }
            self.send(Writer::new(id, if on { 0 } else { 1 }));
        }
    }

    /// The host's pointer moved by `(dx, dy)` (device pixels, in 1/256)
    /// while locked: `relative_motion` to every relative pointer.
    pub fn pointer_relative(&mut self, dx: i32, dy: i32, time_ms: u32) {
        if self.relative_pointers.is_empty() {
            return;
        }
        let us = u64::from(time_ms) * 1000;
        for p in self.relative_pointers.clone() {
            // relative_motion(utime_hi, utime_lo, dx, dy, dx_unaccel, dy_unaccel)
            self.send(
                Writer::new(p, 0)
                    .uint((us >> 32) as u32)
                    .uint(us as u32)
                    .fixed(Fixed(dx))
                    .fixed(Fixed(dy))
                    .fixed(Fixed(dx))
                    .fixed(Fixed(dy)),
            );
        }
        self.pointer_frame();
    }
}
