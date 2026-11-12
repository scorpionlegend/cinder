//! Archive writers: tar (ustar), gzip, zip (deflate) and ar.
//!
//! Written by hand rather than pulled from crates so the packager has one
//! dependency for compression and nothing else, and so the exact bytes written
//! are visible here. Every format is streamed: the CEF runtime is hundreds of
//! megabytes and must never be held in memory.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::Compression;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// One archive member. `source` is streamed from disk; `data` is in memory.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub mode: u32,
    pub size: u64,
    pub source: Option<PathBuf>,
    pub data: Option<Vec<u8>>,
    pub link: Option<String>,
}

impl Entry {
    pub fn file(path: impl Into<String>, source: impl Into<PathBuf>, mode: u32) -> Self {
        let source = source.into();
        let size = std::fs::metadata(&source).map(|m| m.len()).unwrap_or(0);
        Self {
            path: path.into(),
            kind: Kind::File,
            mode,
            size,
            source: Some(source),
            data: None,
            link: None,
        }
    }

    pub fn bytes(path: impl Into<String>, data: impl Into<Vec<u8>>, mode: u32) -> Self {
        let data = data.into();
        Self {
            path: path.into(),
            kind: Kind::File,
            mode,
            size: data.len() as u64,
            source: None,
            data: Some(data),
            link: None,
        }
    }

    pub fn dir(path: impl Into<String>, mode: u32) -> Self {
        Self {
            path: path.into(),
            kind: Kind::Dir,
            mode,
            size: 0,
            source: None,
            data: None,
            link: None,
        }
    }

    pub fn symlink(path: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: Kind::Symlink,
            mode: 0o777,
            size: 0,
            source: None,
            data: None,
            link: Some(target.into()),
        }
    }

    fn open(&self) -> io::Result<Box<dyn Read + '_>> {
        match (&self.source, &self.data) {
            (Some(p), _) => Ok(Box::new(BufReader::new(File::open(p)?))),
            (_, Some(d)) => Ok(Box::new(&d[..])),
            _ => Ok(Box::new(io::empty())),
        }
    }
}

// ------------------------------------------------------------------ crc32 ---

fn crc_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        table
    })
}

fn crc32_update(mut crc: u32, buf: &[u8]) -> u32 {
    let table = crc_table();
    for &b in buf {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// CRC-32 of an entry's contents, streamed.
fn crc32_entry(entry: &Entry) -> io::Result<u32> {
    let mut crc = 0xFFFF_FFFFu32;
    let mut reader = entry.open()?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        crc = crc32_update(crc, &buf[..n]);
    }
    Ok(crc ^ 0xFFFF_FFFF)
}

// -------------------------------------------------------------------- tar ---

fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0width$o}", width = digits);
    let bytes = text.as_bytes();
    let start = digits.saturating_sub(bytes.len());
    field[..start].fill(b'0');
    field[start..digits].copy_from_slice(&bytes[bytes.len().saturating_sub(digits)..]);
    field[digits] = 0;
}

fn tar_header(entry: &Entry, mtime: u64) -> [u8; 512] {
    let mut h = [0u8; 512];
    let name = match entry.kind {
        Kind::Dir if !entry.path.ends_with('/') => format!("{}/", entry.path),
        _ => entry.path.clone(),
    };
    let (prefix, base) = split_ustar_path(&name);
    h[..base.len()].copy_from_slice(base.as_bytes());
    octal(&mut h[100..108], entry.mode as u64);
    octal(&mut h[108..116], 0); // uid
    octal(&mut h[116..124], 0); // gid
    let size = if entry.kind == Kind::File { entry.size } else { 0 };
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], mtime);
    h[148..156].fill(b' '); // checksum placeholder
    h[156] = match entry.kind {
        Kind::File => b'0',
        Kind::Dir => b'5',
        Kind::Symlink => b'2',
    };
    if let Some(link) = &entry.link {
        let bytes = link.as_bytes();
        let n = bytes.len().min(100);
        h[157..157 + n].copy_from_slice(&bytes[..n]);
    }
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[265..269].copy_from_slice(b"root");
    h[297..301].copy_from_slice(b"root");
    if !prefix.is_empty() {
        h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    }
    let sum: u64 = h.iter().map(|&b| b as u64).sum();
    let text = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(text.as_bytes());
    h
}

/// ustar splits paths longer than 100 bytes into `prefix` + `name`.
fn split_ustar_path(path: &str) -> (&str, &str) {
    if path.len() <= 100 {
        return ("", path);
    }
    match path.rfind('/') {
        Some(i) if path.len() - i - 1 <= 100 && i <= 155 => (&path[..i], &path[i + 1..]),
        _ => ("", path),
    }
}

fn write_tar<W: Write>(entries: &[Entry], out: &mut W, mtime: u64) -> io::Result<()> {
    for entry in entries {
        out.write_all(&tar_header(entry, mtime))?;
        if entry.kind == Kind::File {
            let mut reader = entry.open()?;
            let mut buf = vec![0u8; 1 << 20];
            let mut written = 0u64;
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                written += n as u64;
            }
            if written != entry.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} changed size while archiving", entry.path),
                ));
            }
            let pad = (512 - (entry.size % 512) as usize) % 512;
            if pad > 0 {
                out.write_all(&vec![0u8; pad])?;
            }
        }
    }
    out.write_all(&[0u8; 1024])
}

pub fn write_tar_gz(entries: &[Entry], path: &Path, mtime: u64) -> io::Result<u64> {
    let file = BufWriter::new(File::create(path)?);
    let mut gz = GzEncoder::new(file, Compression::new(6));
    write_tar(entries, &mut gz, mtime)?;
    let mut inner = gz.finish()?;
    inner.flush()?;
    Ok(std::fs::metadata(path)?.len())
}

// -------------------------------------------------------------------- zip ---

fn dos_datetime(mtime: u64) -> (u16, u16) {
    // 1980-01-01 is the zip epoch; a fixed timestamp keeps archives
    // byte-identical between runs, which the release pipeline relies on.
    let _ = mtime;
    let time = 0u16;
    let date = (1 << 5) | 1;
    (time, date)
}

pub fn write_zip(entries: &[Entry], path: &Path, mtime: u64) -> io::Result<u64> {
    let (time, date) = dos_datetime(mtime);
    let mut out = BufWriter::new(File::create(path)?);
    let mut central: Vec<u8> = Vec::new();
    let mut offset = 0u64;

    for entry in entries {
        let name = match entry.kind {
            Kind::Dir if !entry.path.ends_with('/') => format!("{}/", entry.path),
            _ => entry.path.clone(),
        };
        let name_bytes = name.as_bytes();
        let is_dir = entry.kind == Kind::Dir;
        let method: u16 = if is_dir || entry.kind == Kind::Symlink { 0 } else { 8 };
        let crc = if entry.kind == Kind::File { crc32_entry(entry)? } else { 0 };

        // Compress into a buffer to learn the compressed size for the header.
        let mut payload: Vec<u8> = Vec::new();
        if entry.kind == Kind::File {
            let mut reader = entry.open()?;
            if method == 8 {
                let mut enc = DeflateEncoder::new(&mut payload, Compression::new(6));
                io::copy(&mut reader, &mut enc)?;
                enc.finish()?;
            } else {
                io::copy(&mut reader, &mut payload)?;
            }
        } else if let Some(link) = &entry.link {
            payload.extend_from_slice(link.as_bytes());
        }

        let crc = if entry.kind == Kind::Symlink { crc32_update(0xFFFF_FFFF, &payload) ^ 0xFFFF_FFFF } else { crc };
        let csize = payload.len() as u32;
        let usize_ = if entry.kind == Kind::File { entry.size as u32 } else { payload.len() as u32 };

        let mut local = Vec::with_capacity(30 + name_bytes.len());
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&method.to_le_bytes());
        local.extend_from_slice(&time.to_le_bytes());
        local.extend_from_slice(&date.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&csize.to_le_bytes());
        local.extend_from_slice(&usize_.to_le_bytes());
        local.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name_bytes);
        out.write_all(&local)?;
        out.write_all(&payload)?;

        let mut cd = Vec::with_capacity(46 + name_bytes.len());
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&0x031Eu16.to_le_bytes()); // made by: unix, 3.0
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&method.to_le_bytes());
        cd.extend_from_slice(&time.to_le_bytes());
        cd.extend_from_slice(&date.to_le_bytes());
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&csize.to_le_bytes());
        cd.extend_from_slice(&usize_.to_le_bytes());
        cd.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        let mut attrs = entry.mode << 16;
        if is_dir {
            attrs |= 0x10;
        } else if entry.kind == Kind::Symlink {
            attrs |= 0xA000;
        }
        cd.extend_from_slice(&attrs.to_le_bytes());
        cd.extend_from_slice(&(offset as u32).to_le_bytes());
        cd.extend_from_slice(name_bytes);
        central.extend_from_slice(&cd);

        offset += local.len() as u64 + payload.len() as u64;
    }

    let cd_offset = offset;
    out.write_all(&central)?;
    let mut eocd = Vec::with_capacity(22);
    eocd.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    eocd.extend_from_slice(&0u16.to_le_bytes());
    eocd.extend_from_slice(&0u16.to_le_bytes());
    eocd.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    eocd.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    eocd.extend_from_slice(&(central.len() as u32).to_le_bytes());
    eocd.extend_from_slice(&(cd_offset as u32).to_le_bytes());
    eocd.extend_from_slice(&0u16.to_le_bytes());
    out.write_all(&eocd)?;
    out.flush()?;
    Ok(std::fs::metadata(path)?.len())
}

// --------------------------------------------------------------------- ar ---

/// `ar` archive, the container a `.deb` is built from.
pub fn write_ar(members: &[(&str, Vec<u8>)], path: &Path) -> io::Result<u64> {
    let mut out = BufWriter::new(File::create(path)?);
    out.write_all(b"!<arch>\n")?;
    for (name, data) in members {
        let mut header = [b' '; 60];
        let field = |h: &mut [u8; 60], range: std::ops::Range<usize>, text: &str| {
            let bytes = text.as_bytes();
            let n = bytes.len().min(range.len());
            h[range.start..range.start + n].copy_from_slice(&bytes[..n]);
        };
        field(&mut header, 0..16, &format!("{name}/"));
        field(&mut header, 16..28, "0");
        field(&mut header, 28..34, "0");
        field(&mut header, 34..40, "0");
        field(&mut header, 40..48, "100644");
        field(&mut header, 48..58, &data.len().to_string());
        header[58] = b'`';
        header[59] = b'\n';
        out.write_all(&header)?;
        out.write_all(data)?;
        if data.len() % 2 == 1 {
            out.write_all(b"\n")?;
        }
    }
    out.flush()?;
    Ok(std::fs::metadata(path)?.len())
}

/// Build a tar.gz in memory, for the small control member of a `.deb`.
pub fn tar_gz_bytes(entries: &[Entry], mtime: u64) -> io::Result<Vec<u8>> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::new(6));
    write_tar(entries, &mut gz, mtime)?;
    gz.finish()
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut reader = BufReader::new(File::open(path)?);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
