//! Just enough MPEG-TS to let a receiver join a live hardware-encoded
//! stream: find the keyframes and remember the tables that precede them, so
//! a new receiver starts on `PAT + PMT + keyframe` instead of mid-GOP (where
//! its decoder would have no parameters and show nothing).

pub const PACKET: usize = 188;

/// What the sender needs to know about one 188-byte packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet {
    Pat,
    Pmt,
    /// A video packet that starts a random-access point (an IDR frame).
    Keyframe,
    Other,
}

/// Splits a byte stream into whole packets and remembers the tables.
#[derive(Default)]
pub struct Scanner {
    carry: Vec<u8>,
    pmt_pid: Option<u16>,
    pub pat: Option<[u8; PACKET]>,
    pub pmt: Option<[u8; PACKET]>,
}

fn pid(p: &[u8]) -> u16 {
    (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2])
}

impl Scanner {
    /// Feeds bytes; returns the whole packets they complete, classified.
    /// Anything before the first sync byte is skipped.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<([u8; PACKET], Packet)> {
        self.carry.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;
        loop {
            // Resynchronise on 0x47 that repeats at the packet interval.
            while at + PACKET < self.carry.len() && !(self.carry[at] == 0x47 && self.carry[at + PACKET] == 0x47) {
                at += 1;
            }
            if at + PACKET > self.carry.len() {
                break;
            }
            // The last packet in the buffer can't be cross-checked yet.
            if at + PACKET == self.carry.len() && self.carry[at] != 0x47 {
                at += 1;
                continue;
            }
            let mut p = [0u8; PACKET];
            p.copy_from_slice(&self.carry[at..at + PACKET]);
            at += PACKET;
            let kind = self.classify(&p);
            out.push((p, kind));
        }
        self.carry.drain(..at);
        out
    }

    fn classify(&mut self, p: &[u8; PACKET]) -> Packet {
        let id = pid(p);
        let start = p[1] & 0x40 != 0;
        let afc = (p[3] >> 4) & 3;
        if id == 0 {
            if start && afc & 1 == 1 {
                // pointer_field, then table 0: program_number → PMT PID.
                let ptr = p[4] as usize;
                let t = 5 + ptr;
                if t + 12 <= PACKET && p[t] == 0 {
                    let (prog, pm) = (u16::from_be_bytes([p[t + 8], p[t + 9]]), pid(&[0, p[t + 10], p[t + 11]]));
                    if prog != 0 {
                        self.pmt_pid = Some(pm);
                    }
                }
            }
            self.pat = Some(*p);
            return Packet::Pat;
        }
        if Some(id) == self.pmt_pid {
            self.pmt = Some(*p);
            return Packet::Pmt;
        }
        if start && afc & 2 != 0 && p[4] > 0 && p[5] & 0x40 != 0 {
            return Packet::Keyframe;
        }
        Packet::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(pid: u16, start: bool, af: Option<u8>) -> [u8; PACKET] {
        let mut p = [0xffu8; PACKET];
        p[0] = 0x47;
        p[1] = (u8::from(start) << 6) | (pid >> 8) as u8;
        p[2] = pid as u8;
        match af {
            Some(flags) => {
                p[3] = 0x30;
                p[4] = 7;
                p[5] = flags;
            }
            None => p[3] = 0x10,
        }
        p
    }

    fn pat(pmt: u16) -> [u8; PACKET] {
        let mut p = pkt(0, true, None);
        p[4] = 0; // pointer
        p[5] = 0; // table id
        p[13] = 0;
        p[14] = 1; // program 1
        p[15] = 0xe0 | (pmt >> 8) as u8;
        p[16] = pmt as u8;
        p
    }

    #[test]
    fn finds_keyframes_and_remembers_tables_across_odd_chunks() {
        let (a, b, c, d) = (pat(0x1000), pkt(0x1000, true, None), pkt(0x100, true, Some(0x40)), pkt(0x100, false, None));
        let mut stream = vec![0x12, 0x34, 0x56]; // junk before sync
        for p in [&a, &b, &c, &d, &d] {
            stream.extend_from_slice(p);
        }
        let mut s = Scanner::default();
        let mut got = Vec::new();
        for chunk in stream.chunks(100) {
            got.extend(s.feed(chunk));
        }
        // The final packet can't be confirmed until another follows.
        let kinds: Vec<_> = got.iter().map(|g| g.1).collect();
        assert_eq!(&kinds[..4], &[Packet::Pat, Packet::Pmt, Packet::Keyframe, Packet::Other]);
        assert!(s.pat.is_some() && s.pmt.is_some());
    }
}
