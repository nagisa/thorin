use std::collections::HashMap;

use gimli::{
    write::{EndianVec, Writer},
    DebugStrOffsetsBase, DebugStrOffsetsIndex, DwarfFileType, Encoding, EndianSlice, Format,
};
use indexmap::IndexSet;
use tracing::debug;

use crate::{
    error::{Error, Result},
    ext::PackageFormatExt,
};

/// New-type'd index from `IndexVec` of strings inserted into the `.debug_str` section.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PackageStringId(usize);

/// New-type'd offset into `.debug_str` section.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PackageStringOffset(usize);

/// DWARF packages need to merge the `.debug_str` sections of input DWARF objects.
/// `.debug_str_offsets` sections then need to be rebuilt with offsets into the new merged
/// `.debug_str` section and then concatenated (indices into each dwarf object's offset list will
/// therefore still refer to the same string).
///
/// Gimli's `StringTable` produces a `.debug_str` section with a single `.debug_str_offsets`
/// section, but `PackageStringTable` accumulates a single `.debug_str` section and can be used to
/// produce multiple `.debug_str_offsets` sections (which will be concatenated) which all offset
/// into the same `.debug_str`.
pub(crate) struct PackageStringTable<E: gimli::Endianity> {
    data: EndianVec<E>,
    strings: IndexSet<Vec<u8>>,
    offsets: HashMap<PackageStringId, PackageStringOffset>,
}

impl<E: gimli::Endianity> PackageStringTable<E> {
    /// Create a new `PackageStringTable` with a given endianity.
    pub(crate) fn new(endianness: E) -> Self {
        Self { data: EndianVec::new(endianness), strings: IndexSet::new(), offsets: HashMap::new() }
    }

    /// Insert a string into the string table and return its offset in the table. If the string is
    /// already in the table, returns its offset.
    pub(crate) fn get_or_insert<T: Into<Vec<u8>>>(
        &mut self,
        bytes: T,
    ) -> Result<PackageStringOffset> {
        let bytes = bytes.into();
        assert!(!bytes.contains(&0));
        let (index, is_new) = self.strings.insert_full(bytes.clone());
        let index = PackageStringId(index);
        if !is_new {
            return Ok(*self.offsets.get(&index).expect("insert exists but no offset"));
        }

        // Keep track of the offset for this string, it might be referenced by the next compilation
        // unit too.
        let offset = PackageStringOffset(self.data.len());
        self.offsets.insert(index, offset);

        // Insert into the string table.
        self.data.write(&bytes)?;
        self.data.write_u8(0)?;

        Ok(offset)
    }

    /// Adds strings from input `.debug_str_offsets` and `.debug_str` into the string table, returns
    /// data for a equivalent `.debug_str_offsets` section with offsets pointing into the new
    /// `.debug_str` section.
    pub(crate) fn remap_str_offsets_section(
        &mut self,
        debug_str: gimli::DebugStr<EndianSlice<E>>,
        debug_str_offsets: gimli::DebugStrOffsets<EndianSlice<E>>,
        section_size: u64,
        endian: E,
        encoding: Encoding,
    ) -> Result<EndianVec<E>> {
        let entry_size = match encoding.format {
            Format::Dwarf32 => 4,
            Format::Dwarf64 => 8,
        };

        let mut data = EndianVec::new(endian);

        // `DebugStrOffsetsBase` knows to skip past the header with DWARF 5.
        let base: gimli::DebugStrOffsetsBase<usize> =
            DebugStrOffsetsBase::default_for_encoding_and_file(encoding, DwarfFileType::Dwo);

        if encoding.is_std_dwarf_package_format() {
            match encoding.format {
                Format::Dwarf32 => {
                    // Unit length (4 bytes): size of the offsets section without this
                    // header (8 bytes total).
                    data.write_u32(
                        (section_size - 8)
                            .try_into()
                            .expect("section size w/out header larger than u32"),
                    )?;
                }
                Format::Dwarf64 => {
                    // Unit length (4 bytes then 8 bytes): size of the offsets section without
                    // this header (16 bytes total).
                    data.write_u32(u32::MAX)?;
                    data.write_u64(section_size - 16)?;
                }
            };
            // Version (2 bytes): DWARF 5
            data.write_u16(5)?;
            // Reserved padding (2 bytes)
            data.write_u16(0)?;
        }
        debug!(?base);

        let base_offset: u64 = base.0.try_into().expect("base offset larger than u64");
        let num_elements = (section_size - base_offset) / entry_size;
        debug!(?section_size, ?base_offset, ?num_elements);

        for i in 0..num_elements {
            let dwo_index = DebugStrOffsetsIndex(i as usize);
            let dwo_offset = debug_str_offsets
                .get_str_offset(encoding.format, base, dwo_index)
                .map_err(|e| Error::OffsetAtIndex(e, i))?;
            let dwo_str =
                debug_str.get_str(dwo_offset).map_err(|e| Error::StrAtOffset(e, dwo_offset.0))?;
            let dwo_str = dwo_str.to_string()?;

            let dwp_offset = self.get_or_insert(dwo_str)?;

            match encoding.format {
                Format::Dwarf32 => {
                    let dwp_offset =
                        dwp_offset.0.try_into().expect("string offset larger than u32");
                    data.write_u32(dwp_offset)?;
                }
                Format::Dwarf64 => {
                    let dwp_offset =
                        dwp_offset.0.try_into().expect("string offset larger than u64");
                    data.write_u64(dwp_offset)?;
                }
            }
        }

        Ok(data)
    }

    /// Returns the accumulated `.debug_str` section data
    pub(crate) fn finish(self) -> EndianVec<E> {
        self.data
    }
}
