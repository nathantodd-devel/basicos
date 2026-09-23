use core::arch::x86_64::__cpuid;

pub struct CpuVendor([u8; 12]);

impl CpuVendor {
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).unwrap_or("????????????")
    }
}

pub fn get_cpu_vendor() -> CpuVendor {
    let res = __cpuid(0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&res.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&res.edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&res.ecx.to_le_bytes());
    CpuVendor(vendor)
}

pub struct CpuBrand([u8; 48]);

impl CpuBrand {
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0)
            .unwrap_or("unknown")
            .trim_end_matches('\0')
            .trim_end() // brand strings are space-padded too
    }
}

pub fn get_cpu_brand() -> CpuBrand {
    let mut brand = [0u8; 48];
    for i in 0..3u32 {
        let res = __cpuid(0x80000002 + i);
        let offset = (i as usize) * 16;
        brand[offset..offset + 4].copy_from_slice(&res.eax.to_le_bytes());
        brand[offset + 4..offset + 8].copy_from_slice(&res.ebx.to_le_bytes());
        brand[offset + 8..offset + 12].copy_from_slice(&res.ecx.to_le_bytes());
        brand[offset + 12..offset + 16].copy_from_slice(&res.edx.to_le_bytes());
    }
    CpuBrand(brand)
}
