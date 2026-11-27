//! Reading a PE32+ image file: headers, sections, imports, base relocations and TLS.
//!
//! Every read is bounds-checked. A malformed file gives a [`ParseError`]; it never panics and
//! never reads outside the bytes it was given.

use std::fmt;

const MACHINE_AMD64: u16 = 0x8664;
const MAGIC_PE32_PLUS: u16 = 0x20B;
const RELOCS_STRIPPED: u16 = 0x0001;
const PAGE: u32 = 0x1000;

const SECTION_EXECUTE: u32 = 0x2000_0000;
const SECTION_READ: u32 = 0x4000_0000;
const SECTION_WRITE: u32 = 0x8000_0000;

const DIRECTORY_IMPORT: usize = 1;
const DIRECTORY_EXCEPTION: usize = 3;
const DIRECTORY_BASE_RELOCATION: usize = 5;
const DIRECTORY_TLS: usize = 9;
const DIRECTORY_DELAY_IMPORT: usize = 13;

const RELOCATION_ABSOLUTE: u8 = 0;
const RELOCATION_DIR64: u8 = 10;

/// The longest module or function name accepted.
const MAX_NAME: u32 = 4096;
/// More sections than this is taken as a corrupt header. The format allows 96.
const MAX_SECTIONS: usize = 96;

/// Why a file is not an image this loader accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The file ends before the named part.
    Truncated(&'static str),
    /// The named signature is missing.
    BadSignature(&'static str),
    /// The image is for another CPU. Titles are x86-64.
    WrongMachine(u16),
    /// The image is not PE32+ (64-bit).
    NotPe32Plus(u16),
    /// The named part contradicts itself or points outside the image.
    Malformed(&'static str),
    /// A base relocation of a kind an x86-64 image does not use.
    UnsupportedRelocation { kind: u8, rva: u32 },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Truncated(what) => write!(f, "the file ends before its {what}"),
            ParseError::BadSignature(what) => write!(f, "the {what} signature is missing"),
            ParseError::WrongMachine(machine) => {
                write!(f, "the image is for machine {machine:#06x}, not x86-64")
            }
            ParseError::NotPe32Plus(magic) => {
                write!(
                    f,
                    "the image is not PE32+ (optional header magic {magic:#06x})"
                )
            }
            ParseError::Malformed(what) => write!(f, "the image's {what} is malformed"),
            ParseError::UnsupportedRelocation { kind, rva } => {
                write!(f, "base relocation of unsupported kind {kind} at {rva:#x}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// One section of the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    /// Where the section starts in the mapped image.
    pub rva: u32,
    /// Its size in the mapped image. Bytes past `raw_size` are zero.
    pub virtual_size: u32,
    /// Where its bytes start in the file.
    pub raw_offset: u32,
    /// How many bytes the file holds for it.
    pub raw_size: u32,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

/// What an import is known by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Symbol {
    Name(String),
    Ordinal(u16),
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Symbol::Name(name) => f.write_str(name),
            Symbol::Ordinal(ordinal) => write!(f, "#{ordinal}"),
        }
    }
}

/// One imported function and the slot its address goes into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub symbol: Symbol,
    /// The 8-byte slot in the import address table.
    pub slot_rva: u32,
}

/// The functions an image imports from one module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedModule {
    /// The module's name as the image spells it, such as `standin_console.dll`.
    pub name: String,
    pub imports: Vec<Import>,
}

/// The image's thread-local storage directory, as offsets into the mapped image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tls {
    /// The template every thread's block starts as.
    pub template_rva: u32,
    pub template_size: u32,
    /// Zero bytes that follow the template in each thread's block.
    pub zero_fill: u32,
    /// Where the loader stores the image's TLS index.
    pub index_rva: u32,
    /// A null-terminated array of callback addresses, or `None`.
    pub callbacks_rva: Option<u32>,
}

/// A parsed PE32+ image file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// The address the image was linked for.
    pub preferred_base: u64,
    /// The size of the mapped image, headers included.
    pub size: u32,
    /// How many bytes at the start of the file are headers.
    pub header_size: u32,
    /// The entry point, as an offset into the mapped image. Zero if there is none.
    pub entry_rva: u32,
    pub sections: Vec<Section>,
    pub imports: Vec<ImportedModule>,
    /// Every 8-byte address in the image that moves with its base.
    pub relocations: Vec<u32>,
    /// Whether the image can be mapped anywhere but its preferred base.
    pub relocatable: bool,
    pub tls: Option<Tls>,
    /// The unwind table (`.pdata`): where it is and how many bytes.
    pub exception_table: Option<(u32, u32)>,
    /// Whether the image has delay-loaded imports, which are not bound here.
    pub has_delay_imports: bool,
}

impl Image {
    /// Parses an image file.
    pub fn parse(bytes: &[u8]) -> Result<Image, ParseError> {
        let file = File(bytes);
        if file.get(0, 2, "DOS header")? != b"MZ" {
            return Err(ParseError::BadSignature("MZ"));
        }
        let pe = file.u32(0x3C, "DOS header")? as usize;
        if file.get(pe, 4, "PE header")? != b"PE\0\0" {
            return Err(ParseError::BadSignature("PE"));
        }
        let coff = pe + 4;
        let machine = file.u16(coff, "file header")?;
        if machine != MACHINE_AMD64 {
            return Err(ParseError::WrongMachine(machine));
        }
        let section_count = file.u16(coff + 2, "file header")? as usize;
        let optional_size = file.u16(coff + 16, "file header")? as usize;
        let characteristics = file.u16(coff + 18, "file header")?;

        let optional = coff + 20;
        let magic = file.u16(optional, "optional header")?;
        if magic != MAGIC_PE32_PLUS {
            return Err(ParseError::NotPe32Plus(magic));
        }
        if optional_size < 112 {
            return Err(ParseError::Malformed("optional header size"));
        }
        file.get(optional, optional_size, "optional header")?;
        let entry_rva = file.u32(optional + 16, "optional header")?;
        let preferred_base = file.u64(optional + 24, "optional header")?;
        let section_alignment = file.u32(optional + 32, "optional header")?;
        let size = file.u32(optional + 56, "optional header")?;
        let header_size = file.u32(optional + 60, "optional header")?;
        let directory_count = file.u32(optional + 108, "optional header")? as usize;

        // Sections are protected page by page, so each has to start on a page of its own.
        if section_alignment < PAGE || !section_alignment.is_power_of_two() {
            return Err(ParseError::Malformed("section alignment"));
        }
        if size == 0 || header_size > size || header_size as usize > bytes.len() {
            return Err(ParseError::Malformed("image or header size"));
        }
        if directory_count > 16 || 112 + directory_count * 8 > optional_size {
            return Err(ParseError::Malformed("data directory count"));
        }
        let directory = |index: usize| -> Result<Option<(u32, u32)>, ParseError> {
            if index >= directory_count {
                return Ok(None);
            }
            let at = optional + 112 + index * 8;
            let rva = file.u32(at, "data directories")?;
            let length = file.u32(at + 4, "data directories")?;
            Ok((rva != 0 && length != 0).then_some((rva, length)))
        };

        if section_count > MAX_SECTIONS {
            return Err(ParseError::Malformed("section count"));
        }
        let mut sections = Vec::with_capacity(section_count);
        for index in 0..section_count {
            let at = optional + optional_size + index * 40;
            let header = file.get(at, 40, "section headers")?;
            let name_end = header[..8].iter().position(|&b| b == 0).unwrap_or(8);
            let flags = file.u32(at + 36, "section headers")?;
            let section = Section {
                name: String::from_utf8_lossy(&header[..name_end]).into_owned(),
                virtual_size: file.u32(at + 8, "section headers")?,
                rva: file.u32(at + 12, "section headers")?,
                raw_size: file.u32(at + 16, "section headers")?,
                raw_offset: file.u32(at + 20, "section headers")?,
                readable: flags & SECTION_READ != 0,
                writable: flags & SECTION_WRITE != 0,
                executable: flags & SECTION_EXECUTE != 0,
            };
            let extent = u64::from(section.virtual_size.max(section.raw_size));
            if !section.rva.is_multiple_of(PAGE)
                || section.rva < header_size
                || u64::from(section.rva) + extent > u64::from(size)
            {
                return Err(ParseError::Malformed("section placement"));
            }
            // Sections come in ascending order and do not overlap, as the linker writes
            // them. Otherwise the tables checked here would not be the bytes that end up
            // mapped.
            if let Some(previous) = sections
                .last()
                .map(|s: &Section| u64::from(s.rva) + u64::from(s.virtual_size.max(s.raw_size)))
                && u64::from(section.rva) < previous
            {
                return Err(ParseError::Malformed("section placement"));
            }
            if u64::from(section.raw_offset) + u64::from(section.raw_size) > bytes.len() as u64 {
                return Err(ParseError::Truncated("section data"));
            }
            sections.push(section);
        }

        let mut image = Image {
            preferred_base,
            size,
            header_size,
            entry_rva,
            sections,
            imports: Vec::new(),
            relocations: Vec::new(),
            relocatable: characteristics & RELOCS_STRIPPED == 0,
            tls: None,
            exception_table: None,
            has_delay_imports: directory(DIRECTORY_DELAY_IMPORT)?.is_some(),
        };

        if entry_rva != 0
            && !image.sections.iter().any(|s| {
                s.executable
                    && entry_rva >= s.rva
                    && entry_rva - s.rva < s.virtual_size.max(s.raw_size)
            })
        {
            return Err(ParseError::Malformed("entry point"));
        }

        let mapped = Mapped {
            file,
            image: &image,
        };
        let imports = match directory(DIRECTORY_IMPORT)? {
            Some((rva, _)) => mapped.imports(rva)?,
            None => Vec::new(),
        };
        let relocations = match directory(DIRECTORY_BASE_RELOCATION)? {
            Some((rva, length)) => mapped.relocations(rva, length)?,
            None => Vec::new(),
        };
        let tls = match directory(DIRECTORY_TLS)? {
            Some((rva, _)) => Some(mapped.tls(rva)?),
            None => None,
        };
        if let Some((rva, length)) = directory(DIRECTORY_EXCEPTION)? {
            if u64::from(rva) + u64::from(length) > u64::from(size) || !length.is_multiple_of(12) {
                return Err(ParseError::Malformed("exception table"));
            }
            image.exception_table = Some((rva, length));
        }
        image.imports = imports;
        image.relocations = relocations;
        image.tls = tls;
        Ok(image)
    }
}

/// The file's bytes, read with bounds checks.
#[derive(Clone, Copy)]
struct File<'a>(&'a [u8]);

impl<'a> File<'a> {
    fn get(
        &self,
        offset: usize,
        length: usize,
        what: &'static str,
    ) -> Result<&'a [u8], ParseError> {
        offset
            .checked_add(length)
            .and_then(|end| self.0.get(offset..end))
            .ok_or(ParseError::Truncated(what))
    }

    fn u16(&self, offset: usize, what: &'static str) -> Result<u16, ParseError> {
        let b = self.get(offset, 2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&self, offset: usize, what: &'static str) -> Result<u32, ParseError> {
        let b = self.get(offset, 4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&self, offset: usize, what: &'static str) -> Result<u64, ParseError> {
        let b = self.get(offset, 8, what)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
}

/// Reads the file by addresses in the mapped image.
struct Mapped<'a> {
    file: File<'a>,
    image: &'a Image,
}

impl<'a> Mapped<'a> {
    /// The file bytes behind `length` bytes of the mapped image at `rva`. Bytes a section
    /// holds only in memory (past its raw data) are not readable here.
    fn get(&self, rva: u32, length: usize, what: &'static str) -> Result<&'a [u8], ParseError> {
        let end = u64::from(rva) + length as u64;
        if end <= u64::from(self.image.header_size) {
            return self.file.get(rva as usize, length, what);
        }
        let section = self
            .image
            .sections
            .iter()
            .find(|s| rva >= s.rva && end <= u64::from(s.rva) + u64::from(s.raw_size))
            .ok_or(ParseError::Malformed(what))?;
        let offset = section.raw_offset as usize + (rva - section.rva) as usize;
        self.file.get(offset, length, what)
    }

    fn u32(&self, rva: u32, what: &'static str) -> Result<u32, ParseError> {
        let b = self.get(rva, 4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&self, rva: u32, what: &'static str) -> Result<u64, ParseError> {
        let b = self.get(rva, 8, what)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// A null-terminated ASCII name.
    fn name(&self, rva: u32, what: &'static str) -> Result<String, ParseError> {
        let mut name = String::new();
        for index in 0..MAX_NAME {
            let at = rva.checked_add(index).ok_or(ParseError::Malformed(what))?;
            let byte = self.get(at, 1, what)?[0];
            if byte == 0 {
                return if name.is_empty() {
                    Err(ParseError::Malformed(what))
                } else {
                    Ok(name)
                };
            }
            if !byte.is_ascii_graphic() {
                return Err(ParseError::Malformed(what));
            }
            name.push(byte as char);
        }
        Err(ParseError::Malformed(what))
    }

    /// Turns an address the image was linked with into an offset into the mapped image.
    fn rva_of(&self, address: u64, what: &'static str) -> Result<u32, ParseError> {
        address
            .checked_sub(self.image.preferred_base)
            .filter(|&rva| rva < u64::from(self.image.size))
            .map(|rva| rva as u32)
            .ok_or(ParseError::Malformed(what))
    }

    fn imports(&self, table_rva: u32) -> Result<Vec<ImportedModule>, ParseError> {
        const WHAT: &str = "import table";
        let bad = ParseError::Malformed(WHAT);
        let mut modules = Vec::new();
        let mut descriptor = table_rva;
        loop {
            let lookup_rva = self.u32(descriptor, WHAT)?;
            let name_rva = self.u32(descriptor.checked_add(12).ok_or(bad.clone())?, WHAT)?;
            let slots_rva = self.u32(descriptor.checked_add(16).ok_or(bad.clone())?, WHAT)?;
            if name_rva == 0 && slots_rva == 0 {
                return Ok(modules);
            }
            if name_rva == 0 || slots_rva == 0 {
                return Err(bad);
            }
            // The lookup table names the imports. Old linkers leave it out and put the names
            // in the address table itself.
            let names_rva = if lookup_rva != 0 {
                lookup_rva
            } else {
                slots_rva
            };
            let mut imports = Vec::new();
            for index in 0u32.. {
                let step = index.checked_mul(8).ok_or(bad.clone())?;
                let entry_rva = names_rva.checked_add(step).ok_or(bad.clone())?;
                let slot_rva = slots_rva.checked_add(step).ok_or(bad.clone())?;
                let entry = self.u64(entry_rva, WHAT)?;
                if entry == 0 {
                    break;
                }
                if u64::from(slot_rva) + 8 > u64::from(self.image.size) {
                    return Err(bad);
                }
                let symbol = if entry >> 63 != 0 {
                    Symbol::Ordinal(entry as u16)
                } else {
                    let hint_rva = u32::try_from(entry).map_err(|_| bad.clone())?;
                    let name_at = hint_rva.checked_add(2).ok_or(bad.clone())?;
                    Symbol::Name(self.name(name_at, "imported function name")?)
                };
                imports.push(Import { symbol, slot_rva });
            }
            modules.push(ImportedModule {
                name: self.name(name_rva, "imported module name")?,
                imports,
            });
            descriptor = descriptor.checked_add(20).ok_or(bad.clone())?;
        }
    }

    fn relocations(&self, table_rva: u32, table_size: u32) -> Result<Vec<u32>, ParseError> {
        const WHAT: &str = "base relocation table";
        let table = self.get(table_rva, table_size as usize, WHAT)?;
        let mut relocations = Vec::new();
        let mut rest = table;
        while rest.len() >= 8 {
            let page_rva = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
            let block_size = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
            if block_size < 8 || !block_size.is_multiple_of(2) || block_size > rest.len() {
                return Err(ParseError::Malformed(WHAT));
            }
            for entry in rest[8..block_size].chunks_exact(2) {
                let entry = u16::from_le_bytes([entry[0], entry[1]]);
                let kind = (entry >> 12) as u8;
                let rva = page_rva
                    .checked_add(u32::from(entry & 0xFFF))
                    .ok_or(ParseError::Malformed(WHAT))?;
                match kind {
                    // Padding that rounds a block up to four bytes.
                    RELOCATION_ABSOLUTE => {}
                    RELOCATION_DIR64 => {
                        if u64::from(rva) + 8 > u64::from(self.image.size) {
                            return Err(ParseError::Malformed(WHAT));
                        }
                        relocations.push(rva);
                    }
                    kind => return Err(ParseError::UnsupportedRelocation { kind, rva }),
                }
            }
            rest = &rest[block_size..];
        }
        Ok(relocations)
    }

    fn tls(&self, directory_rva: u32) -> Result<Tls, ParseError> {
        const WHAT: &str = "TLS directory";
        let directory = self.get(directory_rva, 40, WHAT)?;
        let u64_at = |at: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&directory[at..at + 8]);
            u64::from_le_bytes(b)
        };
        let (start, end, index, callbacks) = (u64_at(0), u64_at(8), u64_at(16), u64_at(24));
        let zero_fill = u64_at(32) as u32;

        let template_size = end.checked_sub(start).ok_or(ParseError::Malformed(WHAT))?;
        let template_size =
            u32::try_from(template_size).map_err(|_| ParseError::Malformed(WHAT))?;
        let template_rva = if template_size == 0 {
            0
        } else {
            let rva = self.rva_of(start, WHAT)?;
            if u64::from(rva) + u64::from(template_size) > u64::from(self.image.size) {
                return Err(ParseError::Malformed(WHAT));
            }
            rva
        };
        let index_rva = self.rva_of(index, WHAT)?;
        if u64::from(index_rva) + 4 > u64::from(self.image.size) {
            return Err(ParseError::Malformed(WHAT));
        }
        let callbacks_rva = match callbacks {
            0 => None,
            address => Some(self.rva_of(address, WHAT)?),
        };
        Ok(Tls {
            template_rva,
            template_size,
            zero_fill,
            index_rva,
            callbacks_rva,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COFF: usize = 0x84;
    const OPTIONAL: usize = COFF + 20;
    const SECTION: usize = OPTIONAL + 240;

    fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
        bytes[at..at + value.len()].copy_from_slice(value);
    }

    /// The smallest image this parser accepts: headers, one executable section, no tables.
    fn minimal() -> Vec<u8> {
        let mut b = vec![0u8; 0x400];
        put(&mut b, 0, b"MZ");
        put(&mut b, 0x3C, &0x80u32.to_le_bytes());
        put(&mut b, 0x80, b"PE\0\0");
        put(&mut b, COFF, &MACHINE_AMD64.to_le_bytes());
        put(&mut b, COFF + 2, &1u16.to_le_bytes());
        put(&mut b, COFF + 16, &240u16.to_le_bytes());
        put(&mut b, OPTIONAL, &MAGIC_PE32_PLUS.to_le_bytes());
        put(&mut b, OPTIONAL + 16, &0x1000u32.to_le_bytes());
        put(&mut b, OPTIONAL + 24, &0x1_4000_0000u64.to_le_bytes());
        put(&mut b, OPTIONAL + 32, &0x1000u32.to_le_bytes());
        put(&mut b, OPTIONAL + 36, &0x200u32.to_le_bytes());
        put(&mut b, OPTIONAL + 56, &0x2000u32.to_le_bytes());
        put(&mut b, OPTIONAL + 60, &0x200u32.to_le_bytes());
        put(&mut b, OPTIONAL + 108, &16u32.to_le_bytes());
        put(&mut b, SECTION, b".text");
        put(&mut b, SECTION + 8, &0x10u32.to_le_bytes());
        put(&mut b, SECTION + 12, &0x1000u32.to_le_bytes());
        put(&mut b, SECTION + 16, &0x200u32.to_le_bytes());
        put(&mut b, SECTION + 20, &0x200u32.to_le_bytes());
        put(
            &mut b,
            SECTION + 36,
            &(SECTION_EXECUTE | SECTION_READ).to_le_bytes(),
        );
        b
    }

    #[test]
    fn the_minimal_image_parses() {
        let image = Image::parse(&minimal()).unwrap();
        assert_eq!(image.preferred_base, 0x1_4000_0000);
        assert_eq!(image.size, 0x2000);
        assert_eq!(image.entry_rva, 0x1000);
        assert_eq!(image.sections.len(), 1);
        assert_eq!(image.sections[0].name, ".text");
        assert!(image.sections[0].executable && !image.sections[0].writable);
        assert!(image.imports.is_empty() && image.relocations.is_empty());
        assert_eq!(image.tls, None);
    }

    #[test]
    fn an_empty_file_is_truncated() {
        assert_eq!(Image::parse(&[]), Err(ParseError::Truncated("DOS header")));
    }

    #[test]
    fn a_file_that_is_not_an_image_is_refused() {
        let text = b"this is a text file, long enough to hold a DOS header if it were one....";
        assert_eq!(Image::parse(text), Err(ParseError::BadSignature("MZ")));
    }

    #[test]
    fn a_header_offset_past_the_end_is_refused() {
        let mut bytes = minimal();
        put(&mut bytes, 0x3C, &0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(
            Image::parse(&bytes),
            Err(ParseError::Truncated("PE header"))
        );
    }

    #[test]
    fn an_image_for_another_cpu_is_refused() {
        let mut bytes = minimal();
        put(&mut bytes, COFF, &0xAA64u16.to_le_bytes());
        assert_eq!(Image::parse(&bytes), Err(ParseError::WrongMachine(0xAA64)));
    }

    #[test]
    fn a_32_bit_image_is_refused() {
        let mut bytes = minimal();
        put(&mut bytes, OPTIONAL, &0x10Bu16.to_le_bytes());
        assert_eq!(Image::parse(&bytes), Err(ParseError::NotPe32Plus(0x10B)));
    }

    #[test]
    fn a_section_outside_the_image_is_refused() {
        let mut bytes = minimal();
        put(&mut bytes, SECTION + 8, &0x5000u32.to_le_bytes());
        assert_eq!(
            Image::parse(&bytes),
            Err(ParseError::Malformed("section placement"))
        );
    }

    #[test]
    fn section_data_past_the_end_of_the_file_is_refused() {
        let mut bytes = minimal();
        put(&mut bytes, SECTION + 20, &0x300u32.to_le_bytes());
        assert_eq!(
            Image::parse(&bytes),
            Err(ParseError::Truncated("section data"))
        );
    }

    #[test]
    fn an_entry_point_outside_code_is_refused() {
        let mut bytes = minimal();
        put(&mut bytes, OPTIONAL + 16, &0x100u32.to_le_bytes());
        assert_eq!(
            Image::parse(&bytes),
            Err(ParseError::Malformed("entry point"))
        );
    }

    #[test]
    fn a_directory_pointing_outside_the_file_is_refused() {
        let mut bytes = minimal();
        let import = OPTIONAL + 112 + DIRECTORY_IMPORT * 8;
        put(&mut bytes, import, &0x1F00u32.to_le_bytes());
        put(&mut bytes, import + 4, &0x28u32.to_le_bytes());
        assert_eq!(
            Image::parse(&bytes),
            Err(ParseError::Malformed("import table"))
        );
    }

    #[test]
    fn a_relocation_of_another_kind_is_refused() {
        let mut bytes = minimal();
        let reloc = OPTIONAL + 112 + DIRECTORY_BASE_RELOCATION * 8;
        put(&mut bytes, reloc, &0x1100u32.to_le_bytes());
        put(&mut bytes, reloc + 4, &12u32.to_le_bytes());
        // One block for page 0x1000: a DIR64 entry and one of kind 3 (HIGHLOW, 32-bit only).
        let block = 0x200 + 0x100;
        put(&mut bytes, block, &0x1000u32.to_le_bytes());
        put(&mut bytes, block + 4, &12u32.to_le_bytes());
        put(&mut bytes, block + 8, &0xA008u16.to_le_bytes());
        put(&mut bytes, block + 10, &0x3010u16.to_le_bytes());
        assert_eq!(
            Image::parse(&bytes),
            Err(ParseError::UnsupportedRelocation {
                kind: 3,
                rva: 0x1010
            })
        );
        put(&mut bytes, block + 10, &0u16.to_le_bytes());
        assert_eq!(Image::parse(&bytes).unwrap().relocations, [0x1008]);
    }

    #[test]
    fn every_truncation_of_the_minimal_image_is_an_error_not_a_panic() {
        let bytes = minimal();
        for length in 0..bytes.len() {
            assert!(Image::parse(&bytes[..length]).is_err(), "length {length}");
        }
    }

    #[test]
    fn no_single_corrupt_header_byte_makes_the_parser_panic() {
        let bytes = minimal();
        for index in 0..0x200 {
            for value in [0x00, 0x7F, 0xFF] {
                let mut corrupt = bytes.clone();
                corrupt[index] = value;
                let _ = Image::parse(&corrupt);
            }
        }
    }
}
