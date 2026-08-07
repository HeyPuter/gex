//! The Wayland wire format.
//!
//! Every message is an object id, then a word holding the opcode in its low
//! half and the total byte length in its high half, then arguments. Everything
//! is 32-bit aligned and little-endian, and the length counts the eight-byte
//! header. That is the whole format; there is no negotiation and no framing
//! beyond it, which is why a stream socket is enough.
//!
//! Descriptors do not appear in the byte stream at all. They travel in the
//! `SCM_RIGHTS` control message of whichever `sendmsg` carried the bytes, and
//! the reader pairs them up by order: the *n*th descriptor received belongs to
//! the *n*th `fd` argument read. That is the only thing about this format that
//! is easy to get wrong, and it is why [`Reader`] takes the descriptor queue.

/// A signed 24.8 fixed-point number, which is how Wayland says "a coordinate".
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Fixed(pub i32);

impl Fixed {
    pub fn from_int(v: i32) -> Fixed {
        Fixed(v << 8)
    }
    pub fn to_int(self) -> i32 {
        self.0 >> 8
    }
}

/// Reads arguments out of one message body.
pub struct Reader<'a> {
    body: &'a [u8],
    at: usize,
    /// Descriptors that arrived with these bytes, taken in order.
    fds: &'a mut Vec<u32>,
}

impl<'a> Reader<'a> {
    pub fn new(body: &'a [u8], fds: &'a mut Vec<u32>) -> Reader<'a> {
        Reader { body, at: 0, fds }
    }

    fn word(&mut self) -> Option<u32> {
        let end = self.at + 4;
        if end > self.body.len() {
            return None;
        }
        let w = u32::from_le_bytes(self.body[self.at..end].try_into().ok()?);
        self.at = end;
        Some(w)
    }

    pub fn uint(&mut self) -> Option<u32> {
        self.word()
    }

    pub fn int(&mut self) -> Option<i32> {
        self.word().map(|w| w as i32)
    }

    pub fn fixed(&mut self) -> Option<Fixed> {
        self.word().map(|w| Fixed(w as i32))
    }

    /// An object id, or zero for "no object".
    pub fn object(&mut self) -> Option<u32> {
        self.word()
    }

    pub fn new_id(&mut self) -> Option<u32> {
        self.word()
    }

    /// A length-prefixed string. The length counts a trailing NUL and the
    /// whole thing is padded out to a word.
    pub fn string(&mut self) -> Option<String> {
        let len = self.word()? as usize;
        if len == 0 {
            return Some(String::new());
        }
        let end = self.at + len;
        if end > self.body.len() {
            return None;
        }
        let raw = &self.body[self.at..end - 1];
        self.at = pad4(end);
        Some(String::from_utf8_lossy(raw).into_owned())
    }

    pub fn array(&mut self) -> Option<Vec<u8>> {
        let len = self.word()? as usize;
        let end = self.at + len;
        if end > self.body.len() {
            return None;
        }
        let out = self.body[self.at..end].to_vec();
        self.at = pad4(end);
        Some(out)
    }

    /// The next descriptor that came with this batch of bytes.
    pub fn fd(&mut self) -> Option<u32> {
        if self.fds.is_empty() {
            None
        } else {
            Some(self.fds.remove(0))
        }
    }
}

/// Builds one message.
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// Start a message from `object` with `opcode`. The length is filled in
    /// by [`Writer::finish`], because it is not known until the arguments are.
    pub fn new(object: u32, opcode: u16) -> Writer {
        let mut buf = Vec::with_capacity(32);
        buf.extend_from_slice(&object.to_le_bytes());
        buf.extend_from_slice(&(opcode as u32).to_le_bytes());
        Writer { buf }
    }

    pub fn uint(mut self, v: u32) -> Writer {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn int(self, v: i32) -> Writer {
        self.uint(v as u32)
    }

    pub fn fixed(self, v: Fixed) -> Writer {
        self.uint(v.0 as u32)
    }

    pub fn object(self, v: u32) -> Writer {
        self.uint(v)
    }

    pub fn new_id(self, v: u32) -> Writer {
        self.uint(v)
    }

    pub fn string(mut self, s: &str) -> Writer {
        let raw = s.as_bytes();
        let len = raw.len() + 1;
        self.buf.extend_from_slice(&(len as u32).to_le_bytes());
        self.buf.extend_from_slice(raw);
        self.buf.push(0);
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
        self
    }

    pub fn array(mut self, a: &[u8]) -> Writer {
        self.buf.extend_from_slice(&(a.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(a);
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
        self
    }

    /// Fill in the length and hand back the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        let len = self.buf.len() as u32;
        let opcode = u32::from_le_bytes(self.buf[4..8].try_into().unwrap()) & 0xffff;
        self.buf[4..8].copy_from_slice(&((len << 16) | opcode).to_le_bytes());
        self.buf
    }
}

/// Round up to the next word, which is where the next argument starts.
pub fn pad4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// One parsed message header.
pub struct Header {
    pub object: u32,
    pub opcode: u16,
    pub size: u16,
}

/// Read a header, if a whole message is present.
pub fn header(buf: &[u8]) -> Option<Header> {
    if buf.len() < 8 {
        return None;
    }
    let object = u32::from_le_bytes(buf[0..4].try_into().ok()?);
    let word = u32::from_le_bytes(buf[4..8].try_into().ok()?);
    let opcode = (word & 0xffff) as u16;
    let size = (word >> 16) as u16;
    if (size as usize) < 8 || buf.len() < size as usize {
        return None;
    }
    Some(Header {
        object,
        opcode,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_round_trips() {
        let bytes = Writer::new(7, 3).uint(42).string("hello").int(-1).finish();
        let h = header(&bytes).expect("a whole message");
        assert_eq!(h.object, 7);
        assert_eq!(h.opcode, 3);
        assert_eq!(h.size as usize, bytes.len());

        let mut fds = Vec::new();
        let mut r = Reader::new(&bytes[8..], &mut fds);
        assert_eq!(r.uint(), Some(42));
        assert_eq!(r.string().as_deref(), Some("hello"));
        assert_eq!(r.int(), Some(-1));
    }

    #[test]
    fn a_string_is_padded_to_a_word_so_the_next_argument_is_aligned() {
        // "ab" is two bytes plus a NUL, so three, which pads to four.
        let bytes = Writer::new(1, 0).string("ab").uint(0xdeadbeef).finish();
        assert_eq!(bytes.len(), 8 + 4 + 4 + 4);
        let mut fds = Vec::new();
        let mut r = Reader::new(&bytes[8..], &mut fds);
        assert_eq!(r.string().as_deref(), Some("ab"));
        assert_eq!(r.uint(), Some(0xdeadbeef));
    }

    #[test]
    fn an_empty_string_is_a_zero_length_and_nothing_else() {
        let bytes = Writer::new(1, 0).string("").uint(5).finish();
        let mut fds = Vec::new();
        let mut r = Reader::new(&bytes[8..], &mut fds);
        assert_eq!(r.string().as_deref(), Some(""));
        assert_eq!(r.uint(), Some(5));
    }

    #[test]
    fn a_partial_message_is_not_a_header() {
        let bytes = Writer::new(1, 0).uint(1).uint(2).finish();
        assert!(header(&bytes[..bytes.len() - 1]).is_none());
        assert!(header(&bytes).is_some());
    }

    #[test]
    fn descriptors_are_taken_in_the_order_they_arrived() {
        let bytes = Writer::new(1, 0).uint(9).finish();
        let mut fds = vec![11, 12];
        let mut r = Reader::new(&bytes[8..], &mut fds);
        assert_eq!(r.uint(), Some(9));
        assert_eq!(r.fd(), Some(11));
        assert_eq!(r.fd(), Some(12));
        assert_eq!(r.fd(), None);
    }

    #[test]
    fn fixed_point_is_twenty_four_and_eight() {
        assert_eq!(Fixed::from_int(3).0, 768);
        assert_eq!(Fixed::from_int(-2).to_int(), -2);
        assert_eq!(Fixed(384).to_int(), 1);
    }
}
