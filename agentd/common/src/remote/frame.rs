//! Frames between `agentd remote` and the holder, and between a hook and the
//! holder (design.md, "Remote panes"): a type byte, a 4-byte big-endian
//! length, the payload.

use std::io::{self, Read, Write};

/// Up, once: `term`, `rows`, `cols`, `have` (JSON).
pub const HELLO: u8 = b'H';
/// Up: keys, as typed.
pub const INPUT: u8 = b'I';
/// Up: `rows`, `cols` (JSON).
pub const RESIZE: u8 = b'R';
/// Down, first: `new`, `at` (JSON).
pub const ATTACHED: u8 = b'A';
/// Down: the program's output.
pub const OUTPUT: u8 = b'O';
/// Down: a hook's request, for the local daemon (JSON).
pub const EVENT: u8 = b'E';
/// Down, last: `code` (JSON).
pub const EXIT: u8 = b'X';
/// Down, last: `why` (JSON).
pub const DETACHED: u8 = b'D';
/// A hook's request, and the holder's reply (JSON).
pub const HOOK: u8 = b'K';
/// `agentd hold` listing: the question, and `attached`, `running` (JSON).
pub const QUERY: u8 = b'Q';

/// The biggest frame is a 64 KiB chunk of output; a hook's request is a few
/// KiB. More is a broken or hostile stream, refused before it is read.
const MAX: usize = 256 << 10;

/// `None` for a payload the other end would refuse.
pub fn encode(kind: u8, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > MAX {
        return None;
    }
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(kind);
    buf.extend_from_slice(&u32::try_from(payload.len()).ok()?.to_be_bytes());
    buf.extend_from_slice(payload);
    Some(buf)
}

/// One frame, in one write: frames from several threads never interleave.
pub fn write(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let frame = encode(kind, payload)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too big"))?;
    w.write_all(&frame)?;
    w.flush()
}

/// One frame; `None` when the stream ends cleanly before it.
pub fn read(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    loop {
        match r.read(&mut head[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    r.read_exact(&mut head[1..])?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too big"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((head[0], payload)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_clean_end() {
        let mut buf = Vec::new();
        write(&mut buf, INPUT, b"ls\r").unwrap();
        write(&mut buf, EXIT, b"").unwrap();
        let mut r = &buf[..];
        assert_eq!(read(&mut r).unwrap(), Some((INPUT, b"ls\r".to_vec())));
        assert_eq!(read(&mut r).unwrap(), Some((EXIT, Vec::new())));
        assert_eq!(read(&mut r).unwrap(), None);
    }

    #[test]
    fn cut_or_huge_frames_are_errors() {
        let buf = encode(OUTPUT, b"hello").unwrap();
        assert!(read(&mut &buf[..3]).is_err());
        assert!(read(&mut &buf[..7]).is_err());
        let huge = [OUTPUT, 0xff, 0xff, 0xff, 0xff];
        assert!(read(&mut &huge[..]).is_err());
        // Nor sent.
        assert!(encode(OUTPUT, &vec![0; MAX + 1]).is_none());
        assert!(write(&mut Vec::new(), OUTPUT, &vec![0; MAX + 1]).is_err());
    }
}
