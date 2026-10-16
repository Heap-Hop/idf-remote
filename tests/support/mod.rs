// Synthetic ELF32 with RAM data and an optional IDF app descriptor. Not firmware.
pub fn elf() -> Vec<u8> {
    elf_with_descriptor(Some(".flash.appdesc"), false)
}

// `.rodata` models an esp_app_desc symbol embedded in an ordinary section.
// The dedicated descriptor section variants have no symbol (stripped ELF).
pub fn elf_with_descriptor(descriptor: Option<&str>, riscv: bool) -> Vec<u8> {
    let ram = if riscv { 0x40800000u32 } else { 0x40380000 };
    let flash = if riscv { 0x42000020u32 } else { 0x3c000020 };
    let name = descriptor.unwrap_or(".rodata");
    let mut names = b"\0.text\0".to_vec();
    let desc_name = names.len() as u32;
    names.extend_from_slice(name.as_bytes());
    names.push(0);
    let strings_name = names.len() as u32;
    names.extend_from_slice(b".shstrtab\0.strtab\0.symtab\0");
    let mut desc = vec![0; 256];
    if descriptor.is_some() {
        desc[..4].copy_from_slice(&0xabcd5432u32.to_le_bytes());
    }
    let mut symbols = vec![0; 16];
    if descriptor == Some(".rodata") {
        symbols.extend_from_slice(&1u32.to_le_bytes()); // name
        symbols.extend_from_slice(&flash.to_le_bytes());
        symbols.extend_from_slice(&256u32.to_le_bytes());
        symbols.extend_from_slice(&[0x11, 0, 2, 0]); // global object in section 2
    }
    let sections = [
        (1, 1, 6, ram, vec![1, 2, 3, 4], 0, 0, 0),
        (desc_name, 1, 2, flash, desc, 0, 0, 0),
        (strings_name, 3, 0, 0, names, 0, 0, 0),
        (
            strings_name + 10,
            3,
            0,
            0,
            b"\0esp_app_desc\0".to_vec(),
            0,
            0,
            0,
        ),
        (strings_name + 18, 2, 0, 0, symbols, 4, 1, 16),
    ];
    let mut bytes = vec![0u8; 52 + 6 * 40];
    bytes[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
    for (offset, value) in [
        (16, 2u16),
        (18, if riscv { 243 } else { 94 }),
        (40, 52),
        (46, 40),
        (48, 6),
        (50, 3),
    ] {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [(20, 1u32), (24, ram), (32, 52)] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    for (index, (name, kind, flags, address, data, link, info, entry_size)) in
        sections.into_iter().enumerate()
    {
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        let offset = bytes.len() as u32;
        let size = data.len() as u32;
        bytes.extend_from_slice(&data);
        for (field, value) in [
            name, kind, flags, address, offset, size, link, info, 4, entry_size,
        ]
        .into_iter()
        .enumerate()
        {
            let start = 52 + (index + 1) * 40 + field * 4;
            bytes[start..start + 4].copy_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}
