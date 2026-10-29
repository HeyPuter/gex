//! `wl_data_device_manager`: data devices, sources and offers.
//!
//! What a client's data device takes part in:
//!
//! - **A drag from the host** onto one of the client's surfaces: the host is
//!   the drag's source (a file dropped on the page, say). [`Client::drag_enter`]
//!   makes an offer of the host's MIME types and enters the surface;
//!   [`Client::drag_motion`], [`Client::drag_leave`] and [`Client::drag_drop`]
//!   follow the pointer. The client says what it would take with
//!   `wl_data_offer.accept` (and, from version 3, `set_actions`), which
//!   [`Client::drag_status`] reports, and reads the data with
//!   `wl_data_offer.receive`: a descriptor for the host to write the bytes
//!   into, from [`Client::take_receives`].
//! - **Sources** the client makes, with the types and actions it offers, and
//!   the selection it sets. A new selection is the host's to take
//!   ([`Client::take_selection_change`]); it asks the source for its data
//!   ([`Client::ask_source`]) with a descriptor of its own to read.
//! - **The host's selection** (its clipboard): offered to the client as
//!   `wl_data_device.selection` ([`Client::offer_selection`]); the client's
//!   `receive`s of it come out of [`Client::take_receives`] like a drag's.
//!
//! A client's own drags (`start_drag`) are not carried: nothing takes them.

use crate::wire::{Fixed, Reader, Writer};
use crate::{Client, Object};
use std::collections::HashMap;

/// `wl_data_device_manager.dnd_action` bits.
pub const DND_NONE: u32 = 0;
pub const DND_COPY: u32 = 1;
pub const DND_MOVE: u32 = 2;
pub const DND_ASK: u32 = 4;

/// The first id the compositor gives an object it makes (Wayland's server
/// id range).
const SERVER_IDS: u32 = 0xff00_0000;

/// `wl_data_offer.set_actions` and `wl_data_offer.finish` are version 3.
const ACTIONS_SINCE: u32 = 3;

/// A source the client made: what it offers.
#[derive(Clone, Debug, Default)]
pub struct Source {
    pub mimes: Vec<String>,
    pub actions: u32,
}

/// An offer the compositor made to the client.
#[derive(Clone, Debug, Default)]
struct Offer {
    version: u32,
    mimes: Vec<String>,
    /// The types and actions of the source, for a drag.
    source_actions: u32,
    /// What the client last said it takes: `accept`'s type (`None`: none),
    /// `set_actions`' actions and preferred one.
    accepted: Option<String>,
    actions: u32,
    preferred: u32,
    /// The action the compositor chose and told the client (`action`).
    chosen: u32,
    finished: bool,
    /// The client destroyed it after the drop: kept for [`Client::drag_status`].
    destroyed: bool,
}

/// The drag over this client, from its enter to its leave or drop.
#[derive(Clone, Copy, Debug)]
struct Drag {
    offer: u32,
    surface: u32,
    serial: u32,
    dropped: bool,
}

/// What a client said of the host's drag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragStatus {
    /// The surface the drag is over.
    pub surface: u32,
    /// The type the client accepts, `None` when it takes nothing there.
    pub accepted: Option<String>,
    /// The action chosen, for a version 3 client ([`DND_COPY`] ...); a
    /// version 1 or 2 client's drop is always a copy.
    pub action: u32,
    /// Whether the drop happened, and the client said it is done with it
    /// (`finish`, version 3; a lower version is done once it destroys the
    /// offer).
    pub dropped: bool,
    pub finished: bool,
}

impl DragStatus {
    /// Whether a drop now would be taken.
    pub fn takes(&self) -> bool {
        self.accepted.is_some() && self.action != DND_NONE
    }
}

/// A client's request for an offer's data: `fd` is a descriptor it sent
/// (the host's token for it, as everywhere in this crate), for the host to
/// write the data of `mime` into and close. Until taken it counts as in use
/// ([`Client::fds_in_use`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receive {
    pub offer: u32,
    pub mime: String,
    pub fd: u32,
}

/// A client's data devices and what they hold.
#[derive(Debug, Default)]
pub(crate) struct DataState {
    /// The data devices, by id, and their versions (the manager's).
    devices: HashMap<u32, u32>,
    sources: HashMap<u32, Source>,
    offers: HashMap<u32, Offer>,
    drag: Option<Drag>,
    receives: Vec<Receive>,
    /// The source the client set as the selection, if it still exists.
    selection: Option<u32>,
    /// `set_selection` was asked since the host last took it.
    selection_changed: bool,
    /// The host's selection, as last offered: the offer's id.
    host_selection: Option<u32>,
    next_id: u32,
}

/// The action for a drop: the client's preferred one when the source has
/// it, else the first both have.
fn choose(source: u32, actions: u32, preferred: u32) -> u32 {
    let both = source & actions;
    if preferred & both != 0 {
        preferred & both
    } else if both != 0 {
        1 << both.trailing_zeros()
    } else {
        DND_NONE
    }
}

impl Client {
    // -- requests -----------------------------------------------------------

    pub(crate) fn data_device_manager(&mut self, version: u32, opcode: u16, r: &mut Reader) -> bool {
        match opcode {
            // create_data_source(id)
            0 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::DataSource);
                self.data.sources.insert(id, Source::default());
                true
            }
            // get_data_device(id, seat)
            1 => {
                let (Some(id), Some(_seat)) = (r.new_id(), r.object()) else { return false };
                self.objects.insert(id, Object::DataDevice);
                self.data.devices.insert(id, version);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn data_device(&mut self, object: u32, opcode: u16, r: &mut Reader) -> bool {
        match opcode {
            // start_drag(source, origin, icon, serial): the client's own drag,
            // which nothing here takes.
            0 => true,
            // set_selection(source, serial)
            1 => {
                let source = r.object().unwrap_or(0);
                self.data.selection = (source != 0 && self.data.sources.contains_key(&source)).then_some(source);
                self.data.selection_changed = true;
                true
            }
            // release (version 2)
            2 => {
                self.objects.remove(&object);
                self.data.devices.remove(&object);
                self.delete_id(object);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn data_source(&mut self, object: u32, opcode: u16, r: &mut Reader) -> bool {
        match opcode {
            // offer(mime_type)
            0 => {
                let Some(mime) = r.string() else { return false };
                if let Some(s) = self.data.sources.get_mut(&object) {
                    s.mimes.push(mime);
                }
                true
            }
            // destroy
            1 => {
                self.objects.remove(&object);
                self.data.sources.remove(&object);
                if self.data.selection == Some(object) {
                    self.data.selection = None;
                    self.data.selection_changed = true;
                }
                self.delete_id(object);
                true
            }
            // set_actions(dnd_actions) (version 3)
            2 => {
                let Some(a) = r.uint() else { return false };
                if let Some(s) = self.data.sources.get_mut(&object) {
                    s.actions = a;
                }
                true
            }
            _ => false,
        }
    }

    pub(crate) fn data_offer(&mut self, object: u32, opcode: u16, r: &mut Reader) -> bool {
        match opcode {
            // accept(serial, mime_type or null)
            0 => {
                let (Some(_serial), Some(mime)) = (r.uint(), r.string()) else { return false };
                if let Some(o) = self.data.offers.get_mut(&object) {
                    o.accepted = (!mime.is_empty() && o.mimes.contains(&mime)).then_some(mime);
                }
                true
            }
            // receive(mime_type, fd)
            1 => {
                let (Some(mime), Some(fd)) = (r.string(), r.fd()) else { return false };
                self.data.receives.push(Receive { offer: object, mime, fd });
                true
            }
            // destroy
            2 => {
                self.objects.remove(&object);
                if self.data.drag.is_some_and(|d| d.offer == object && d.dropped) {
                    // The drop's outcome stays readable until the next drag.
                    if let Some(o) = self.data.offers.get_mut(&object) {
                        o.destroyed = true;
                    }
                } else {
                    self.data.offers.remove(&object);
                    if self.data.drag.is_some_and(|d| d.offer == object) {
                        self.data.drag = None;
                    }
                }
                true
            }
            // finish (version 3)
            3 => {
                if let Some(o) = self.data.offers.get_mut(&object) {
                    o.finished = true;
                }
                true
            }
            // set_actions(dnd_actions, preferred_action) (version 3)
            4 => {
                let (Some(actions), Some(preferred)) = (r.uint(), r.uint()) else { return false };
                let Some(o) = self.data.offers.get_mut(&object) else { return true };
                o.actions = actions;
                o.preferred = preferred;
                let chosen = choose(o.source_actions, actions, preferred);
                if chosen != o.chosen {
                    o.chosen = chosen;
                    self.send(Writer::new(object, 2).uint(chosen)); // action
                }
                true
            }
            _ => false,
        }
    }

    // -- the host's drag ----------------------------------------------------

    /// Whether the client has a data device, so a drag can reach it.
    pub fn has_data_device(&self) -> bool {
        !self.data.devices.is_empty()
    }

    /// A drag from the host enters `surface` at surface-local `(x, y)`,
    /// offering `mimes` with the dnd `actions` the host allows. Leaves the
    /// drag before it, if any. False when the client has no data device or no
    /// such surface.
    pub fn drag_enter(&mut self, surface: u32, x: i32, y: i32, mimes: &[&str], actions: u32) -> bool {
        self.drag_leave();
        let Some((&device, &version)) = self.data.devices.iter().min_by_key(|d| *d.0) else {
            return false;
        };
        if !self.surfaces.contains_key(&surface) {
            return false;
        }
        let offer = SERVER_IDS + self.data.next_id;
        self.data.next_id += 1;
        self.objects.insert(offer, Object::DataOffer);
        // data_offer(id), then the offer's types and, from version 3, the
        // source's actions: all before the enter that names it.
        self.send(Writer::new(device, 0).new_id(offer));
        for m in mimes {
            self.send(Writer::new(offer, 0).string(m));
        }
        if version >= ACTIONS_SINCE {
            self.send(Writer::new(offer, 1).uint(actions));
        }
        let serial = self.serial();
        // enter(serial, surface, x, y, id)
        self.send(
            Writer::new(device, 1)
                .uint(serial)
                .object(surface)
                .fixed(Fixed::from_int(x))
                .fixed(Fixed::from_int(y))
                .object(offer),
        );
        self.data.offers.insert(
            offer,
            Offer {
                version,
                mimes: mimes.iter().map(|m| m.to_string()).collect(),
                source_actions: actions,
                // A version 1 or 2 drop is a copy; version 3 waits for set_actions.
                chosen: if version >= ACTIONS_SINCE { DND_NONE } else { DND_COPY },
                ..Default::default()
            },
        );
        self.data.drag = Some(Drag { offer, surface, serial, dropped: false });
        true
    }

    fn drag_device(&self) -> Option<u32> {
        self.data.devices.keys().min().copied()
    }

    /// The host's drag moved, to surface-local `(x, y)` on the surface it
    /// entered.
    pub fn drag_motion(&mut self, x: i32, y: i32, time: u32) {
        let (Some(d), Some(device)) = (self.data.drag, self.drag_device()) else { return };
        if d.dropped {
            return;
        }
        self.send(Writer::new(device, 3).uint(time).fixed(Fixed::from_int(x)).fixed(Fixed::from_int(y)));
    }

    /// The host's drag left the client's surfaces, or was cancelled.
    pub fn drag_leave(&mut self) {
        let Some(d) = self.data.drag.take() else { return };
        if self.data.offers.get(&d.offer).is_some_and(|o| o.destroyed) {
            self.data.offers.remove(&d.offer);
        }
        if d.dropped {
            // The drop is over for the compositor; the offer is the client's
            // until it destroys it.
            return;
        }
        if let Some(device) = self.drag_device() {
            self.send(Writer::new(device, 2)); // leave
        }
        // The offer is dead to the client after a leave: it destroys it.
    }

    /// The host's drag was released over the surface. A drop the client
    /// does not take (no accepted type, or no action) is a leave instead,
    /// as the protocol asks: false then.
    pub fn drag_drop(&mut self) -> bool {
        let Some(status) = self.drag_status() else { return false };
        if !status.takes() {
            self.drag_leave();
            return false;
        }
        let Some(device) = self.drag_device() else { return false };
        self.send(Writer::new(device, 4)); // drop
        if let Some(d) = self.data.drag.as_mut() {
            d.dropped = true;
        }
        true
    }

    /// What the client said of the host's drag so far.
    pub fn drag_status(&self) -> Option<DragStatus> {
        let d = self.data.drag?;
        let o = self.data.offers.get(&d.offer)?;
        Some(DragStatus {
            surface: d.surface,
            accepted: o.accepted.clone(),
            action: o.chosen,
            dropped: d.dropped,
            finished: o.finished || (o.destroyed && o.version < ACTIONS_SINCE),
        })
    }

    /// The serial of the drag's enter: what `accept` names.
    pub fn drag_serial(&self) -> Option<u32> {
        self.data.drag.map(|d| d.serial)
    }

    /// The offers' data the client asked for since the last call.
    pub fn take_receives(&mut self) -> Vec<Receive> {
        std::mem::take(&mut self.data.receives)
    }

    /// The descriptors waiting in [`Client::take_receives`].
    pub(crate) fn receive_fds(&self) -> impl Iterator<Item = u32> + '_ {
        self.data.receives.iter().map(|r| r.fd)
    }

    // -- the selection --------------------------------------------------------

    /// The client's new selection since the last call: `Some(Some((source,
    /// types)))` for a source, `Some(None)` when it cleared it.
    pub fn take_selection_change(&mut self) -> Option<Option<(u32, Vec<String>)>> {
        if !std::mem::take(&mut self.data.selection_changed) {
            return None;
        }
        Some(self.data.selection.and_then(|s| self.data.sources.get(&s).map(|src| (s, src.mimes.clone()))))
    }

    /// Ask `source` for its data as `mime` (`wl_data_source.send`): the
    /// descriptor it writes into must ride with these bytes, the next
    /// staged on the socket.
    pub fn ask_source(&mut self, source: u32, mime: &str) {
        self.send(Writer::new(source, 1).string(mime));
    }

    /// Offer the host's selection, as `mimes`, to the client: a new offer
    /// and `wl_data_device.selection` naming it. False with no data device.
    pub fn offer_selection(&mut self, mimes: &[&str]) -> Option<u32> {
        let (&device, &version) = self.data.devices.iter().min_by_key(|d| *d.0)?;
        let offer = SERVER_IDS + self.data.next_id;
        self.data.next_id += 1;
        self.objects.insert(offer, Object::DataOffer);
        self.send(Writer::new(device, 0).new_id(offer)); // data_offer
        for m in mimes {
            self.send(Writer::new(offer, 0).string(m));
        }
        self.send(Writer::new(device, 5).object(offer)); // selection
        self.data.offers.insert(
            offer,
            Offer { version, mimes: mimes.iter().map(|m| m.to_string()).collect(), ..Default::default() },
        );
        self.data.host_selection = Some(offer);
        Some(offer)
    }

    /// Whether `offer` is the host's selection (its receives are the
    /// clipboard's, not a drag's).
    pub fn is_host_selection(&self, offer: u32) -> bool {
        self.data.host_selection == Some(offer)
    }

    /// The sources the client made, and the one it set as the selection.
    pub fn data_sources(&self) -> (&HashMap<u32, Source>, Option<u32>) {
        (&self.data.sources, self.data.selection)
    }
}

#[cfg(test)]
mod tests {
    use super::choose;
    use super::*;

    #[test]
    fn choose_prefers_the_clients_preference() {
        assert_eq!(choose(DND_COPY | DND_MOVE, DND_COPY | DND_MOVE, DND_MOVE), DND_MOVE);
        assert_eq!(choose(DND_COPY, DND_COPY | DND_MOVE, DND_MOVE), DND_COPY);
        assert_eq!(choose(DND_COPY, DND_MOVE, DND_MOVE), DND_NONE);
    }
}
