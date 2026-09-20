//! Modbus/TCP wire primitives: function codes, exception PDUs and MBAP framing.

pub const FC_READ_COILS: u8 = 1;
pub const FC_READ_DISCRETE: u8 = 2;
pub const FC_READ_HOLDING: u8 = 3;
pub const FC_READ_INPUT: u8 = 4;
pub const FC_WRITE_COIL: u8 = 5;
pub const FC_WRITE_REGISTER: u8 = 6;
pub const FC_WRITE_COILS: u8 = 15;
pub const FC_WRITE_REGISTERS: u8 = 16;

pub const EXC_ILLEGAL_FUNCTION: u8 = 0x01;
pub const EXC_ILLEGAL_ADDRESS: u8 = 0x02;
// Part of the protocol surface kept complete on purpose: these are referenced by
// the docs and by future call sites, and dropping them would make the module
// harder to reason about than one allow() costs.
#[allow(dead_code)]
pub const EXC_GATEWAY_BUSY: u8 = 0x06;
pub const EXC_GATEWAY_FAIL: u8 = 0x0B;

/// MBAP header length in bytes.
pub const MBAP_LEN: usize = 7;

#[allow(dead_code)]
pub fn is_read_fc(fc: u8) -> bool {
    fc == FC_READ_COILS || fc == FC_READ_DISCRETE || fc == FC_READ_HOLDING || fc == FC_READ_INPUT
}

pub fn is_write_fc(fc: u8) -> bool {
    fc == FC_WRITE_COIL
        || fc == FC_WRITE_REGISTER
        || fc == FC_WRITE_COILS
        || fc == FC_WRITE_REGISTERS
}

/// An exception PDU: the function code with its high bit set, then the code.
pub fn exc_pdu(fc: u8, code: u8) -> Vec<u8> {
    vec![fc | 0x80, code]
}

/// Build an MBAP frame around a PDU. Length counts the unit id + PDU.
pub fn mbap(tid: u16, pid: u16, uid: u8, pdu: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MBAP_LEN + pdu.len());
    out.extend_from_slice(&tid.to_be_bytes());
    out.extend_from_slice(&pid.to_be_bytes());
    out.extend_from_slice(&((pdu.len() + 1) as u16).to_be_bytes());
    out.push(uid);
    out.extend_from_slice(pdu);
    out
}

#[derive(Debug, Clone, Copy)]
pub struct MbapHeader {
    pub tid: u16,
    pub pid: u16,
    pub length: u16,
    pub uid: u8,
}

pub fn parse_mbap(hdr: &[u8]) -> Option<MbapHeader> {
    if hdr.len() < MBAP_LEN {
        return None;
    }
    Some(MbapHeader {
        tid: u16::from_be_bytes([hdr[0], hdr[1]]),
        pid: u16::from_be_bytes([hdr[2], hdr[3]]),
        length: u16::from_be_bytes([hdr[4], hdr[5]]),
        uid: hdr[6],
    })
}

/// Interpret a register as a signed 16-bit value (Python's `raw - 65536 if raw > 32767`).
pub fn as_i16(raw: u16) -> i16 {
    raw as i16
}

/// Pack register values as a read response PDU: fc, byte count, then words.
pub fn read_response(fc: u8, regs: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + regs.len() * 2);
    out.push(fc);
    out.push((regs.len() * 2) as u8);
    for r in regs {
        out.extend_from_slice(&r.to_be_bytes());
    }
    out
}

/// Pack bit values LSB-first into a bit response PDU (as FC1/FC2 require).
pub fn bits_response(fc: u8, values: &[u16], count: usize) -> Vec<u8> {
    let nbytes = (count + 7) / 8;
    let mut data = vec![0u8; nbytes];
    for (i, v) in values.iter().take(count).enumerate() {
        if *v != 0 {
            data[i / 8] |= 1 << (i % 8);
        }
    }
    let mut out = Vec::with_capacity(2 + nbytes);
    out.push(fc);
    out.push(nbytes as u8);
    out.extend_from_slice(&data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mbap_length_counts_unit_and_pdu() {
        let f = mbap(0x4242, 0, 1, &[3, 0, 0, 0, 4]);
        assert_eq!(&f[0..2], &[0x42, 0x42]); // tid
        assert_eq!(&f[2..4], &[0, 0]); // pid
        assert_eq!(u16::from_be_bytes([f[4], f[5]]), 6); // len = uid + 5
        assert_eq!(f[6], 1);
        assert_eq!(&f[7..], &[3, 0, 0, 0, 4]);
    }

    #[test]
    fn parse_round_trips() {
        let f = mbap(7, 0, 9, &[3, 0, 0, 0, 2]);
        let h = parse_mbap(&f).unwrap();
        assert_eq!((h.tid, h.pid, h.uid), (7, 0, 9));
        assert_eq!(h.length, 6);
        assert!(parse_mbap(&[0, 1, 2]).is_none());
    }

    #[test]
    fn signed_conversion_matches_python() {
        assert_eq!(as_i16(0), 0);
        assert_eq!(as_i16(32767), 32767);
        assert_eq!(as_i16(65535), -1);
        assert_eq!(as_i16(65534), -2);
    }

    #[test]
    fn read_response_has_byte_count() {
        assert_eq!(read_response(3, &[1, 2]), vec![3, 4, 0, 1, 0, 2]);
    }

    #[test]
    fn bits_pack_lsb_first() {
        // bit 0 and bit 9 set -> bytes 0b00000001, 0b00000010
        let v: Vec<u16> = (0..10)
            .map(|i| if i == 0 || i == 9 { 1 } else { 0 })
            .collect();
        assert_eq!(
            bits_response(1, &v, 10),
            vec![1, 2, 0b0000_0001, 0b0000_0010]
        );
    }

    #[test]
    fn exception_pdu_sets_high_bit_and_keeps_code() {
        assert_eq!(exc_pdu(3, 0x02), vec![0x83, 0x02]);
        assert_eq!(exc_pdu(16, 0x0B), vec![0x90, 0x0B]);
    }
}
