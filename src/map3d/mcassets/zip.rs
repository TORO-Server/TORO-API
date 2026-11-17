//! zip (jar) の中のファイルを読む小さなもの (Minecraft のクライアントの jar と、Unifont の zip)。
//! 移管前の地図サーバーの `server/utils/zip.ts` の移植。中央ディレクトリだけを先に読み、ファイルはそのつど位置を指定して読む
//! (jar 全体をメモリに載せない)。deflate (8) と無圧縮 (0)、zip64 の大きさと位置だけを扱う。

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use flate2::read::DeflateDecoder;

/// 1 つのファイルの展開後の大きさの上限 (壊れた zip で大きなメモリを使わないように)
const MAX_ENTRY: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: u64 = 200_000;
const MAX_DIRECTORY: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ZipEntry {
    pub name: String,
    method: u16,
    compressed_size: u64,
    size: u64,
    offset: u64,
}

pub struct ZipFile<R: Read + Seek = File> {
    reader: R,
    size: u64,
    /// 中央ディレクトリの順 (同じ名前が重なったら、最初の位置に後のものを入れる。TS の `Map` と同じ)
    entries: Vec<ZipEntry>,
    index: HashMap<String, usize>,
}

fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("zip: {message}"))
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

impl ZipFile<File> {
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::new(File::open(path)?)
    }
}

impl<R: Read + Seek> ZipFile<R> {
    pub fn new(mut reader: R) -> io::Result<Self> {
        let size = reader.seek(SeekFrom::End(0))?;
        let mut zip = Self {
            reader,
            size,
            entries: Vec::new(),
            index: HashMap::new(),
        };
        zip.read_directory()?;
        Ok(zip)
    }

    fn read_at(&mut self, position: u64, length: u64) -> io::Result<Vec<u8>> {
        if position
            .checked_add(length)
            .is_none_or(|end| end > self.size)
        {
            return Err(bad("unexpected end of file"));
        }
        self.reader.seek(SeekFrom::Start(position))?;
        let mut buf = vec![0u8; length as usize];
        self.reader.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn read_directory(&mut self) -> io::Result<()> {
        // 中央ディレクトリの終わり (EOCD) を後ろから探す (コメントは最大 65535 バイト)
        let tail = self.size.min(65557);
        let end = self.read_at(self.size - tail, tail)?;
        let at = (0..=end.len().saturating_sub(22))
            .rev()
            .find(|&i| end.len() >= 22 && u32_at(&end, i) == 0x0605_4B50)
            .ok_or_else(|| bad("no end of central directory"))?;
        let mut count = u64::from(u16_at(&end, at + 10));
        let mut dir_size = u64::from(u32_at(&end, at + 12));
        let mut dir_offset = u64::from(u32_at(&end, at + 16));
        // zip64
        if (dir_offset == 0xFFFF_FFFF || count == 0xFFFF)
            && at >= 20
            && u32_at(&end, at - 20) == 0x0706_4B50
        {
            let eocd64 = u64_at(&end, at - 20 + 8);
            let rec = self.read_at(eocd64, 56)?;
            if u32_at(&rec, 0) != 0x0606_4B50 {
                return Err(bad("bad zip64 record"));
            }
            count = u64_at(&rec, 32);
            dir_size = u64_at(&rec, 40);
            dir_offset = u64_at(&rec, 48);
        }
        if count > MAX_ENTRIES || dir_size > MAX_DIRECTORY || dir_offset + dir_size > self.size {
            return Err(bad("bad central directory"));
        }
        let dir = self.read_at(dir_offset, dir_size)?;
        let mut p = 0usize;
        for _ in 0..count {
            if p + 46 > dir.len() || u32_at(&dir, p) != 0x0201_4B50 {
                return Err(bad("bad central directory entry"));
            }
            let method = u16_at(&dir, p + 10);
            let mut compressed_size = u64::from(u32_at(&dir, p + 20));
            let mut size = u64::from(u32_at(&dir, p + 24));
            let name_length = usize::from(u16_at(&dir, p + 28));
            let extra_length = usize::from(u16_at(&dir, p + 30));
            let comment_length = usize::from(u16_at(&dir, p + 32));
            let mut offset = u64::from(u32_at(&dir, p + 42));
            let name_end = p + 46 + name_length;
            let extra_end = name_end + extra_length;
            if extra_end + comment_length > dir.len() {
                return Err(bad("bad central directory entry"));
            }
            let name = String::from_utf8_lossy(&dir[p + 46..name_end]).into_owned();
            // zip64 の追加の欄
            let mut e = name_end;
            while e + 4 <= extra_end {
                let id = u16_at(&dir, e);
                let len = usize::from(u16_at(&dir, e + 2));
                if id == 1 {
                    let mut q = e + 4;
                    let field_end = (e + 4 + len).min(dir.len());
                    for (field, marker) in [
                        (&mut size, 0xFFFF_FFFFu64),
                        (&mut compressed_size, 0xFFFF_FFFF),
                        (&mut offset, 0xFFFF_FFFF),
                    ] {
                        if *field == marker && q + 8 <= field_end {
                            *field = u64_at(&dir, q);
                            q += 8;
                        }
                    }
                }
                e += 4 + len;
            }
            p = extra_end + comment_length;
            if name.ends_with('/') {
                continue;
            }
            let entry = ZipEntry {
                name: name.clone(),
                method,
                compressed_size,
                size,
                offset,
            };
            match self.index.get(&name) {
                Some(&i) => self.entries[i] = entry,
                None => {
                    self.index.insert(name, self.entries.len());
                    self.entries.push(entry);
                }
            }
        }
        Ok(())
    }

    /// 名前の一覧 (中央ディレクトリの順)
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// ファイルの中身。なければ None
    pub fn get(&mut self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let Some(&i) = self.index.get(name) else {
            return Ok(None);
        };
        let e = self.entries[i].clone();
        if e.size > MAX_ENTRY || e.compressed_size > MAX_ENTRY {
            return Err(bad(&format!("entry too large: {name}")));
        }
        let head = self.read_at(e.offset, 30)?;
        if u32_at(&head, 0) != 0x0403_4B50 {
            return Err(bad(&format!("bad local header: {name}")));
        }
        let start = e.offset + 30 + u64::from(u16_at(&head, 26)) + u64::from(u16_at(&head, 28));
        let raw = self.read_at(start, e.compressed_size)?;
        match e.method {
            0 => Ok(Some(raw)),
            8 => {
                let mut out = Vec::with_capacity(e.size as usize);
                DeflateDecoder::new(&raw[..])
                    .take(MAX_ENTRY + 1)
                    .read_to_end(&mut out)
                    .map_err(|err| bad(&format!("cannot inflate {name}: {err}")))?;
                if out.len() as u64 != e.size {
                    return Err(bad(&format!("size mismatch: {name}")));
                }
                Ok(Some(out))
            }
            m => Err(bad(&format!("unsupported method {m}: {name}"))),
        }
    }
}
