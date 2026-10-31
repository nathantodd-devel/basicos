//! The two checksums ext4 uses: CRC-32C (Castagnoli) for metadata_csum
//! and the journal, and CRC-16 (the ANSI/`0xA001` one) for the older
//! `uninit_bg` group-descriptor checksum.

const fn make_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC32C_TABLE: [u32; 256] = make_table();

/// Raw CRC-32C update, *without* the usual pre/post inversion — this is
/// what ext4's `ext4_chksum()` does: the caller supplies the running
/// value (seed `!0` for a fresh one) and no final xor is applied.
pub fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC32C_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// CRC-16 as used by `ext4_group_desc_csum` in non-metadata_csum mode:
/// reflected, polynomial `0xA001`, no final xor.
pub fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_vector() {
        // The standard check value for CRC-32C("123456789") is 0xE3069283
        // with the usual init/xorout of 0xFFFFFFFF.
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
    }

    #[test]
    fn crc32c_is_incremental() {
        let whole = crc32c(!0, b"hello world");
        let split = crc32c(crc32c(!0, b"hello "), b"world");
        assert_eq!(whole, split);
    }

    #[test]
    fn crc16_known_vector() {
        // CRC-16/ARC of "123456789" is 0xBB3D (init 0).
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }
}
