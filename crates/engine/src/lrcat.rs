//! Read-only Lightroom Classic (`.lrcat`) migration input.
//!
//! Lightroom catalogs are SQLite files. LightCraft deliberately does not link SQLite (the
//! product is pure Rust), so this reader implements the safe, read-only table subset needed to
//! locate Lightroom file records and compressed XMP packets. It never writes the source catalog.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use miniz_oxide::inflate::decompress_to_vec_zlib;

const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ROWS: usize = 1_000_000;
const MAX_PACKET_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub xmp: Option<String>,
}

/// An opened, read-only Lightroom catalog.
#[derive(Debug)]
pub struct Catalog {
    db: Db,
    tables: HashMap<String, Table>,
}

#[derive(Debug)]
struct Table {
    root: u32,
    columns: Vec<String>,
}

#[derive(Clone, Debug)]
enum Value {
    Null,
    Integer(i64),
    Text(String),
    Blob(Vec<u8>),
    Real(f64),
}

impl Value {
    fn int(&self) -> Option<i64> {
        match self {
            Self::Integer(v) => Some(*v),
            Self::Real(v) if v.is_finite() && v.fract() == 0.0 && *v >= i64::MIN as f64 && *v <= i64::MAX as f64 => Some(*v as i64),
            _ => None,
        }
    }
    fn text(&self) -> Option<&str> {
        if let Self::Text(v) = self { Some(v) } else { None }
    }
    fn blob(&self) -> Option<&[u8]> {
        if let Self::Blob(v) = self { Some(v) } else { None }
    }
}

impl Catalog {
    /// Read `path` without modifying or locking it.
    pub fn open(path: &Path) -> Result<Self, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
        if meta.len() > MAX_CATALOG_BYTES {
            return Err(format!("{} is larger than the 4 GiB Lightroom import limit", path.display()));
        }
        let mut bytes = std::fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
        // Lightroom commonly leaves recent catalog pages in the SQLite WAL. Overlay the latest
        // committed frame for each page without opening or modifying the source files.
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        if let Ok(wal_bytes) = std::fs::read(&wal) {
            overlay_wal(&mut bytes, &wal_bytes)?;
        }
        let db = Db::new(bytes)?;
        let mut tables = HashMap::new();
        for row in db.table_rows(1)? {
            let Some(kind) = row.first().and_then(Value::text) else { continue };
            if kind != "table" {
                continue;
            }
            let (Some(name), Some(root), Some(sql)) =
                (row.get(1).and_then(Value::text), row.get(3).and_then(Value::int), row.get(4).and_then(Value::text))
            else {
                continue;
            };
            if root > 0 && root <= i64::from(u32::MAX) {
                tables.insert(name.to_string(), Table { root: root as u32, columns: columns(sql) });
            }
        }
        if !tables.contains_key("Adobe_images") || !tables.contains_key("AgLibraryFile") {
            return Err("not a supported Lightroom Classic catalog (required photo tables are missing)".into());
        }
        Ok(Self { db, tables })
    }

    /// Photo paths plus the XMP Lightroom keeps in its catalog. Missing source files are left for
    /// the caller to report; paths are never guessed or rewritten.
    pub fn entries(&self) -> Result<Vec<Entry>, String> {
        let roots = self.rows("AgLibraryRootFolder")?;
        let folders = self.rows("AgLibraryFolder")?;
        let files_rows = self.rows("AgLibraryFile")?;
        let images = self.rows("Adobe_images")?;
        let packets = if self.tables.contains_key("Adobe_AdditionalMetadata") { self.rows("Adobe_AdditionalMetadata")? } else { Vec::new() };

        let roots: HashMap<i64, String> = roots.iter().filter_map(|r| Some((id(r)?, text(r, "absolutePath")?.to_string()))).collect();
        let folders: HashMap<i64, (i64, String)> =
            folders.iter().filter_map(|r| Some((id(r)?, (integer(r, "rootFolder")?, text(r, "pathFromRoot").unwrap_or("").to_string())))).collect();
        let files: HashMap<i64, (i64, String)> = files_rows
            .iter()
            .filter_map(|r| {
                let name = text(r, "originalFilename").filter(|s| !s.is_empty()).map(str::to_string).or_else(|| {
                    let base = text(r, "baseName")?;
                    let ext = text(r, "extension").unwrap_or("");
                    Some(if ext.is_empty() { base.to_string() } else { format!("{base}.{ext}") })
                })?;
                Some((id(r)?, (integer(r, "folder")?, name)))
            })
            .collect();
        let packets: HashMap<i64, String> = packets
            .iter()
            .filter_map(|r| Some((integer(r, "image")?, r.get("xmp").and_then(Value::blob).and_then(|b| unpack_xmp(b).ok())?)))
            .collect();

        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for image in images {
            let (Some(image_id), Some(file_id)) = (id(&image), integer(&image, "rootFile").or_else(|| integer(&image, "file"))) else { continue };
            let Some((folder_id, name)) = files.get(&file_id) else {
                // Some Lightroom generations keep a complete path on the image/file row.
                if let Some(path) = text(&image, "absolutePath").or_else(|| text(&image, "path")).filter(|p| !p.is_empty()) {
                    if seen.insert(path.to_string()) {
                        out.push(Entry { path: path.to_string(), xmp: packets.get(&image_id).cloned() });
                    }
                }
                continue;
            };
            let Some((root_id, relative)) = folders.get(folder_id) else { continue };
            let Some(root) = roots.get(root_id) else { continue };
            let path = PathBuf::from(root).join(relative.trim_start_matches(['/', '\\'])).join(name).to_string_lossy().to_string();
            if seen.insert(path.clone()) {
                out.push(Entry { path, xmp: packets.get(&image_id).cloned() });
            }
        }
        // A few catalogs omit the folder relation but retain an absolute path on AgLibraryFile.
        if out.is_empty() {
            for (file_id, (_folder, name)) in &files {
                let Some(row) = files_rows.iter().find(|r| id(r) == Some(*file_id)) else { continue };
                let Some(path) = text(row, "absolutePath").or_else(|| text(row, "path")).filter(|p| !p.is_empty()) else { continue };
                let path = if Path::new(path).extension().is_some() {
                    path.to_string()
                } else {
                    PathBuf::from(path).join(name).to_string_lossy().to_string()
                };
                if seen.insert(path.clone()) {
                    out.push(Entry { path, xmp: None });
                }
            }
        }
        if out.is_empty() {
            return Err("the Lightroom catalog has no readable photo paths".into());
        }
        Ok(out)
    }

    fn rows(&self, name: &str) -> Result<Vec<BTreeMap<String, Value>>, String> {
        let table = self.tables.get(name).ok_or_else(|| format!("Lightroom catalog has no `{name}` table"))?;
        Ok(self.db.table_rows(table.root)?.into_iter().map(|row| table.columns.iter().cloned().zip(row).collect()).collect())
    }
}

fn id(row: &BTreeMap<String, Value>) -> Option<i64> {
    integer(row, "id_local")
}
fn integer(row: &BTreeMap<String, Value>, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::int)
}
fn text<'a>(row: &'a BTreeMap<String, Value>, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::text)
}

fn unpack_xmp(blob: &[u8]) -> Result<String, String> {
    let data = blob.get(4..).ok_or("truncated Lightroom XMP packet")?;
    let xmp = decompress_to_vec_zlib(data).map_err(|_| "invalid compressed Lightroom XMP packet")?;
    if xmp.len() > MAX_PACKET_BYTES {
        return Err("Lightroom XMP packet exceeds 64 MiB safety limit".into());
    }
    String::from_utf8(xmp).map_err(|_| "Lightroom XMP packet is not UTF-8".into())
}

fn columns(sql: &str) -> Vec<String> {
    let Some(start) = sql.find('(') else { return Vec::new() };
    let Some(end) = sql.rfind(')') else { return Vec::new() };
    sql[start + 1..end]
        .split(',')
        .filter_map(|part| {
            let name = part.trim().split_whitespace().next()?;
            (!matches!(name.to_ascii_uppercase().as_str(), "PRIMARY" | "UNIQUE" | "CONSTRAINT" | "FOREIGN" | "CHECK")
                && !name.to_ascii_uppercase().starts_with("PRIMARY(")
                && !name.to_ascii_uppercase().starts_with("UNIQUE(")
                && !name.to_ascii_uppercase().starts_with("CONSTRAINT(")
                && !name.to_ascii_uppercase().starts_with("FOREIGN(")
                && !name.to_ascii_uppercase().starts_with("CHECK("))
            .then(|| name.trim_matches(['`', '"', '[', ']']).to_string())
        })
        .collect()
}

#[derive(Debug)]
struct Db {
    bytes: Vec<u8>,
    page_size: usize,
}

impl Db {
    fn new(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.get(..16) != Some(b"SQLite format 3\0") {
            return Err("not an SQLite database".into());
        }
        let raw = be16(bytes.get(16..18).ok_or("truncated SQLite header")?)? as usize;
        let page_size = if raw == 1 { 65_536 } else { raw };
        if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() || bytes.len() < page_size {
            return Err("unsupported SQLite page size".into());
        }
        Ok(Self { bytes, page_size })
    }
    fn page(&self, n: u32) -> Result<&[u8], String> {
        let start = (n as usize).checked_sub(1).and_then(|p| p.checked_mul(self.page_size)).ok_or("invalid SQLite page number")?;
        self.bytes.get(start..start.saturating_add(self.page_size)).ok_or_else(|| "SQLite page lies outside catalog".into())
    }
    fn table_rows(&self, root: u32) -> Result<Vec<Vec<Value>>, String> {
        let mut pages = Vec::new();
        self.table_pages(root, 0, &mut pages)?;
        let mut out = Vec::new();
        for page_no in pages {
            let page = self.page(page_no)?;
            let h = if page_no == 1 { 100 } else { 0 };
            let cells = be16(page.get(h + 3..h + 5).ok_or("truncated SQLite table header")?)? as usize;
            for i in 0..cells {
                if out.len() >= MAX_ROWS {
                    return Err("Lightroom catalog exceeds the one-million-record safety limit".into());
                }
                let at = h.checked_add(8).and_then(|v| v.checked_add(i.saturating_mul(2))).ok_or("SQLite cell offset overflow")?;
                let off = be16(page.get(at..at + 2).ok_or("truncated SQLite cell pointer")?)? as usize;
                let cell = page.get(off..).ok_or("SQLite cell lies outside its page")?;
                let (size, a) = varint(cell)?;
                let (rowid, b) = varint(cell.get(a..).ok_or("truncated SQLite rowid")?)?;
                let values = record(&self.payload(page_no, off, size as usize, a + b)?)?;
                // `INTEGER PRIMARY KEY` is an alias for SQLite's rowid, so its on-page record
                // field is NULL. Lightroom uses this for every foreign-key join (`id_local`).
                out.push(with_rowid(values, rowid));
            }
        }
        Ok(out)
    }
    fn table_pages(&self, page_no: u32, depth: usize, out: &mut Vec<u32>) -> Result<(), String> {
        if depth > 100 || out.len() > MAX_ROWS {
            return Err("SQLite table b-tree is too deep or large".into());
        }
        let page = self.page(page_no)?;
        let h = if page_no == 1 { 100 } else { 0 };
        let kind = *page.get(h).ok_or("truncated SQLite b-tree page")?;
        let cells = be16(page.get(h + 3..h + 5).ok_or("truncated SQLite b-tree header")?)? as usize;
        match kind {
            13 => out.push(page_no),
            5 => {
                for i in 0..cells {
                    let at = h.checked_add(12).and_then(|v| v.checked_add(i.saturating_mul(2))).ok_or("SQLite cell offset overflow")?;
                    let off = be16(page.get(at..at + 2).ok_or("truncated SQLite cell pointer")?)? as usize;
                    self.table_pages(be32(page.get(off..off + 4).ok_or("truncated SQLite child page")?)?, depth + 1, out)?;
                }
                self.table_pages(be32(page.get(h + 8..h + 12).ok_or("truncated SQLite right child")?)?, depth + 1, out)?;
            }
            _ => return Err("SQLite table has an unsupported b-tree page".into()),
        }
        Ok(())
    }
    fn payload(&self, page_no: u32, off: usize, size: usize, header: usize) -> Result<Vec<u8>, String> {
        if size > MAX_PACKET_BYTES.saturating_add(1024 * 1024) {
            return Err("SQLite record exceeds safety limit".into());
        }
        let max_local = self.page_size.saturating_sub(35);
        let min_local = ((self.page_size.saturating_sub(12)) * 32 / 255).saturating_sub(23);
        let local = if size <= max_local {
            size
        } else {
            let n = min_local + (size - min_local) % (self.page_size - 4);
            if n > max_local { min_local } else { n }
        };
        let page = self.page(page_no)?;
        let start = off.checked_add(header).ok_or("SQLite payload offset overflow")?;
        let mut out = page.get(start..start + local).ok_or("truncated SQLite payload")?.to_vec();
        if local == size {
            return Ok(out);
        }
        let mut next = be32(page.get(start + local..start + local + 4).ok_or("truncated SQLite overflow pointer")?)?;
        let mut hops = 0usize;
        while out.len() < size {
            if next == 0 || hops > MAX_ROWS {
                return Err("invalid SQLite overflow chain".into());
            }
            hops += 1;
            let page = self.page(next)?;
            next = be32(page.get(..4).ok_or("truncated SQLite overflow page")?)?;
            let take = (size - out.len()).min(self.page_size - 4);
            out.extend_from_slice(page.get(4..4 + take).ok_or("truncated SQLite overflow payload")?);
        }
        Ok(out)
    }
}

fn record(bytes: &[u8]) -> Result<Vec<Value>, String> {
    let (header, first) = varint(bytes)?;
    let end = header as usize;
    if end < first || end > bytes.len() {
        return Err("invalid SQLite record header".into());
    }
    let mut types = Vec::new();
    let mut at = first;
    while at < end {
        let (kind, used) = varint(bytes.get(at..end).ok_or("truncated SQLite serial type")?)?;
        types.push(kind);
        at += used;
    }
    let mut data = end;
    let mut out = Vec::new();
    for kind in types {
        let (value, used) = serial(kind, bytes.get(data..).ok_or("truncated SQLite field")?)?;
        data = data.checked_add(used).ok_or("SQLite field offset overflow")?;
        out.push(value);
    }
    Ok(out)
}
fn serial(kind: u64, b: &[u8]) -> Result<(Value, usize), String> {
    let n = match kind {
        0 => return Ok((Value::Null, 0)),
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        8 => return Ok((Value::Integer(0), 0)),
        9 => return Ok((Value::Integer(1), 0)),
        10 | 11 => return Err("reserved SQLite serial type".into()),
        k if k >= 12 => ((k - if k % 2 == 0 { 12 } else { 13 }) / 2) as usize,
        _ => return Err("invalid SQLite serial type".into()),
    };
    let data = b.get(..n).ok_or("truncated SQLite field")?;
    let value = match kind {
        1..=6 => Value::Integer(signed(data)),
        7 => Value::Real(f64::from_bits(u64::from_be_bytes(data.try_into().map_err(|_| "invalid SQLite real")?))),
        k if k % 2 == 0 => Value::Blob(data.to_vec()),
        _ => Value::Text(String::from_utf8_lossy(data).into_owned()),
    };
    Ok((value, n))
}
fn signed(bytes: &[u8]) -> i64 {
    let mut value = 0i64;
    for byte in bytes {
        value = (value << 8) | i64::from(*byte);
    }
    if bytes.first().is_some_and(|b| b & 0x80 != 0) { value - (1i64 << (bytes.len() * 8)) } else { value }
}
fn varint(bytes: &[u8]) -> Result<(u64, usize), String> {
    let mut value = 0u64;
    for i in 0..9 {
        let byte = *bytes.get(i).ok_or("truncated SQLite varint")?;
        if i == 8 {
            return Ok(((value << 8) | u64::from(byte), 9));
        }
        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    Err("invalid SQLite varint".into())
}
fn be16(bytes: &[u8]) -> Result<u16, String> {
    let a: [u8; 2] = bytes.try_into().map_err(|_| "truncated SQLite u16")?;
    Ok(u16::from_be_bytes(a))
}
fn be32(bytes: &[u8]) -> Result<u32, String> {
    let a: [u8; 4] = bytes.try_into().map_err(|_| "truncated SQLite u32")?;
    Ok(u32::from_be_bytes(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_sqlite_input() {
        let error = Db::new(b"not a catalog".to_vec()).expect_err("invalid input must be rejected");
        assert!(error.contains("SQLite"));
    }

    #[test]
    fn extracts_declared_column_names() {
        let names = columns("CREATE TABLE sample (id_local INTEGER PRIMARY KEY, name TEXT, UNIQUE(name))");
        assert_eq!(names, ["id_local", "name"]);
    }

    #[test]
    fn integer_primary_key_uses_sqlite_rowid() {
        let values = with_rowid(vec![Value::Null, Value::Text("photo".into())], 42);
        assert!(matches!(values.first(), Some(Value::Integer(42))));
        let values = with_rowid(vec![Value::Integer(7)], 42);
        assert!(matches!(values.first(), Some(Value::Integer(7))));
    }
}

fn with_rowid(mut values: Vec<Value>, rowid: u64) -> Vec<Value> {
    if matches!(values.first(), Some(Value::Null)) && rowid <= i64::MAX as u64 {
        values[0] = Value::Integer(rowid as i64);
    }
    values
}

fn overlay_wal(db: &mut [u8], wal: &[u8]) -> Result<(), String> {
    if wal.len() < 32 || wal.get(..4) != Some(&[0x37, 0x7f, 0x06, 0x82]) {
        return Ok(());
    }
    let page_size = u32::from_be_bytes(wal[8..12].try_into().map_err(|_| "invalid SQLite WAL header")?) as usize;
    if !(512..=65_536).contains(&page_size) || wal.len() < 32 + 24 + page_size {
        return Ok(());
    }
    let mut at = 32usize;
    while at.checked_add(24 + page_size).is_some_and(|end| end <= wal.len()) {
        let page_no = u32::from_be_bytes(wal[at..at + 4].try_into().map_err(|_| "invalid SQLite WAL frame")?) as usize;
        let src = &wal[at + 24..at + 24 + page_size];
        let dst_start = page_no.saturating_sub(1).saturating_mul(page_size);
        if page_no > 0 && dst_start.checked_add(page_size).is_some_and(|end| end <= db.len()) {
            db[dst_start..dst_start + page_size].copy_from_slice(src);
        }
        at += 24 + page_size;
    }
    Ok(())
}
