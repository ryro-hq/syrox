//! Bounded ELF64 parsing for the verified Linux runtime closure.

use super::{RuntimeError, invalid};

pub(super) const MAX_NEEDED: usize = 128;
const MAX_HEADERS: usize = 128;

#[derive(Clone, Debug)]
pub(super) struct Elf {
    pub(super) kind: u16,
    pub(super) interpreter: Option<String>,
    pub(super) needed: Vec<String>,
    pub(super) soname: Option<String>,
    pub(super) runpath: Option<String>,
}

#[allow(clippy::too_many_lines)]
#[cfg(test)]
pub(super) fn inspect(bytes: &[u8]) -> Result<Elf, RuntimeError> {
    inspect_ranges(bytes.len(), |start, length| {
        Ok(checked(bytes, start, length)?.to_vec())
    })
}

#[allow(clippy::too_many_lines)]
pub(super) fn inspect_ranges(
    file_size: usize,
    mut read: impl FnMut(usize, usize) -> Result<Vec<u8>, RuntimeError>,
) -> Result<Elf, RuntimeError> {
    let header = read(0, 64)?;
    let bytes = header.as_slice();
    if bytes.get(..16).is_none_or(|ident| {
        ident[..4] != *b"\x7fELF" || ident[4] != 2 || ident[5] != 1 || ident[6] != 1
    }) || u16_at(bytes, 18)? != 62
        || u32_at(bytes, 20)? != 1
        || u16_at(bytes, 52)? != 64
    {
        return Err(invalid(
            "unsupported ELF format (requires Linux x86_64 ELF64 little-endian)",
        ));
    }
    let kind = u16_at(bytes, 16)?;
    let offset = index(u64_at(bytes, 32)?)?;
    let size = usize::from(u16_at(bytes, 54)?);
    let count = usize::from(u16_at(bytes, 56)?);
    if size != 56 || count == 0 || count > MAX_HEADERS {
        return Err(invalid("invalid ELF program header table"));
    }
    let headers = read(
        offset,
        size.checked_mul(count)
            .ok_or_else(|| invalid("ELF header overflow"))?,
    )?;
    let mut loads = Vec::new();
    let mut dynamic = None;
    let mut interpreter = None;
    for n in 0..count {
        let base = n * size;
        let typ = u32_at(&headers, base)?;
        let file_offset = index(u64_at(&headers, base + 8)?)?;
        let vaddr = u64_at(&headers, base + 16)?;
        let segment_size = index(u64_at(&headers, base + 32)?)?;
        let mem_size = u64_at(&headers, base + 40)?;
        if segment_size > 0 {
            bounded_range(file_size, file_offset, segment_size)?;
        }
        if typ == 1 {
            if segment_size as u64 > mem_size {
                return Err(invalid("ELF segment exceeds memory size"));
            }
            loads.push((vaddr, file_offset, segment_size));
        } else if typ == 2 {
            if dynamic.replace((file_offset, segment_size)).is_some() {
                return Err(invalid("multiple ELF dynamic segments"));
            }
        } else if typ == 3 {
            if interpreter.is_some() || !(2..=256).contains(&segment_size) {
                return Err(invalid("invalid ELF interpreter segment"));
            }
            let raw = read(file_offset, segment_size)?;
            if raw.last() != Some(&0) || raw[..raw.len() - 1].contains(&0) {
                return Err(invalid("invalid ELF interpreter string"));
            }
            let path = std::str::from_utf8(&raw[..raw.len() - 1])
                .map_err(|_| invalid("non-UTF-8 ELF interpreter"))?;
            // An ELF used as a library may contain PT_INTERP for direct
            // invocation (glibc's libc.so.6 does). Only the application entry
            // is executed; its interpreter is compared with the exact declared
            // loader output by verify_runtime.
            if !path.starts_with('/') || path.bytes().any(|b| !b.is_ascii_graphic()) {
                return Err(invalid("invalid ELF interpreter pathname"));
            }
            interpreter = Some(path.to_owned());
        }
    }
    let mut needed = Vec::new();
    let mut soname = None;
    let mut runpath = None;
    if let Some((start, length)) = dynamic {
        if length == 0 || length % 16 != 0 || length / 16 > 4096 {
            return Err(invalid("invalid ELF dynamic table"));
        }
        let mut strings = None;
        let mut string_size = None;
        let mut needed_offsets = Vec::new();
        let mut soname_offset = None;
        let mut runpath_offset = None;
        let mut terminated = false;
        let dynamic_bytes = read(start, length)?;
        for pos in (0..length).step_by(16) {
            let tag = u64_at(&dynamic_bytes, pos)?;
            let value = u64_at(&dynamic_bytes, pos + 8)?;
            match tag {
                0 => {
                    terminated = true;
                    break;
                }
                1 => {
                    if needed_offsets.len() == MAX_NEEDED {
                        return Err(invalid("too many ELF dependencies"));
                    }
                    needed_offsets.push(index(value)?);
                }
                5 => {
                    if strings.replace(value).is_some() {
                        return Err(invalid("duplicate ELF string table"));
                    }
                }
                10 => {
                    if string_size.replace(index(value)?).is_some() {
                        return Err(invalid("duplicate ELF string table size"));
                    }
                }
                14 => {
                    if soname_offset.replace(index(value)?).is_some() {
                        return Err(invalid("duplicate ELF SONAME"));
                    }
                }
                15 => return Err(invalid("legacy ELF RPATH is not supported")),
                29 if runpath_offset.replace(index(value)?).is_some() => {
                    return Err(invalid("duplicate ELF RUNPATH"));
                }
                _ => {}
            }
        }
        if !terminated {
            return Err(invalid("unterminated ELF dynamic table"));
        }
        if !needed_offsets.is_empty() || soname_offset.is_some() || runpath_offset.is_some() {
            let addr = strings.ok_or_else(|| invalid("missing ELF string table"))?;
            let length = string_size.ok_or_else(|| invalid("missing ELF string table size"))?;
            if length == 0 || length > file_size {
                return Err(invalid("invalid ELF string table size"));
            }
            let (virtual_start, file_start, file_length) = loads
                .iter()
                .copied()
                .find(|(vaddr, _, size)| {
                    addr >= *vaddr
                        && addr
                            .checked_add(length as u64)
                            .is_some_and(|end| end <= vaddr.saturating_add(*size as u64))
                })
                .ok_or_else(|| invalid("ELF string table is not file-backed"))?;
            let delta = index(addr - virtual_start)?;
            if delta
                .checked_add(length)
                .is_none_or(|end| end > file_length)
            {
                return Err(invalid("ELF string table crosses segment"));
            }
            let table_start = file_start
                .checked_add(delta)
                .ok_or_else(|| invalid("ELF offset overflow"))?;
            bounded_range(file_size, table_start, length)?;
            for offset in needed_offsets {
                needed.push(elf_name_from_range(table_start, length, offset, &mut read)?);
            }
            if let Some(offset) = soname_offset {
                soname = Some(elf_name_from_range(table_start, length, offset, &mut read)?);
            }
            if let Some(offset) = runpath_offset {
                let tail = read_string(table_start, length, offset, 4097, &mut read)?;
                let end = tail
                    .iter()
                    .take(4097)
                    .position(|&b| b == 0)
                    .ok_or_else(|| invalid("unterminated ELF RUNPATH"))?;
                let path = std::str::from_utf8(&tail[..end])
                    .map_err(|_| invalid("non-UTF-8 ELF RUNPATH"))?;
                if path.len() > 4096
                    || path.split(':').any(|part| {
                        !part.starts_with("/syrox/store/")
                            || part.bytes().any(|b| !b.is_ascii_graphic())
                    })
                {
                    return Err(invalid("ELF RUNPATH escapes the Syrox runtime ABI"));
                }
                runpath = Some(path.to_owned());
            }
        }
    }
    Ok(Elf {
        kind,
        interpreter,
        needed,
        soname,
        runpath,
    })
}

fn elf_name(table: &[u8], start: usize) -> Result<String, RuntimeError> {
    let tail = table
        .get(start..)
        .ok_or_else(|| invalid("ELF name outside string table"))?;
    let end = tail
        .iter()
        .take(256)
        .position(|&b| b == 0)
        .ok_or_else(|| invalid("unterminated ELF name"))?;
    let name = std::str::from_utf8(&tail[..end]).map_err(|_| invalid("non-UTF-8 ELF name"))?;
    if name.is_empty()
        || name.len() > 255
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        || name == "."
        || name == ".."
    {
        return Err(invalid("unsafe ELF library name"));
    }
    Ok(name.to_owned())
}

fn bounded_range(total: usize, start: usize, length: usize) -> Result<(), RuntimeError> {
    if start.checked_add(length).is_none_or(|end| end > total) {
        return Err(invalid("truncated ELF"));
    }
    Ok(())
}

fn read_string(
    table_start: usize,
    table_length: usize,
    offset: usize,
    maximum: usize,
    read: &mut impl FnMut(usize, usize) -> Result<Vec<u8>, RuntimeError>,
) -> Result<Vec<u8>, RuntimeError> {
    let remaining = table_length
        .checked_sub(offset)
        .filter(|length| *length > 0)
        .ok_or_else(|| invalid("ELF name outside string table"))?;
    read(
        table_start
            .checked_add(offset)
            .ok_or_else(|| invalid("ELF offset overflow"))?,
        remaining.min(maximum),
    )
}

fn elf_name_from_range(
    table_start: usize,
    table_length: usize,
    offset: usize,
    read: &mut impl FnMut(usize, usize) -> Result<Vec<u8>, RuntimeError>,
) -> Result<String, RuntimeError> {
    let text = read_string(table_start, table_length, offset, 256, read)?;
    elf_name(&text, 0)
}

fn index(value: u64) -> Result<usize, RuntimeError> {
    usize::try_from(value).map_err(|_| invalid("ELF offset exceeds address space"))
}
pub(super) fn checked(bytes: &[u8], start: usize, length: usize) -> Result<&[u8], RuntimeError> {
    bytes
        .get(
            start
                ..start
                    .checked_add(length)
                    .ok_or_else(|| invalid("ELF offset overflow"))?,
        )
        .ok_or_else(|| invalid("truncated ELF"))
}
fn u16_at(bytes: &[u8], at: usize) -> Result<u16, RuntimeError> {
    Ok(u16::from_le_bytes(
        checked(bytes, at, 2)?.try_into().unwrap(),
    ))
}
fn u32_at(bytes: &[u8], at: usize) -> Result<u32, RuntimeError> {
    Ok(u32::from_le_bytes(
        checked(bytes, at, 4)?.try_into().unwrap(),
    ))
}
fn u64_at(bytes: &[u8], at: usize) -> Result<u64, RuntimeError> {
    Ok(u64::from_le_bytes(
        checked(bytes, at, 8)?.try_into().unwrap(),
    ))
}
