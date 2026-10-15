// Synthetic ELF32/Xtensa with one RAM section. Not executable firmware.
pub fn elf() -> Vec<u8> {
    let mut bytes = vec![0u8; 193];
    bytes[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
    for (offset, value) in [(16, 2u16), (18, 94), (40, 52), (46, 40), (48, 3), (50, 2)] {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [
        (20, 1u32),
        (24, 0x40380000),
        (32, 52),
        (92, 1),
        (96, 1),
        (100, 6),
        (104, 0x40380000),
        (108, 172),
        (112, 4),
        (124, 4),
        (132, 7),
        (136, 3),
        (148, 176),
        (152, 17),
        (164, 1),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes[172..176].copy_from_slice(&[1, 2, 3, 4]);
    bytes[176..].copy_from_slice(b"\0.text\0.shstrtab\0");
    bytes
}
