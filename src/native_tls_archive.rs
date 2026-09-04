use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

const MAX_MEMBERS: usize = 16_384;
const MAX_NAME: usize = 1_024;
const MAX_NAME_TABLE: usize = 8 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_BUILD_ID_SCAN_BYTES: u64 = MAX_ARCHIVE_BYTES;
/* Rust's Windows std archive currently contains CGUs with more than 4096
COMDAT sections; keep a hard bound without rejecting the real staticlib. */
const MAX_SECTIONS: usize = 16_384;
/* Apply aggregate bounds as well as per-object bounds. A small archive can
contain many objects whose section tables or overlapping one-byte scan ranges
would otherwise amplify validation into millions of seeks. Each supported
target pack must remain below these limits in its platform CI gate. */
const MAX_AGGREGATE_SECTIONS: usize = 262_144;
const MAX_BUILD_ID_SCAN_RANGES: usize = 65_536;
const MAX_SYMBOLS: usize = 262_144;
const MAX_SYMBOL_TABLE_BYTES: usize = MAX_SYMBOLS * 24;
const MAX_STRING_TABLE: usize = 16 * 1024 * 1024;
const MAX_LOAD_COMMANDS: usize = 4_096;
const MAX_LOAD_BYTES: usize = 1024 * 1024;

const REQUIRED: [&[u8]; 17] = [
    b"ku_tls_abi_version",
    b"ku_tls_v1_build_id",
    b"ku_tls_v1_config_new",
    b"ku_tls_v1_config_drop",
    b"ku_tls_v1_client_new",
    b"ku_tls_v1_client_drop",
    b"ku_tls_v1_client_wants_read",
    b"ku_tls_v1_client_wants_write",
    b"ku_tls_v1_client_is_handshaking",
    b"ku_tls_v1_client_peer_closed",
    b"ku_tls_v1_client_feed_ciphertext",
    b"ku_tls_v1_client_process",
    b"ku_tls_v1_client_drain_ciphertext",
    b"ku_tls_v1_client_write_plaintext",
    b"ku_tls_v1_client_read_plaintext",
    b"ku_tls_v1_client_send_close_notify",
    b"ku_tls_v1_client_notify_eof",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeTlsArchiveFormat {
    CoffX86_64,
    ElfX86_64,
    MachOArm64,
}

struct Member {
    name: Vec<u8>,
    offset: u64,
    len: u64,
}
struct Evidence {
    symbols: BTreeSet<Vec<u8>>,
    symbol_count: usize,
    build_id: bool,
    build_id_scan_bytes: u64,
    build_id_scan_limit: u64,
    build_id_scan_ranges: usize,
    build_id_prefix: Vec<usize>,
    section_count: usize,
    rcgu: bool,
    objects: usize,
}

fn add(a: u64, b: u64, what: &str) -> Result<u64, String> {
    a.checked_add(b).ok_or_else(|| format!("{what} overflows"))
}
fn range(offset: u64, len: u64, end: u64, what: &str) -> Result<(), String> {
    if add(offset, len, what)? > end {
        Err(format!("{what} is outside its bound"))
    } else {
        Ok(())
    }
}
fn member_end(member: &Member, what: &str) -> Result<u64, String> {
    add(member.offset, member.len, what)
}
fn member_range(member: &Member, relative: u64, len: u64, what: &str) -> Result<u64, String> {
    let offset = add(member.offset, relative, what)?;
    range(offset, len, member_end(member, what)?, what)?;
    Ok(offset)
}
fn mul(a: u64, b: u64, what: &str) -> Result<u64, String> {
    a.checked_mul(b).ok_or_else(|| format!("{what} overflows"))
}
fn read_at(file: &mut File, offset: u64, out: &mut [u8], what: &str) -> Result<(), String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek {what}: {e}"))?;
    file.read_exact(out)
        .map_err(|e| format!("read {what}: {e}"))
}
fn u16le(x: &[u8]) -> u16 {
    u16::from_le_bytes([x[0], x[1]])
}
fn u32le(x: &[u8]) -> u32 {
    u32::from_le_bytes([x[0], x[1], x[2], x[3]])
}
fn u64le(x: &[u8]) -> u64 {
    u64::from_le_bytes(x[..8].try_into().unwrap())
}
fn bounded_vec(
    file: &mut File,
    off: u64,
    len: u64,
    cap: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    let n = usize::try_from(len).map_err(|_| format!("{what} does not fit host"))?;
    if n > cap {
        return Err(format!("{what} exceeds limit"));
    }
    let mut v = Vec::new();
    v.try_reserve_exact(n)
        .map_err(|_| format!("cannot reserve {what}"))?;
    v.resize(n, 0);
    read_at(file, off, &mut v, what)?;
    Ok(v)
}
fn decimal(bytes: &[u8], what: &str) -> Result<u64, String> {
    if !bytes.is_ascii() {
        return Err(format!("{what} is not ASCII"));
    }
    let end = bytes.iter().rposition(|b| *b != b' ').map_or(0, |i| i + 1);
    let s = std::str::from_utf8(&bytes[..end]).map_err(|_| format!("{what} is not ASCII"))?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{what} is not canonical decimal"));
    }
    s.parse().map_err(|_| format!("{what} overflows"))
}
fn safe_name(name: &[u8]) -> Result<Vec<u8>, String> {
    let name = name.strip_suffix(b"/").unwrap_or(name);
    /* Rust staticlibs may retain source paths (notably ring's pregenerated
    assembly). Members are never extracted; retain only the basename. */
    let name = name
        .rsplit(|b| *b == b'/' || *b == b'\\')
        .next()
        .unwrap_or(name);
    if name.is_empty()
        || name.len() > MAX_NAME
        || name.contains(&0)
        || name == b"."
        || name == b".."
    {
        return Err(format!(
            "archive member name is unsafe: {:?}",
            String::from_utf8_lossy(name)
        ));
    }
    Ok(name.to_vec())
}

fn members(file: &mut File, file_len: u64) -> Result<Vec<Member>, String> {
    if file_len < 8 {
        return Err("archive is truncated".into());
    }
    let mut magic = [0; 8];
    read_at(file, 0, &mut magic, "archive magic")?;
    if &magic == b"!<thin>\n" {
        return Err("thin archive is forbidden".into());
    }
    if &magic != b"!<arch>\n" {
        return Err("not a complete ar archive".into());
    }
    let mut cursor = 8u64;
    let mut out = Vec::new();
    let mut names = Vec::new();
    let mut member_count = 0usize;
    while cursor < file_len {
        if member_count >= MAX_MEMBERS {
            return Err("archive member limit exceeded".into());
        }
        member_count += 1;
        range(cursor, 60, file_len, "archive member header")?;
        let mut h = [0; 60];
        read_at(file, cursor, &mut h, "archive member header")?;
        if &h[58..] != b"`\n" {
            return Err("archive member header terminator is invalid".into());
        }
        let size = decimal(&h[48..58], "archive member size")?;
        let mut data = add(cursor, 60, "archive member data")?;
        range(data, size, file_len, "archive member")?;
        let raw = h[..16]
            .iter()
            .copied()
            .take_while(|b| *b != b' ')
            .collect::<Vec<_>>();
        let (name, payload_len) = if raw == b"//" {
            names = bounded_vec(file, data, size, MAX_NAME_TABLE, "GNU archive name table")?;
            (None, size)
        } else if raw == b"/" || raw == b"/SYM64/" || raw.starts_with(b"__.SYMDEF") {
            (None, size)
        } else if let Some(rest) = raw.strip_prefix(b"#1/") {
            let n = decimal(rest, "BSD member name length")?;
            if n > size || n > MAX_NAME as u64 {
                return Err("BSD member name is out of bounds".into());
            }
            let v = bounded_vec(file, data, n, MAX_NAME, "BSD member name")?;
            data = add(data, n, "BSD payload")?;
            (Some(safe_name(&v)?), size - n)
        } else if raw.starts_with(b"/") && raw.len() > 1 {
            let off = usize::try_from(decimal(&raw[1..], "GNU member name offset")?)
                .map_err(|_| "name offset does not fit host")?;
            let tail = names
                .get(off..)
                .ok_or("GNU member name offset is outside table")?;
            let slash_end = tail.windows(2).position(|w| w == b"/\n");
            let nul_end = tail.iter().position(|b| *b == 0);
            let end = match (slash_end, nul_end) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) | (None, Some(a)) => a,
                (None, None) => return Err("GNU member name is unterminated".into()),
            };
            (Some(safe_name(&tail[..end])?), size)
        } else {
            (Some(safe_name(&raw)?), size)
        };
        if let Some(name) = name {
            out.push(Member {
                name,
                offset: data,
                len: payload_len,
            });
        }
        cursor = add(add(cursor, 60, "archive cursor")?, size, "archive cursor")?;
        if cursor & 1 != 0 {
            range(cursor, 1, file_len, "archive padding")?;
            let mut padding = [0u8; 1];
            read_at(file, cursor, &mut padding, "archive padding")?;
            if padding[0] != b'\n' {
                return Err("archive padding byte is not canonical".into());
            }
            cursor += 1;
        }
    }
    if cursor != file_len {
        return Err("archive is not consumed exactly".into());
    }
    if out.is_empty() {
        return Err("archive has no object members".into());
    }
    Ok(out)
}

fn cstr<'a>(table: &'a [u8], off: usize, what: &str) -> Result<&'a [u8], String> {
    let tail = table
        .get(off..)
        .ok_or_else(|| format!("{what} offset is outside string table"))?;
    let n = tail
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| format!("{what} is unterminated"))?;
    if n > MAX_NAME {
        return Err(format!("{what} exceeds name limit"));
    }
    Ok(&tail[..n])
}
fn record_symbol(
    ev: &mut Evidence,
    name: &[u8],
    defined: bool,
    external: bool,
) -> Result<(), String> {
    ev.symbol_count = ev
        .symbol_count
        .checked_add(1)
        .ok_or("aggregate symbol count overflows")?;
    if ev.symbol_count > MAX_SYMBOLS {
        return Err("aggregate symbol limit exceeded".into());
    }
    if defined && external && REQUIRED.contains(&name) && !ev.symbols.insert(name.to_vec()) {
        return Err(format!(
            "duplicate TLS ABI symbol {}",
            String::from_utf8_lossy(name)
        ));
    }
    Ok(())
}

fn charge_sections(ev: &mut Evidence, count: usize) -> Result<(), String> {
    ev.section_count = ev
        .section_count
        .checked_add(count)
        .ok_or("aggregate section count overflows")?;
    if ev.section_count > MAX_AGGREGATE_SECTIONS {
        return Err("aggregate section limit exceeded".into());
    }
    Ok(())
}

fn kmp_prefix(needle: &[u8]) -> Result<Vec<usize>, String> {
    let mut prefix = Vec::new();
    prefix
        .try_reserve_exact(needle.len())
        .map_err(|_| "cannot reserve build-id matcher state".to_string())?;
    prefix.resize(needle.len(), 0);
    let mut matched = 0usize;
    for index in 1..needle.len() {
        while matched > 0 && needle[index] != needle[matched] {
            matched = prefix[matched - 1];
        }
        if needle[index] == needle[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }
    Ok(prefix)
}

fn scan_with_prefix(
    file: &mut File,
    off: u64,
    len: u64,
    needle: &[u8],
    prefix: &[usize],
) -> Result<bool, String> {
    if needle.is_empty() {
        return Err("expected build id is empty".into());
    }
    if prefix.len() != needle.len() {
        return Err("build-id matcher state is invalid".into());
    }
    let mut cursor = 0;
    let mut matched = 0usize;
    let mut buf = [0u8; 64 * 1024];
    while cursor < len {
        let n = (len - cursor).min(buf.len() as u64) as usize;
        read_at(
            file,
            add(off, cursor, "build-id scan offset")?,
            &mut buf[..n],
            "object data",
        )?;
        for &byte in &buf[..n] {
            while matched > 0 && byte != needle[matched] {
                matched = prefix[matched - 1];
            }
            if byte == needle[matched] {
                matched += 1;
                if matched == needle.len() {
                    return Ok(true);
                }
            }
        }
        cursor = add(cursor, n as u64, "build-id scan cursor")?;
    }
    Ok(false)
}

#[cfg(test)]
fn scan(file: &mut File, off: u64, len: u64, needle: &[u8]) -> Result<bool, String> {
    let prefix = kmp_prefix(needle)?;
    scan_with_prefix(file, off, len, needle, &prefix)
}

fn scan_build_id(
    file: &mut File,
    offset: u64,
    len: u64,
    id: &[u8],
    ev: &mut Evidence,
) -> Result<(), String> {
    ev.build_id_scan_ranges = ev
        .build_id_scan_ranges
        .checked_add(1)
        .ok_or("aggregate build-id scan range count overflows")?;
    if ev.build_id_scan_ranges > MAX_BUILD_ID_SCAN_RANGES {
        return Err("aggregate build-id scan range limit exceeded".into());
    }
    let next = ev
        .build_id_scan_bytes
        .checked_add(len)
        .ok_or("aggregate build-id section scan size overflows")?;
    if next > ev.build_id_scan_limit {
        return Err(format!(
            "aggregate build-id section scan exceeds {} bytes",
            ev.build_id_scan_limit
        ));
    }
    /* Charge the complete section before the first read. Repeated or
    overlapping ranges are intentionally charged again. */
    ev.build_id_scan_bytes = next;
    if !ev.build_id {
        ev.build_id = scan_with_prefix(file, offset, len, id, &ev.build_id_prefix)?;
    }
    Ok(())
}

fn coff(file: &mut File, m: &Member, id: &[u8], ev: &mut Evidence) -> Result<(), String> {
    let mut h = [0; 20];
    let header = member_range(m, 0, 20, "COFF header")?;
    read_at(file, header, &mut h, "COFF header")?;
    let bigobj = h[..4] == [0, 0, 0xff, 0xff];
    let (header_size, ns, sym_rel, count, symbol_size) = if bigobj {
        if m.len < 56 {
            if u16le(&h[6..]) != 0x8664 {
                return Err("wrong-target COFF import member".into());
            }
            return Ok(());
        }
        let mut big = [0u8; 56];
        let big_header = member_range(m, 0, 56, "COFF bigobj header")?;
        read_at(file, big_header, &mut big, "COFF bigobj header")?;
        const CLASS: [u8; 16] = [
            0xc7, 0xa1, 0xba, 0xd1, 0xee, 0xba, 0xa9, 0x4b, 0xaf, 0x20, 0xfa, 0xf6, 0x6a, 0xa4,
            0xdc, 0xb8,
        ];
        if u16le(&big[4..]) < 2 || u16le(&big[6..]) != 0x8664 || big[12..28] != CLASS {
            if u16le(&big[6..]) != 0x8664 {
                return Err("wrong-target COFF import member".into());
            }
            return Ok(());
        }
        (
            56usize,
            u32le(&big[44..]) as usize,
            u32le(&big[48..]),
            u32le(&big[52..]) as usize,
            20usize,
        )
    } else {
        if u16le(&h) != 0x8664 || u16le(&h[16..]) != 0 {
            return Err("expected AMD64 COFF relocatable object".into());
        }
        (
            20usize,
            u16le(&h[2..]) as usize,
            u32le(&h[8..]),
            u32le(&h[12..]) as usize,
            18usize,
        )
    };
    if (!bigobj && ns == 0) || ns > MAX_SECTIONS {
        return Err(format!(
            "COFF section count {ns} invalid (bigobj={bigobj}) in {}",
            String::from_utf8_lossy(&m.name)
        ));
    }
    charge_sections(ev, ns)?;
    let section_bytes = mul(ns as u64, 40, "COFF section table size")?;
    member_range(m, header_size as u64, section_bytes, "COFF sections")?;
    if count > MAX_SYMBOLS {
        return Err("COFF symbol limit exceeded".into());
    }
    let symbol_bytes = mul(count as u64, symbol_size as u64, "COFF symbol table size")?;
    let symbol_data = member_range(m, sym_rel as u64, symbol_bytes, "COFF symbols")?;
    let symbol_table = bounded_vec(
        file,
        symbol_data,
        symbol_bytes,
        MAX_SYMBOL_TABLE_BYTES,
        "COFF symbols",
    )?;
    let str_rel = add(sym_rel as u64, symbol_bytes, "COFF string table offset")?;
    let str_off = member_range(m, str_rel, 4, "COFF string table")?;
    let mut sl = [0; 4];
    read_at(file, str_off, &mut sl, "COFF string length")?;
    let str_len = u32le(&sl) as usize;
    if !(4..=MAX_STRING_TABLE).contains(&str_len) {
        return Err("COFF string table size invalid".into());
    }
    member_range(m, str_rel, str_len as u64, "COFF string table")?;
    let strings = bounded_vec(
        file,
        str_off,
        str_len as u64,
        MAX_STRING_TABLE,
        "COFF strings",
    )?;
    let mut i = 0;
    while i < count {
        let start = i
            .checked_mul(symbol_size)
            .ok_or("COFF symbol offset overflows")?;
        let end = start
            .checked_add(symbol_size)
            .ok_or("COFF symbol offset overflows")?;
        let s = symbol_table
            .get(start..end)
            .ok_or("COFF symbol is outside its validated table")?;
        let name = if s[..4] == [0; 4] {
            cstr(&strings, u32le(&s[4..8]) as usize, "COFF symbol")?
        } else {
            let n = s[..8].iter().position(|b| *b == 0).unwrap_or(8);
            &s[..n]
        };
        let (section, class, aux) = if bigobj {
            (u32le(&s[12..]) as i32, s[18], s[19] as usize)
        } else {
            ((u16le(&s[12..]) as i16) as i32, s[16], s[17] as usize)
        };
        let defined = section > 0 && usize::try_from(section).is_ok_and(|value| value <= ns);
        record_symbol(ev, name, defined, class == 2)?;
        if i + aux >= count {
            return Err("COFF auxiliary symbol overflow".into());
        }
        i += 1 + aux;
    }
    for i in 0..ns {
        let mut s = [0; 40];
        let section_rel = add(
            header_size as u64,
            mul(i as u64, 40, "COFF section offset")?,
            "COFF section offset",
        )?;
        let section = member_range(m, section_rel, 40, "COFF section")?;
        read_at(file, section, &mut s, "COFF section")?;
        let n = u32le(&s[16..]) as u64;
        let relative = u32le(&s[20..]) as u64;
        let characteristics = u32le(&s[36..]);
        if n > 0 && characteristics & 0x40 != 0 {
            let data = member_range(m, relative, n, "COFF section data")?;
            scan_build_id(file, data, n, id, ev)?;
        }
    }
    ev.objects += 1;
    Ok(())
}

fn elf(file: &mut File, m: &Member, id: &[u8], ev: &mut Evidence) -> Result<(), String> {
    let mut h = [0; 64];
    let header = member_range(m, 0, 64, "ELF header")?;
    read_at(file, header, &mut h, "ELF header")?;
    if &h[..6] != b"\x7fELF\x02\x01" || u16le(&h[16..]) != 1 || u16le(&h[18..]) != 62 {
        return Err("expected x86_64 ELF64 relocatable object".into());
    }
    let shoff = u64le(&h[40..]);
    let ents = u16le(&h[58..]) as usize;
    let n = u16le(&h[60..]) as usize;
    if ents != 64 || n == 0 || n > MAX_SECTIONS {
        return Err("ELF section table is invalid".into());
    }
    charge_sections(ev, n)?;
    let section_bytes = mul(n as u64, 64, "ELF section table size")?;
    member_range(m, shoff, section_bytes, "ELF sections")?;
    let mut sections = Vec::with_capacity(n);
    for i in 0..n {
        let mut s = [0; 64];
        let section_rel = add(
            shoff,
            mul(i as u64, 64, "ELF section offset")?,
            "ELF section offset",
        )?;
        let section = member_range(m, section_rel, 64, "ELF section")?;
        read_at(file, section, &mut s, "ELF section")?;
        sections.push(s);
    }
    if sections
        .iter()
        .filter(|section| u32le(&section[4..]) == 2)
        .take(2)
        .count()
        > 1
    {
        return Err("ELF object contains multiple symbol tables".into());
    }
    for s in &sections {
        let ty = u32le(&s[4..]);
        let flags = u64le(&s[8..]);
        let off = u64le(&s[24..]);
        let len = u64le(&s[32..]);
        if ty != 8 && len > 0 {
            member_range(m, off, len, "ELF section data")?;
        }
        if ty == 1 && (flags & 2) != 0 && len > 0 {
            let data = member_range(m, off, len, "ELF data")?;
            scan_build_id(file, data, len, id, ev)?;
        }
        if ty == 2 {
            let link = u32le(&s[40..]) as usize;
            let strsec = sections.get(link).ok_or("ELF symtab string link invalid")?;
            if u32le(&strsec[4..]) != 3 {
                return Err("ELF symtab does not link a string table".into());
            }
            let so = u64le(&strsec[24..]);
            let sn = u64le(&strsec[32..]);
            let string_data = member_range(m, so, sn, "ELF strings")?;
            let strings = bounded_vec(file, string_data, sn, MAX_STRING_TABLE, "ELF strings")?;
            let off = u64le(&s[24..]);
            let len = u64le(&s[32..]);
            let ent = u64le(&s[56..]);
            if ent != 24 || len.checked_rem(24) != Some(0) || len / 24 > MAX_SYMBOLS as u64 {
                return Err("ELF symbol table invalid".into());
            }
            let symbol_data = member_range(m, off, len, "ELF symbols")?;
            let symbol_table = bounded_vec(
                file,
                symbol_data,
                len,
                MAX_SYMBOL_TABLE_BYTES,
                "ELF symbols",
            )?;
            for x in symbol_table.chunks_exact(24) {
                let name = cstr(&strings, u32le(x) as usize, "ELF symbol")?;
                let section = u16le(&x[6..]) as usize;
                record_symbol(
                    ev,
                    name,
                    section > 0 && section < n,
                    x[4] >> 4 == 1 || x[4] >> 4 == 2,
                )?;
            }
        }
    }
    ev.objects += 1;
    Ok(())
}

fn macho(file: &mut File, m: &Member, id: &[u8], ev: &mut Evidence) -> Result<(), String> {
    let mut h = [0; 32];
    let header = member_range(m, 0, 32, "Mach-O header")?;
    read_at(file, header, &mut h, "Mach-O header")?;
    if &h[..4] != b"\xcf\xfa\xed\xfe" || u32le(&h[4..]) != 0x0100000c || u32le(&h[12..]) != 1 {
        return Err("expected arm64 Mach-O relocatable object".into());
    }
    let n = u32le(&h[16..]) as usize;
    let bytes = u32le(&h[20..]) as usize;
    if n > MAX_LOAD_COMMANDS || bytes > MAX_LOAD_BYTES {
        return Err("Mach-O load commands exceed limit".into());
    }
    member_range(m, 32, bytes as u64, "Mach-O commands")?;
    let mut cur = 32u64;
    let end = add(cur, bytes as u64, "Mach-O command end")?;
    let mut symtab = None;
    let mut section_count = 0usize;
    for _ in 0..n {
        let mut c = [0; 8];
        let command = member_range(m, cur, 8, "Mach-O command")?;
        read_at(file, command, &mut c, "Mach-O command")?;
        let kind = u32le(&c);
        let size = u32le(&c[4..]) as u64;
        let next = add(cur, size, "Mach-O command")?;
        if size < 8 || next > end {
            return Err("Mach-O command is invalid".into());
        }
        if kind == 2 {
            let mut s = [0; 24];
            if size < 24 {
                return Err("Mach-O symtab command truncated".into());
            }
            let symtab_command = member_range(m, cur, 24, "Mach-O symtab")?;
            read_at(file, symtab_command, &mut s, "Mach-O symtab")?;
            if symtab
                .replace((
                    u32le(&s[8..]),
                    u32le(&s[12..]),
                    u32le(&s[16..]),
                    u32le(&s[20..]),
                ))
                .is_some()
            {
                return Err("multiple Mach-O symtabs".into());
            }
        }
        if kind == 0x19 {
            let mut seg = [0; 72];
            if size < 72 {
                return Err("Mach-O segment truncated".into());
            }
            let segment = member_range(m, cur, 72, "Mach-O segment")?;
            read_at(file, segment, &mut seg, "Mach-O segment")?;
            let count = u32le(&seg[64..]) as usize;
            section_count = section_count
                .checked_add(count)
                .ok_or("Mach-O section count overflow")?;
            charge_sections(ev, count)?;
            let command_section_bytes = mul(count as u64, 80, "Mach-O section table size")?;
            let command_size = add(72, command_section_bytes, "Mach-O segment size")?;
            if section_count > MAX_SECTIONS || command_size > size {
                return Err("Mach-O sections invalid".into());
            }
            for i in 0..count {
                let mut s = [0; 80];
                let section_rel = add(
                    add(cur, 72, "Mach-O section offset")?,
                    mul(i as u64, 80, "Mach-O section offset")?,
                    "Mach-O section offset",
                )?;
                let section = member_range(m, section_rel, 80, "Mach-O section")?;
                read_at(file, section, &mut s, "Mach-O section")?;
                let len = u64le(&s[40..]);
                let off = u32le(&s[48..]) as u64;
                let section_type = u32le(&s[64..]) & 0xff;
                let zerofill = matches!(section_type, 1 | 0x0c | 0x12);
                if len > 0 && !zerofill {
                    let data = member_range(m, off, len, "Mach-O section data")?;
                    scan_build_id(file, data, len, id, ev)?;
                }
            }
        }
        cur = next;
    }
    if cur != end {
        return Err("Mach-O commands not consumed exactly".into());
    }
    let (so, count, stro, strn) = symtab.ok_or("Mach-O object has no symtab")?;
    if count as usize > MAX_SYMBOLS {
        return Err("Mach-O symbol limit exceeded".into());
    }
    let string_data = member_range(m, stro as u64, strn as u64, "Mach-O strings")?;
    let strings = bounded_vec(
        file,
        string_data,
        strn as u64,
        MAX_STRING_TABLE,
        "Mach-O strings",
    )?;
    let symbol_bytes = mul(count as u64, 16, "Mach-O symbol table size")?;
    let symbol_data = member_range(m, so as u64, symbol_bytes, "Mach-O symbols")?;
    let symbol_table = bounded_vec(
        file,
        symbol_data,
        symbol_bytes,
        MAX_SYMBOL_TABLE_BYTES,
        "Mach-O symbols",
    )?;
    for s in symbol_table.chunks_exact(16) {
        let mut name = cstr(&strings, u32le(s) as usize, "Mach-O symbol")?;
        if let Some(x) = name.strip_prefix(b"_") {
            name = x;
        }
        let typ = s[4];
        let section = s[5] as usize;
        let defined = (typ & 0x0e) == 0x0e && section != 0 && section <= section_count;
        record_symbol(ev, name, defined, (typ & 1) != 0)?;
    }
    ev.objects += 1;
    Ok(())
}

fn validate_with_scan_limit(
    file: &mut File,
    file_len: u64,
    format: NativeTlsArchiveFormat,
    expected_build_id: &[u8],
    build_id_scan_limit: u64,
) -> Result<(), String> {
    if expected_build_id.is_empty() || expected_build_id.len() > 1024 {
        return Err("expected build id is invalid".into());
    }
    if file_len > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "archive exceeds the {MAX_ARCHIVE_BYTES}-byte validation limit"
        ));
    }
    if build_id_scan_limit > MAX_BUILD_ID_SCAN_BYTES {
        return Err("build-id scan limit exceeds the validator maximum".into());
    }
    let members = members(file, file_len)?;
    let build_id_prefix = kmp_prefix(expected_build_id)?;
    let mut ev = Evidence {
        symbols: BTreeSet::new(),
        symbol_count: 0,
        build_id: false,
        build_id_scan_bytes: 0,
        build_id_scan_limit,
        build_id_scan_ranges: 0,
        build_id_prefix,
        section_count: 0,
        rcgu: false,
        objects: 0,
    };
    for m in members {
        let lower = m
            .name
            .iter()
            .map(u8::to_ascii_lowercase)
            .collect::<Vec<_>>();
        if lower == b"rust.metadata.bin" {
            continue;
        }
        ev.rcgu |= lower
            .windows(b"ku_native_tls".len())
            .any(|w| w == b"ku_native_tls")
            && (lower.ends_with(b".rcgu.o") || lower.ends_with(b".rcgu.obj"));
        match format {
            NativeTlsArchiveFormat::CoffX86_64 => coff(file, &m, expected_build_id, &mut ev)?,
            NativeTlsArchiveFormat::ElfX86_64 => elf(file, &m, expected_build_id, &mut ev)?,
            NativeTlsArchiveFormat::MachOArm64 => macho(file, &m, expected_build_id, &mut ev)?,
        }
    }
    if ev.objects == 0 || !ev.rcgu {
        return Err("archive has no ku_native_tls rcgu object".into());
    }
    if !ev.build_id {
        return Err("expected TLS build id is absent from object data".into());
    }
    for s in REQUIRED {
        if !ev.symbols.contains(s) {
            return Err(format!(
                "missing defined TLS ABI symbol {}",
                String::from_utf8_lossy(s)
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate(
    file: &mut File,
    file_len: u64,
    format: NativeTlsArchiveFormat,
    expected_build_id: &[u8],
) -> Result<(), String> {
    validate_with_scan_limit(
        file,
        file_len,
        format,
        expected_build_id,
        file_len.min(MAX_BUILD_ID_SCAN_BYTES),
    )
}

#[cfg(test)]
pub(crate) use tests::fixture_archive;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::time::{SystemTime, UNIX_EPOCH};
    fn ar(name: &str, data: &[u8]) -> Vec<u8> {
        let mut v = b"!<arch>\n".to_vec();
        let mut n = [b' '; 16];
        let mut payload = Vec::new();
        if name.len() <= 15 {
            n[..name.len()].copy_from_slice(name.as_bytes());
        } else {
            let encoded = format!("#1/{}", name.len());
            n[..encoded.len()].copy_from_slice(encoded.as_bytes());
            payload.extend_from_slice(name.as_bytes());
        }
        payload.extend_from_slice(data);
        v.extend(n);
        v.extend(format!("{:<12}{:<6}{:<6}{:<8}{:<10}`\n", 0, 0, 0, 0, payload.len()).as_bytes());
        v.extend(&payload);
        if payload.len() % 2 != 0 {
            v.push(b'\n');
        }
        v
    }
    fn temp(bytes: &[u8]) -> (std::path::PathBuf, File) {
        let p = std::env::temp_dir().join(format!(
            "ku-tls-ar-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, bytes).unwrap();
        let f = OpenOptions::new().read(true).open(&p).unwrap();
        (p, f)
    }
    fn names() -> (Vec<u8>, Vec<u32>) {
        let mut table = vec![0];
        let mut offsets = Vec::new();
        for name in REQUIRED {
            offsets.push(table.len() as u32);
            table.extend_from_slice(name);
            table.push(0);
        }
        (table, offsets)
    }
    fn coff_fixture(id: &[u8]) -> Vec<u8> {
        let (strings, offsets) = names();
        let raw = 60u32;
        let symbols = raw + id.len() as u32;
        let mut out = vec![0; symbols as usize + REQUIRED.len() * 18 + 4 + strings.len()];
        out[0..2].copy_from_slice(&0x8664u16.to_le_bytes());
        out[2..4].copy_from_slice(&1u16.to_le_bytes());
        out[8..12].copy_from_slice(&symbols.to_le_bytes());
        out[12..16].copy_from_slice(&(REQUIRED.len() as u32).to_le_bytes());
        out[20..26].copy_from_slice(b".rdata");
        out[36..40].copy_from_slice(&(id.len() as u32).to_le_bytes());
        out[40..44].copy_from_slice(&raw.to_le_bytes());
        out[56..60].copy_from_slice(&0x40u32.to_le_bytes());
        out[raw as usize..raw as usize + id.len()].copy_from_slice(id);
        for (i, offset) in offsets.iter().enumerate() {
            let at = symbols as usize + i * 18;
            out[at + 4..at + 8].copy_from_slice(&(*offset + 4).to_le_bytes());
            out[at + 12..at + 14].copy_from_slice(&1i16.to_le_bytes());
            out[at + 16] = 2;
        }
        let at = symbols as usize + REQUIRED.len() * 18;
        out[at..at + 4].copy_from_slice(&((strings.len() + 4) as u32).to_le_bytes());
        out[at + 4..].copy_from_slice(&strings);
        out
    }
    fn elf_fixture(id: &[u8]) -> Vec<u8> {
        let (strings, offsets) = names();
        let data = 64usize;
        let symbols = data + id.len();
        let symbol_len = (REQUIRED.len() + 1) * 24;
        let strtab = symbols + symbol_len;
        let sections = strtab + strings.len();
        let mut out = vec![0; sections + 4 * 64];
        out[..6].copy_from_slice(b"\x7fELF\x02\x01");
        out[16..18].copy_from_slice(&1u16.to_le_bytes());
        out[18..20].copy_from_slice(&62u16.to_le_bytes());
        out[40..48].copy_from_slice(&(sections as u64).to_le_bytes());
        out[58..60].copy_from_slice(&64u16.to_le_bytes());
        out[60..62].copy_from_slice(&4u16.to_le_bytes());
        out[data..data + id.len()].copy_from_slice(id);
        let sh = sections + 64;
        out[sh + 4..sh + 8].copy_from_slice(&1u32.to_le_bytes());
        out[sh + 8..sh + 16].copy_from_slice(&2u64.to_le_bytes());
        out[sh + 24..sh + 32].copy_from_slice(&(data as u64).to_le_bytes());
        out[sh + 32..sh + 40].copy_from_slice(&(id.len() as u64).to_le_bytes());
        let sh = sections + 128;
        out[sh + 4..sh + 8].copy_from_slice(&2u32.to_le_bytes());
        out[sh + 24..sh + 32].copy_from_slice(&(symbols as u64).to_le_bytes());
        out[sh + 32..sh + 40].copy_from_slice(&(symbol_len as u64).to_le_bytes());
        out[sh + 40..sh + 44].copy_from_slice(&3u32.to_le_bytes());
        out[sh + 56..sh + 64].copy_from_slice(&24u64.to_le_bytes());
        let sh = sections + 192;
        out[sh + 4..sh + 8].copy_from_slice(&3u32.to_le_bytes());
        out[sh + 24..sh + 32].copy_from_slice(&(strtab as u64).to_le_bytes());
        out[sh + 32..sh + 40].copy_from_slice(&(strings.len() as u64).to_le_bytes());
        out[strtab..strtab + strings.len()].copy_from_slice(&strings);
        for (i, offset) in offsets.iter().enumerate() {
            let at = symbols + (i + 1) * 24;
            out[at..at + 4].copy_from_slice(&offset.to_le_bytes());
            out[at + 4] = 0x10;
            out[at + 6..at + 8].copy_from_slice(&1u16.to_le_bytes());
        }
        out
    }
    fn macho_fixture(id: &[u8]) -> Vec<u8> {
        let (strings, offsets) = names();
        let data = 208usize;
        let symbols = data + id.len();
        let strtab = symbols + REQUIRED.len() * 16;
        let mut out = vec![0; strtab + strings.len()];
        out[..4].copy_from_slice(b"\xcf\xfa\xed\xfe");
        out[4..8].copy_from_slice(&0x0100000cu32.to_le_bytes());
        out[12..16].copy_from_slice(&1u32.to_le_bytes());
        out[16..20].copy_from_slice(&2u32.to_le_bytes());
        out[20..24].copy_from_slice(&176u32.to_le_bytes());
        out[32..36].copy_from_slice(&0x19u32.to_le_bytes());
        out[36..40].copy_from_slice(&152u32.to_le_bytes());
        out[96..100].copy_from_slice(&1u32.to_le_bytes());
        out[144..152].copy_from_slice(&(id.len() as u64).to_le_bytes());
        out[152..156].copy_from_slice(&(data as u32).to_le_bytes());
        out[184..188].copy_from_slice(&2u32.to_le_bytes());
        out[188..192].copy_from_slice(&24u32.to_le_bytes());
        out[192..196].copy_from_slice(&(symbols as u32).to_le_bytes());
        out[196..200].copy_from_slice(&(REQUIRED.len() as u32).to_le_bytes());
        out[200..204].copy_from_slice(&(strtab as u32).to_le_bytes());
        out[204..208].copy_from_slice(&(strings.len() as u32).to_le_bytes());
        out[data..data + id.len()].copy_from_slice(id);
        out[strtab..].copy_from_slice(&strings);
        for (i, offset) in offsets.iter().enumerate() {
            let at = symbols + i * 16;
            out[at..at + 4].copy_from_slice(&offset.to_le_bytes());
            out[at + 4] = 0x0f;
            out[at + 5] = 1;
        }
        out
    }

    fn append_elf_section(object: &mut Vec<u8>, source_index: usize) {
        let section_table = u64le(&object[40..48]) as usize;
        let section_count = u16le(&object[60..62]) as usize;
        assert!(source_index < section_count);
        assert_eq!(section_table + section_count * 64, object.len());
        let start = section_table + source_index * 64;
        let section = object[start..start + 64].to_vec();
        object.extend_from_slice(&section);
        object[60..62].copy_from_slice(&((section_count + 1) as u16).to_le_bytes());
    }

    fn validation_error(object: &[u8], format: NativeTlsArchiveFormat, id: &[u8]) -> String {
        let bytes = ar("ku_native_tls.fixture.rcgu.o", object);
        let (path, mut file) = temp(&bytes);
        let error = validate(&mut file, bytes.len() as u64, format, id).unwrap_err();
        drop(file);
        std::fs::remove_file(path).ok();
        error
    }

    #[test]
    fn streaming_build_id_matcher_is_linear_and_crosses_chunk_boundaries() {
        let needle = b"abababababababababababac";
        let mut bytes = vec![b'x'; 64 * 1024 - 7];
        bytes.extend_from_slice(needle);
        let (path, mut file) = temp(&bytes);
        assert!(scan(&mut file, 0, bytes.len() as u64, needle).unwrap());
        drop(file);
        std::fs::remove_file(path).ok();

        let mut adversarial_needle = vec![b'a'; 1024];
        *adversarial_needle.last_mut().unwrap() = b'b';
        let adversarial = vec![b'a'; 2 * 1024 * 1024];
        let (path, mut file) = temp(&adversarial);
        assert!(!scan(&mut file, 0, adversarial.len() as u64, &adversarial_needle,).unwrap());
        drop(file);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn aggregate_section_and_scan_range_limits_fail_before_io() {
        let id = b"fixture-build-id";
        let mut evidence = Evidence {
            symbols: BTreeSet::new(),
            symbol_count: 0,
            build_id: false,
            build_id_scan_bytes: 0,
            build_id_scan_limit: MAX_BUILD_ID_SCAN_BYTES,
            build_id_scan_ranges: MAX_BUILD_ID_SCAN_RANGES,
            build_id_prefix: kmp_prefix(id).unwrap(),
            section_count: MAX_AGGREGATE_SECTIONS,
            rcgu: false,
            objects: 0,
        };
        assert_eq!(
            charge_sections(&mut evidence, 1).unwrap_err(),
            "aggregate section limit exceeded"
        );

        let (path, mut file) = temp(&[]);
        assert_eq!(
            scan_build_id(&mut file, 0, 0, id, &mut evidence).unwrap_err(),
            "aggregate build-id scan range limit exceeded"
        );
        drop(file);
        std::fs::remove_file(path).ok();
    }

    pub(crate) fn fixture_archive(format: NativeTlsArchiveFormat, id: &[u8]) -> Vec<u8> {
        match format {
            NativeTlsArchiveFormat::CoffX86_64 => {
                ar("ku_native_tls.fixture.rcgu.o", &coff_fixture(id))
            }
            NativeTlsArchiveFormat::ElfX86_64 => {
                ar("ku_native_tls.fixture.rcgu.o", &elf_fixture(id))
            }
            NativeTlsArchiveFormat::MachOArm64 => {
                ar("ku_native_tls.fixture.rcgu.o", &macho_fixture(id))
            }
        }
    }
    #[test]
    fn validates_minimal_objects_for_all_formats() {
        let id = b"fixture-build-id";
        for (object, format, name) in [
            (
                coff_fixture(id),
                NativeTlsArchiveFormat::CoffX86_64,
                "ku_native_tls.fixture.rcgu.o",
            ),
            (
                elf_fixture(id),
                NativeTlsArchiveFormat::ElfX86_64,
                "ku_native_tls.fixture.rcgu.o",
            ),
            (
                macho_fixture(id),
                NativeTlsArchiveFormat::MachOArm64,
                "ku_native_tls.fixture.rcgu.o",
            ),
        ] {
            let bytes = ar(name, &object);
            let (path, mut file) = temp(&bytes);
            validate(&mut file, bytes.len() as u64, format, id).unwrap();
            drop(file);
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn rejects_required_symbols_outside_real_object_sections() {
        let id = b"fixture-build-id";

        let mut coff = coff_fixture(id);
        let coff_symbols = 60 + id.len();
        coff[coff_symbols + 12..coff_symbols + 14].copy_from_slice(&2i16.to_le_bytes());
        let error = validation_error(&coff, NativeTlsArchiveFormat::CoffX86_64, id);
        assert!(error.contains("missing defined TLS ABI symbol"), "{error}");

        let mut elf = elf_fixture(id);
        let elf_symbols = 64 + id.len();
        let first_required_symbol = elf_symbols + 24;
        elf[first_required_symbol + 6..first_required_symbol + 8]
            .copy_from_slice(&0xfff1u16.to_le_bytes());
        let error = validation_error(&elf, NativeTlsArchiveFormat::ElfX86_64, id);
        assert!(error.contains("missing defined TLS ABI symbol"), "{error}");
    }

    #[test]
    fn rejects_empty_import_and_wrong_target_archives() {
        for (bytes, fmt, msg) in [
            (
                b"!<arch>\n".to_vec(),
                NativeTlsArchiveFormat::CoffX86_64,
                "no object",
            ),
            (
                ar(
                    "x.o/",
                    &[
                        0, 0, 0xff, 0xff, 0, 0, 0x64, 0x86, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    ],
                ),
                NativeTlsArchiveFormat::CoffX86_64,
                "no ku_native_tls rcgu",
            ),
            (
                ar("x.o/", &[0; 64]),
                NativeTlsArchiveFormat::ElfX86_64,
                "ELF64",
            ),
        ] {
            let (p, mut f) = temp(&bytes);
            let e = validate(&mut f, bytes.len() as u64, fmt, b"id").unwrap_err();
            assert!(e.contains(msg), "{e}");
            drop(f);
            std::fs::remove_file(p).ok();
        }
    }
    #[test]
    fn rejects_thin_truncated_and_missing_evidence() {
        for bytes in [b"!<thin>\n".to_vec(), b"!<arch>\nshort".to_vec()] {
            let (p, mut f) = temp(&bytes);
            assert!(validate(
                &mut f,
                bytes.len() as u64,
                NativeTlsArchiveFormat::CoffX86_64,
                b"id"
            )
            .is_err());
            drop(f);
            std::fs::remove_file(p).ok();
        }
    }

    #[test]
    fn rejects_wrapping_and_out_of_member_object_offsets() {
        let member = Member {
            name: b"overflow.o".to_vec(),
            offset: u64::MAX - 1,
            len: 8,
        };
        assert!(member_range(&member, 0, 1, "fixture member")
            .unwrap_err()
            .contains("overflows"));

        let id = b"fixture-build-id";
        let mut elf = elf_fixture(id);
        elf[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        let error = validation_error(&elf, NativeTlsArchiveFormat::ElfX86_64, id);
        assert!(error.contains("overflows"), "{error}");

        let mut coff = coff_fixture(id);
        coff[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        let error = validation_error(&coff, NativeTlsArchiveFormat::CoffX86_64, id);
        assert!(error.contains("outside its bound"), "{error}");

        let mut macho = macho_fixture(id);
        macho[152..156].copy_from_slice(&u32::MAX.to_le_bytes());
        let error = validation_error(&macho, NativeTlsArchiveFormat::MachOArm64, id);
        assert!(error.contains("outside its bound"), "{error}");
    }

    #[test]
    fn charges_overlapping_elf_sections_against_the_scan_budget() {
        let id = b"fixture-build-id";
        let mut object = elf_fixture(id);
        append_elf_section(&mut object, 1);
        let bytes = ar("ku_native_tls.fixture.rcgu.o", &object);
        let (path, mut file) = temp(&bytes);
        let error = validate_with_scan_limit(
            &mut file,
            bytes.len() as u64,
            NativeTlsArchiveFormat::ElfX86_64,
            id,
            id.len() as u64,
        )
        .unwrap_err();
        assert!(
            error.contains("aggregate build-id section scan exceeds"),
            "{error}"
        );
        drop(file);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn default_scan_budget_is_bounded_by_the_archive_length() {
        let id = b"fixture-build-id";
        let mut object = elf_fixture(id);
        let section_table = u64le(&object[40..48]);
        let data_len = section_table - 64;
        let data_section = section_table as usize + 64;
        object[data_section + 32..data_section + 40].copy_from_slice(&data_len.to_le_bytes());
        append_elf_section(&mut object, 1);

        let bytes = ar("ku_native_tls.fixture.rcgu.o", &object);
        assert!(data_len * 2 > bytes.len() as u64);
        let (path, mut file) = temp(&bytes);
        let error = validate(
            &mut file,
            bytes.len() as u64,
            NativeTlsArchiveFormat::ElfX86_64,
            id,
        )
        .unwrap_err();
        assert!(
            error.contains("aggregate build-id section scan exceeds"),
            "{error}"
        );
        drop(file);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_multiple_elf_symbol_tables_before_reading_strings() {
        let id = b"fixture-build-id";
        let mut object = elf_fixture(id);
        let section_table = u64le(&object[40..48]) as usize;
        let string_section = section_table + 3 * 64;
        object[string_section + 24..string_section + 32].copy_from_slice(&u64::MAX.to_le_bytes());
        append_elf_section(&mut object, 2);
        let error = validation_error(&object, NativeTlsArchiveFormat::ElfX86_64, id);
        assert_eq!(error, "ELF object contains multiple symbol tables");
    }

    #[test]
    fn rejects_archive_length_above_core_limit_before_reading() {
        let bytes = b"!<arch>\n";
        let (path, mut file) = temp(bytes);
        let error = validate(
            &mut file,
            MAX_ARCHIVE_BYTES + 1,
            NativeTlsArchiveFormat::CoffX86_64,
            b"id",
        )
        .unwrap_err();
        assert!(error.contains("archive exceeds"), "{error}");
        drop(file);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn validates_opt_in_real_msvc_rust_staticlib() {
        let Some(path) = std::env::var_os("KU_NATIVE_TLS_REAL_ARCHIVE_FILE") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        assert!(
            path.is_absolute(),
            "real archive test path must be absolute"
        );
        let metadata = std::fs::symlink_metadata(&path).expect("inspect real MSVC TLS archive");
        assert!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "real archive test input must be a plain file"
        );
        let mut file = OpenOptions::new().read(true).open(&path).unwrap();
        let len = file.metadata().unwrap().len();
        validate(&mut file, len, NativeTlsArchiveFormat::CoffX86_64,
            b"ku-native-tls/0.1.0;abi=1;rustls=0.23.40;ring=0.17.14;webpki-roots=1.0.7;buffer=65536;handshake=1048576;record-staging=65540;resumption=disabled")
            .expect("real MSVC ku-native-tls staticlib must validate");
    }
}
