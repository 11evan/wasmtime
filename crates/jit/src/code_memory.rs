//! Memory management for executable code.

use crate::subslice_range;
use crate::unwind::UnwindRegistration;
use anyhow::{anyhow, bail, Context, Result};
use object::read::{File, Object, ObjectSection};
use std::mem;
use std::mem::ManuallyDrop;
use std::ops::Range;
use wasmtime_environ::obj;
use wasmtime_environ::FunctionLoc;
use wasmtime_jit_icache_coherence as icache_coherence;
use wasmtime_runtime::{MmapVec, VMTrampoline};

/// Management of executable memory within a `MmapVec`
///
/// This type consumes ownership of a region of memory and will manage the
/// executable permissions of the contained JIT code as necessary.
pub struct CodeMemory {
    // NB: these are `ManuallyDrop` because `unwind_registration` must be
    // dropped first since it refers to memory owned by `mmap`.
    mmap: ManuallyDrop<MmapVec>,
    unwind_registration: ManuallyDrop<Option<UnwindRegistration>>,
    published: bool,
    enable_branch_protection: bool,

    // Ranges within `self.mmap` of where the particular sections lie.
    text: Range<usize>,
    unwind: Range<usize>,
    trap_data: Range<usize>,
    wasm_data: Range<usize>,
    address_map_data: Range<usize>,
    func_name_data: Range<usize>,
    info_data: Range<usize>,

    /// Map of dwarf sections indexed by `gimli::SectionId` which points to the
    /// range within `code_memory`'s mmap as to the contents of the section.
    dwarf_sections: Vec<Range<usize>>,
}

impl Drop for CodeMemory {
    fn drop(&mut self) {
        // Drop `unwind_registration` before `self.mmap`
        unsafe {
            ManuallyDrop::drop(&mut self.unwind_registration);
            ManuallyDrop::drop(&mut self.mmap);
        }
    }
}

fn _assert() {
    fn _assert_send_sync<T: Send + Sync>() {}
    _assert_send_sync::<CodeMemory>();
}

impl CodeMemory {
    /// Creates a new `CodeMemory` by taking ownership of the provided
    /// `MmapVec`.
    ///
    /// The returned `CodeMemory` manages the internal `MmapVec` and the
    /// `publish` method is used to actually make the memory executable.
    pub fn new(mmap: MmapVec) -> Result<Self> {
        use gimli::SectionId::*;

        let obj = File::parse(&mmap[..])
            .with_context(|| "failed to parse internal compilation artifact")?;

        let mut text = 0..0;
        let mut unwind = 0..0;
        let mut enable_branch_protection = None;
        let mut trap_data = 0..0;
        let mut wasm_data = 0..0;
        let mut address_map_data = 0..0;
        let mut func_name_data = 0..0;
        let mut info_data = 0..0;
        let mut dwarf_sections = Vec::new();
        for section in obj.sections() {
            let data = section.data()?;
            let name = section.name()?;
            let range = subslice_range(data, &mmap);

            // Double-check that sections are all aligned properly.
            if section.align() != 0 && data.len() != 0 {
                if (data.as_ptr() as u64 - mmap.as_ptr() as u64) % section.align() != 0 {
                    bail!(
                        "section `{}` isn't aligned to {:#x}",
                        section.name().unwrap_or("ERROR"),
                        section.align()
                    );
                }
            }

            let mut gimli = |id: gimli::SectionId| {
                let idx = id as usize;
                if dwarf_sections.len() <= idx {
                    dwarf_sections.resize(idx + 1, 0..0);
                }
                dwarf_sections[idx] = range.clone();
            };

            match name {
                obj::ELF_WASM_BTI => match data.len() {
                    1 => enable_branch_protection = Some(data[0] != 0),
                    _ => bail!("invalid `{name}` section"),
                },
                ".text" => {
                    text = range;

                    // Double-check there are no relocations in the text section. At
                    // this time relocations are not expected at all from loaded code
                    // since everything should be resolved at compile time. Handling
                    // must be added here, though, if relocations pop up.
                    assert!(section.relocations().count() == 0);
                }
                UnwindRegistration::SECTION_NAME => unwind = range,
                obj::ELF_WASM_DATA => wasm_data = range,
                obj::ELF_WASMTIME_ADDRMAP => address_map_data = range,
                obj::ELF_WASMTIME_TRAPS => trap_data = range,
                obj::ELF_NAME_DATA => func_name_data = range,
                obj::ELF_WASMTIME_INFO => info_data = range,

                // Register dwarf sections into the `dwarf_sections`
                // array which is indexed by `gimli::SectionId`
                ".debug_abbrev.wasm" => gimli(DebugAbbrev),
                ".debug_addr.wasm" => gimli(DebugAddr),
                ".debug_aranges.wasm" => gimli(DebugAranges),
                ".debug_frame.wasm" => gimli(DebugFrame),
                ".eh_frame.wasm" => gimli(EhFrame),
                ".eh_frame_hdr.wasm" => gimli(EhFrameHdr),
                ".debug_info.wasm" => gimli(DebugInfo),
                ".debug_line.wasm" => gimli(DebugLine),
                ".debug_line_str.wasm" => gimli(DebugLineStr),
                ".debug_loc.wasm" => gimli(DebugLoc),
                ".debug_loc_lists.wasm" => gimli(DebugLocLists),
                ".debug_macinfo.wasm" => gimli(DebugMacinfo),
                ".debug_macro.wasm" => gimli(DebugMacro),
                ".debug_pub_names.wasm" => gimli(DebugPubNames),
                ".debug_pub_types.wasm" => gimli(DebugPubTypes),
                ".debug_ranges.wasm" => gimli(DebugRanges),
                ".debug_rng_lists.wasm" => gimli(DebugRngLists),
                ".debug_str.wasm" => gimli(DebugStr),
                ".debug_str_offsets.wasm" => gimli(DebugStrOffsets),
                ".debug_types.wasm" => gimli(DebugTypes),
                ".debug_cu_index.wasm" => gimli(DebugCuIndex),
                ".debug_tu_index.wasm" => gimli(DebugTuIndex),

                _ => log::debug!("ignoring section {name}"),
            }
        }
        Ok(Self {
            mmap: ManuallyDrop::new(mmap),
            unwind_registration: ManuallyDrop::new(None),
            published: false,
            enable_branch_protection: enable_branch_protection
                .ok_or_else(|| anyhow!("missing `{}` section", obj::ELF_WASM_BTI))?,
            text,
            unwind,
            trap_data,
            address_map_data,
            func_name_data,
            dwarf_sections,
            info_data,
            wasm_data,
        })
    }

    /// Returns a reference to the underlying `MmapVec` this memory owns.
    pub fn mmap(&self) -> &MmapVec {
        &self.mmap
    }

    /// Returns the contents of the text section of the ELF executable this
    /// represents.
    pub fn text(&self) -> &[u8] {
        &self.mmap[self.text.clone()]
    }

    /// Returns the data in the corresponding dwarf section, or an empty slice
    /// if the section wasn't present.
    pub fn dwarf_section(&self, section: gimli::SectionId) -> &[u8] {
        let range = self
            .dwarf_sections
            .get(section as usize)
            .cloned()
            .unwrap_or(0..0);
        &self.mmap[range]
    }

    /// Returns the data in the `ELF_NAME_DATA` section.
    pub fn func_name_data(&self) -> &[u8] {
        &self.mmap[self.func_name_data.clone()]
    }

    /// Returns the concatenated list of all data associated with this wasm
    /// module.
    ///
    /// This is used for initialization of memories and all data ranges stored
    /// in a `Module` are relative to the slice returned here.
    pub fn wasm_data(&self) -> &[u8] {
        &self.mmap[self.wasm_data.clone()]
    }

    /// Returns the encoded address map section used to pass to
    /// `wasmtime_environ::lookup_file_pos`.
    pub fn address_map_data(&self) -> &[u8] {
        &self.mmap[self.address_map_data.clone()]
    }

    /// Returns the contents of the `ELF_WASMTIME_INFO` section, or an empty
    /// slice if it wasn't found.
    pub fn wasmtime_info(&self) -> &[u8] {
        &self.mmap[self.info_data.clone()]
    }

    /// Returns the contents of the `ELF_WASMTIME_TRAPS` section, or an empty
    /// slice if it wasn't found.
    pub fn trap_data(&self) -> &[u8] {
        &self.mmap[self.trap_data.clone()]
    }

    /// Returns a `VMTrampoline` function pointer for the given function in the
    /// text section.
    ///
    /// # Unsafety
    ///
    /// This function is unsafe as there's no guarantee that the returned
    /// function pointer is valid.
    pub unsafe fn vmtrampoline(&self, loc: FunctionLoc) -> VMTrampoline {
        let ptr = self.text()[loc.start as usize..][..loc.length as usize].as_ptr();
        mem::transmute::<*const u8, VMTrampoline>(ptr)
    }

    /// Publishes the internal ELF image to be ready for execution.
    ///
    /// This method can only be called once and will panic if called twice. This
    /// will parse the ELF image from the original `MmapVec` and do everything
    /// necessary to get it ready for execution, including:
    ///
    /// * Change page protections from read/write to read/execute.
    /// * Register unwinding information with the OS
    ///
    /// After this function executes all JIT code should be ready to execute.
    pub fn publish(&mut self) -> Result<()> {
        assert!(!self.published);
        self.published = true;

        if self.text().is_empty() {
            return Ok(());
        }

        // The unsafety here comes from a few things:
        //
        // * We're actually updating some page protections to executable memory.
        //
        // * We're registering unwinding information which relies on the
        //   correctness of the information in the first place. This applies to
        //   both the actual unwinding tables as well as the validity of the
        //   pointers we pass in itself.
        unsafe {
            let text = self.text();

            // Clear the newly allocated code from cache if the processor requires it
            //
            // Do this before marking the memory as R+X, technically we should be able to do it after
            // but there are some CPU's that have had errata about doing this with read only memory.
            icache_coherence::clear_cache(text.as_ptr().cast(), text.len())
                .expect("Failed cache clear");

            // Switch the executable portion from read/write to
            // read/execute, notably not using read/write/execute to prevent
            // modifications.
            self.mmap
                .make_executable(self.text.clone(), self.enable_branch_protection)
                .expect("unable to make memory executable");

            // Flush any in-flight instructions from the pipeline
            icache_coherence::pipeline_flush_mt().expect("Failed pipeline flush");

            // With all our memory set up use the platform-specific
            // `UnwindRegistration` implementation to inform the general
            // runtime that there's unwinding information available for all
            // our just-published JIT functions.
            self.register_unwind_info()?;
        }

        Ok(())
    }

    unsafe fn register_unwind_info(&mut self) -> Result<()> {
        if self.unwind.len() == 0 {
            return Ok(());
        }
        let text = self.text();
        let unwind_info = &self.mmap[self.unwind.clone()];
        let registration =
            UnwindRegistration::new(text.as_ptr(), unwind_info.as_ptr(), unwind_info.len())
                .context("failed to create unwind info registration")?;
        *self.unwind_registration = Some(registration);
        Ok(())
    }
}
