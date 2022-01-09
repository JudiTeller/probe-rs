//! Debugging support for probe-rs
//!
//! The `debug` module contains various debug functionality, which can be
//! used to implement a debugger based on `probe-rs`.

mod variable;

use crate::{core::Core, MemoryInterface};
use num_traits::Zero;
use probe_rs_target::Architecture;
pub use variable::{Variable, VariableInclusion, VariableKind, VariantRole};

use std::{
    borrow,
    collections::HashMap,
    io,
    num::NonZeroU64,
    path::{Path, PathBuf},
    rc::Rc,
    str::{from_utf8, Utf8Error},
};

use gimli::{
    DW_AT_abstract_origin, DebuggingInformationEntry, FileEntry, LineProgramHeader, Location,
    UnitOffset,
};
use object::read::{Object, ObjectSection};

#[derive(Debug, thiserror::Error)]
pub enum DebugError {
    #[error("IO Error while accessing debug data")]
    Io(#[from] io::Error),
    #[error("Error accessing debug data")]
    DebugData(#[from] object::read::Error),
    #[error("Error parsing debug data")]
    Parse(#[from] gimli::read::Error),
    #[error("Non-UTF8 data found in debug data")]
    NonUtf8(#[from] Utf8Error),
    #[error("Error using the probe")]
    Probe(#[from] crate::Error),
    #[error(transparent)]
    CharConversion(#[from] std::char::CharTryFromError),
    #[error(transparent)]
    IntConversion(#[from] std::num::TryFromIntError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum ColumnType {
    LeftEdge,
    Column(u64),
}

impl From<gimli::ColumnType> for ColumnType {
    fn from(column: gimli::ColumnType) -> Self {
        match column {
            gimli::ColumnType::LeftEdge => ColumnType::LeftEdge,
            gimli::ColumnType::Column(c) => ColumnType::Column(c.get()),
        }
    }
}

#[derive(Debug)]
pub struct StackFrame {
    pub id: u64,
    pub function_name: String,
    pub source_location: Option<SourceLocation>,
    pub registers: Registers,
    pub pc: u32,
    // variables that are in scope between the start of the frame and the current `pc`.
    pub variables: Vec<Variable>,
}

impl std::fmt::Display for StackFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        writeln!(f, "{}: {}", self.id, self.function_name)?;
        if let Some(si) = &self.source_location {
            write!(
                f,
                "\t{}/{}",
                si.directory
                    .as_ref()
                    .map(|p| p.to_string_lossy())
                    .unwrap_or_else(|| std::borrow::Cow::from("<unknown dir>")),
                si.file.as_ref().unwrap_or(&"<unknown file>".to_owned())
            )?;

            if let (Some(column), Some(line)) = (si.column, si.line) {
                match column {
                    ColumnType::Column(c) => write!(f, ":{}:{}", line, c)?,
                    ColumnType::LeftEdge => write!(f, ":{}", line)?,
                }
            }
        }

        writeln!(f)?;
        writeln!(f, "\tVariables:")?;

        for variable in &self.variables {
            variable_recurse(variable, 0, f)?;
        }
        write!(f, "")
    }
}

fn variable_recurse(
    variable: &Variable,
    level: u32,
    f: &mut std::fmt::Formatter,
) -> std::fmt::Result {
    for _depth in 0..level {
        write!(f, "   ")?;
    }
    let new_level = level + 1;
    let ret = writeln!(f, "|-> {} \t= {}", variable.name, variable.get_value());
    if let Some(children) = variable.children.clone() {
        for variable in &children {
            variable_recurse(variable, new_level, f)?;
        }
    }

    ret
}
#[derive(Debug, Clone)]
pub struct Registers {
    values: HashMap<u32, u32>,

    architecture: Architecture,
}

impl Registers {
    /// Read all registers from the given core.
    pub fn from_core(core: &mut Core) -> Self {
        let register_file = core.registers();

        let num_platform_registers = register_file.platform_registers.len();

        let mut registers = Registers {
            values: HashMap::new(),
            architecture: core.architecture(),
        };

        for i in 0..num_platform_registers {
            match core.read_core_reg(register_file.platform_register(i)) {
                Ok(value) => registers.values.insert(i as u32, value),
                Err(e) => {
                    log::debug!("Failed to read value for register {}: {}", i, e);
                    None
                }
            };
        }
        registers
    }

    /// Get the canonical frame address, as specified in the [DWARF] specification, section 6.4.
    ///
    /// For ARM, this is the value of the stack pointer at the call site in the previous frame.
    ///
    /// [DWARF]: https://dwarfstd.org
    pub fn get_canonical_frame_address(&self) -> Option<u32> {
        match self.architecture {
            Architecture::Arm => self.values.get(&13).copied(),
            Architecture::Avr => todo!("Not implemented for AVR yet"),
            Architecture::Riscv => self.values.get(&2).copied(),
        }
    }

    /// Set the canonical frame address, as specified in the [DWARF] specification, section 6.4.
    ///
    /// For ARM, this is the value of the stack pointer at the call site in the previous frame.
    ///
    /// [DWARF]: https://dwarfstd.org
    pub fn set_canonical_frame_address(&mut self, value: Option<u32>) {
        let register_address = match self.architecture {
            Architecture::Arm => 13,
            Architecture::Avr => todo!("Not implemented for AVR yet"),
            Architecture::Riscv => 2,
        };

        if let Some(value) = value {
            self.values.insert(register_address, value);
        } else {
            self.values.remove(&register_address);
        }
    }

    pub fn get_program_counter(&self) -> Option<u32> {
        match self.architecture {
            Architecture::Arm => self.values.get(&14).copied(),
            Architecture::Avr => todo!("Not implemented for AVR yet"),
            Architecture::Riscv => self.values.get(&1).copied(),
        }
    }

    pub fn get_return_address(&self) -> Option<u32> {
        match self.architecture {
            Architecture::Arm => self.values.get(&15).copied(),
            Architecture::Avr => todo!("Not implemented for AVR yet"),
            Architecture::Riscv => self.values.get(&1).copied(),
        }
    }

    pub fn get_by_dwarf_register_number(&self, register_number: u32) -> Option<u32> {
        self.values.get(&register_number).copied()
    }

    pub fn set_by_dwarf_register_number(&mut self, register_number: u32, value: Option<u32>) {
        if let Some(value) = value {
            self.values.insert(register_number, value);
        } else {
            self.values.remove(&register_number);
        }
    }

    pub fn registers(&self) -> impl Iterator<Item = (&u32, &u32)> {
        self.values.iter()
    }
}

#[derive(Debug, PartialEq)]
pub struct SourceLocation {
    pub line: Option<u64>,
    pub column: Option<ColumnType>,

    pub file: Option<String>,
    pub directory: Option<PathBuf>,
}

#[derive(Debug, Clone)]
enum InlineFunctionState {
    /// We are at the state where the function was inlined.
    InlinedCallSite {
        call_line: Option<u64>,
        call_file: Option<String>,
        call_directory: Option<PathBuf>,
        call_column: Option<u64>,
    },
    /// Not handling anything related to inlining.
    NoInlining,
}

pub struct StackFrameIterator<'debuginfo, 'probe, 'core> {
    debug_info: &'debuginfo DebugInfo,
    core: &'core mut Core<'probe>,
    frame_count: u64,

    /// Current program counter. If set to `None`, this indicates that no further stack frame can be determined.
    pc: Option<u64>,
    registers: Registers,
    inlining_state: InlineFunctionState,
}

impl<'debuginfo, 'probe, 'core> StackFrameIterator<'debuginfo, 'probe, 'core> {
    pub fn new(
        debug_info: &'debuginfo DebugInfo,
        core: &'core mut Core<'probe>,
        address: u64,
    ) -> Self {
        let registers = Registers::from_core(core);
        let pc = address;
        Self {
            debug_info,
            core,
            frame_count: 0,
            pc: Some(pc),
            registers,
            inlining_state: InlineFunctionState::NoInlining,
        }
    }
}

impl<'debuginfo, 'probe, 'core> Iterator for StackFrameIterator<'debuginfo, 'probe, 'core> {
    type Item = StackFrame;

    fn next(&mut self) -> Option<Self::Item> {
        let pc = match self.pc {
            Some(pc) => pc,
            None => {
                log::debug!("Unable to determine next frame, program counter is unknown");
                return None;
            }
        };

        use gimli::UnwindSection;
        let mut ctx = Box::new(gimli::UnwindContext::new());
        let bases = gimli::BaseAddresses::default();

        log::debug!("StackFrame: Unwinding at address {:#010x}", pc);

        // Find function information, to check if we are in an inlined function.

        let inline_call_site = match self.inlining_state {
            InlineFunctionState::InlinedCallSite { .. } => true,
            InlineFunctionState::NoInlining => false,
        };

        if inline_call_site {
            log::debug!("At call site of inlined function.");
        }

        let mut in_inlined_function = false;

        let mut inline_call_site_info = None;

        if !inline_call_site {
            let mut units = self.debug_info.get_units();
            while let Some(unit_info) = self.debug_info.get_next_unit_info(&mut units) {
                if let Some(die_cursor_state) = &mut unit_info.get_function_die(pc, true) {
                    if die_cursor_state.is_inline {
                        // Add a 'virtual' stack frame, for the inlined call.
                        // For this, we need the following attributes:
                        //
                        // - DW_AT_call_file
                        // - DW_AT_call_line
                        // - DW_AT_call_column

                        let call_column = die_cursor_state
                            .get_attribute(gimli::DW_AT_call_column)
                            .and_then(|attr| attr.udata_value());

                        let call_file_index = die_cursor_state
                            .get_attribute(gimli::DW_AT_call_file)
                            .and_then(|attr| attr.udata_value());

                        let call_line = die_cursor_state
                            .get_attribute(gimli::DW_AT_call_line)
                            .and_then(|attr| attr.udata_value());

                        let (call_file, call_directory) = match call_file_index {
                            Some(0) => (None, None),
                            Some(n) => {
                                // Lookup source file in the line number information table.

                                if let Some(header) = unit_info
                                    .unit
                                    .line_program
                                    .as_ref()
                                    .map(|line_program| line_program.header())
                                {
                                    if let Some(file_entry) = header.file(n) {
                                        self.debug_info
                                            .find_file_and_directory(
                                                &unit_info.unit,
                                                header,
                                                file_entry,
                                            )
                                            .unwrap()
                                    } else {
                                        (None, None)
                                    }
                                } else {
                                    (None, None)
                                }
                            }
                            None => (None, None),
                        };

                        self.inlining_state = InlineFunctionState::InlinedCallSite {
                            call_column,
                            call_file,
                            call_line,
                            call_directory,
                        };

                        log::debug!(
                            "Current function {:?} is inlined at: {:?}",
                            die_cursor_state.function_name(&unit_info),
                            self.inlining_state
                        );
                        in_inlined_function = true;
                    } else {
                        // No inlined function
                    }
                    break;
                }
            }
        } else {
            inline_call_site_info = Some(std::mem::replace(
                &mut self.inlining_state,
                InlineFunctionState::NoInlining,
            ));
        };

        let unwind_info = self.debug_info.frame_section.unwind_info_for_address(
            &bases,
            &mut ctx,
            pc,
            gimli::DebugFrame::cie_from_offset,
        );

        let unwind_info = match unwind_info {
            Ok(uw) => uw,
            Err(e) => {
                log::info!(
                    "Failed to retrieve debug information for program counter {:#x}: {}",
                    pc,
                    e
                );
                return None;
            }
        };

        let current_cfa = match unwind_info.cfa() {
            gimli::CfaRule::RegisterAndOffset { register, offset } => {
                let reg_val = self
                    .registers
                    .get_by_dwarf_register_number(register.0 as u32);

                match reg_val {
                    Some(reg_val) => Some((i64::from(reg_val) + offset) as u32),
                    None => {
                        log::warn!(
                            "Unable to calculate CFA: Missing value of register {}",
                            register.0
                        );
                        return None;
                    }
                }
            }
            gimli::CfaRule::Expression(_) => unimplemented!(),
        };

        if let Some(ref cfa) = &current_cfa {
            log::debug!("Current CFA: {:#x}", cfa);
        }

        let return_frame = match self.debug_info.get_stackframe_info(
            self.core,
            pc,
            self.frame_count,
            self.registers.clone(),
            in_inlined_function,
        ) {
            Ok(mut frame) => {
                if let Some(InlineFunctionState::InlinedCallSite {
                    call_line,
                    call_column,
                    call_file,
                    call_directory,
                }) = inline_call_site_info
                {
                    // Update location to match call site

                    frame.source_location = Some(SourceLocation {
                        line: call_line,
                        column: call_column.map(|c| {
                            if c == 0 {
                                ColumnType::LeftEdge
                            } else {
                                ColumnType::Column(c)
                            }
                        }),
                        file: call_file,
                        directory: call_directory,
                    })
                }

                Some(frame)
            }

            Err(e) => {
                log::warn!("Unable to get stack frame information: {}", e);
                None
            }
        };

        if !in_inlined_function {
            // generate previous registers

            // TODO: This is only valid for ARM right now, this should be more general.
            for i in 0..16 {
                // Register 13 is the stack pointer / canonical frame address in ARM.
                // There is a separate rule for this register, see above.
                if i == 13 {
                    continue;
                }

                use gimli::read::RegisterRule::*;

                let register_rule = unwind_info.register(gimli::Register(i as u16));

                log::trace!("Register {}: {:?}", i, &register_rule);

                let new_value = match register_rule {
                    Undefined => {
                        // If we get undefined for the LR register (register 14) or any callee saved register,
                        // we assume that it is unchanged. Gimli doesn't allow us
                        // to distinguish if  a rule is not present or actually set to Undefined
                        // in the call frame information.

                        match i {
                            4 | 5 | 6 | 7 | 8 | 10 | 11 | 14 => {
                                self.registers.get_by_dwarf_register_number(i)
                            }
                            15 => Some(pc as u32),
                            _ => None,
                        }
                    }
                    SameValue => self.registers.get_by_dwarf_register_number(i),
                    Offset(o) => {
                        let addr = i64::from(current_cfa.unwrap()) + o;

                        let mut buff = [0u8; 4];

                        if let Err(e) = self.core.read(addr as u32, &mut buff) {
                            log::info!(
                                "Failed to read from address {:#010x} ({} bytes): {}",
                                addr,
                                4,
                                e
                            );
                            log::debug!(
                                "Rule: Offset {} from address {:#010x}",
                                o,
                                current_cfa.unwrap()
                            );
                            return None;
                        }

                        let val = u32::from_le_bytes(buff);

                        log::debug!("reg[{: >}] @ {:#010x} = {:#08x}", i, addr, val);

                        Some(val)
                    }
                    _ => unimplemented!(),
                };

                self.registers.set_by_dwarf_register_number(i, new_value);
            }

            self.registers.set_canonical_frame_address(current_cfa);
        }

        self.frame_count += 1;

        if !in_inlined_function {
            // Next function is where our current return register is pointing to.
            // We just have to remove the lowest bit (indicator for Thumb mode).
            //
            // We also have to subtract one, as we want the calling instruction for
            // a backtrace, not the next instruction to be executed.

            // TODO: Setting bit to zero is only necessary on ARM, but probably doesn't hurt on other architectures.
            self.pc = self
                .registers
                .get_program_counter()
                .map(|pc| u64::from(pc & !1));

            log::debug!("Called from pc={:#010x?}", self.pc);

            // We need to ensure that the program counter changed, otherwise we are caught in an endless loop

            if let Some(new_pc) = self.pc {
                if new_pc == pc {
                    log::debug!("Program counter did not change while unwinding, aborting.");
                    self.pc = None;
                }
            }
        }

        return_frame
    }
}

type GimliReader = gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>;
type GimliAttribute = gimli::Attribute<GimliReader>;

type DwarfReader = gimli::read::EndianRcSlice<gimli::LittleEndian>;

type FunctionDieType<'abbrev, 'unit> =
    gimli::DebuggingInformationEntry<'abbrev, 'unit, GimliReader, usize>;

type UnitIter =
    gimli::DebugInfoUnitHeadersIter<gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>>;

/// Debug information which is parsed from DWARF debugging information.
pub struct DebugInfo {
    dwarf: gimli::Dwarf<DwarfReader>,
    frame_section: gimli::DebugFrame<DwarfReader>,
}

impl DebugInfo {
    /// Read debug info directly from a ELF file.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<DebugInfo, DebugError> {
        let data = std::fs::read(path)?;

        DebugInfo::from_raw(&data)
    }

    /// Parse debug information directly from a buffer containing an ELF file.
    pub fn from_raw(data: &[u8]) -> Result<Self, DebugError> {
        let object = object::File::parse(data)?;

        // Load a section and return as `Cow<[u8]>`.
        let load_section = |id: gimli::SectionId| -> Result<DwarfReader, gimli::Error> {
            let data = object
                .section_by_name(id.name())
                .and_then(|section| section.uncompressed_data().ok())
                .unwrap_or_else(|| borrow::Cow::Borrowed(&[][..]));

            Ok(gimli::read::EndianRcSlice::new(
                Rc::from(&*data),
                gimli::LittleEndian,
            ))
        };

        // Load all of the sections.
        let dwarf_cow = gimli::Dwarf::load(&load_section)?;

        use gimli::Section;
        let mut frame_section = gimli::DebugFrame::load(load_section)?;

        // To support DWARF v2, where the address size is not encoded in the .debug_frame section,
        // we have to set the address size here.
        frame_section.set_address_size(4);

        Ok(DebugInfo {
            dwarf: dwarf_cow,
            frame_section,
        })
    }

    pub fn function_name(&self, address: u64, find_inlined: bool) -> Option<String> {
        let mut units = self.dwarf.units();

        while let Some(unit_info) = self.get_next_unit_info(&mut units) {
            if let Some(die_cursor_state) = &mut unit_info.get_function_die(address, find_inlined) {
                let function_name = die_cursor_state.function_name(&unit_info);

                if function_name.is_some() {
                    return function_name;
                }
            }
        }

        None
    }

    /// Try get the [`SourceLocation`] for a given address.
    pub fn get_source_location(&self, address: u64) -> Option<SourceLocation> {
        let mut units = self.dwarf.units();

        while let Ok(Some(header)) = units.next() {
            let unit = match self.dwarf.unit(header) {
                Ok(unit) => unit,
                Err(_) => continue,
            };

            let mut ranges = self.dwarf.unit_ranges(&unit).unwrap();

            while let Ok(Some(range)) = ranges.next() {
                if (range.begin <= address) && (address < range.end) {
                    // Get the function name.

                    let ilnp = match unit.line_program.as_ref() {
                        Some(ilnp) => ilnp,
                        None => return None,
                    };

                    let (program, sequences) = ilnp.clone().sequences().unwrap();

                    // Normalize the address.
                    let mut target_seq = None;

                    for seq in sequences {
                        if (seq.start <= address) && (address < seq.end) {
                            target_seq = Some(seq);
                            break;
                        }
                    }

                    target_seq.as_ref()?;

                    let mut previous_row: Option<gimli::LineRow> = None;

                    let mut rows =
                        program.resume_from(target_seq.as_ref().expect("Sequence not found"));

                    while let Ok(Some((header, row))) = rows.next_row() {
                        if row.address() == address {
                            let (file, directory) = self
                                .find_file_and_directory(&unit, header, row.file(header).unwrap())
                                .unwrap();

                            return Some(SourceLocation {
                                line: row.line().map(NonZeroU64::get),
                                column: Some(row.column().into()),
                                file,
                                directory,
                            });
                        } else if (row.address() > address) && previous_row.is_some() {
                            let row = previous_row.unwrap();

                            let (file, directory) = self
                                .find_file_and_directory(&unit, header, row.file(header).unwrap())
                                .unwrap();

                            return Some(SourceLocation {
                                line: row.line().map(NonZeroU64::get),
                                column: Some(row.column().into()),
                                file,
                                directory,
                            });
                        }
                        previous_row = Some(*row);
                    }
                }
            }
        }
        None
    }

    fn get_units(&self) -> UnitIter {
        self.dwarf.units()
    }

    fn get_next_unit_info(&self, units: &mut UnitIter) -> Option<UnitInfo> {
        while let Ok(Some(header)) = units.next() {
            if let Ok(unit) = self.dwarf.unit(header) {
                return Some(UnitInfo {
                    debug_info: self,
                    unit,
                });
            };
        }
        None
    }

    /// Returns a vector of all the static `Variable`s in the stack.
    pub fn get_stack_statics(
        &self,
        core: &mut Core<'_>,
        program_counter: u64,
    ) -> Result<Vec<Variable>, DebugError> {
        let mut static_root_variable = Variable::new();
        static_root_variable.name = "<statics>".to_string();
        let mut header_units = self.get_units();
        // Iterate through the unit headers.
        while let Some(unit_info) = self.get_next_unit_info(&mut header_units) {
            let abbrevs = &unit_info.unit.abbreviations;
            // Navigate the current unit from the header down.
            if let Ok(mut header_tree) = unit_info.unit.header.entries_tree(abbrevs, None) {
                let unit_node = header_tree.root()?;
                unit_info.process_tree(
                    unit_node,
                    &mut static_root_variable,
                    core,
                    0,
                    program_counter,
                )?;
            }
        }
        match static_root_variable.children {
            Some(function_variables) => Ok(function_variables),
            None => Ok(vec![]),
        }
    }

    /// Returns a populated (resolved) [`StackFrame`] struct.
    fn get_stackframe_info(
        &self,
        core: &mut Core<'_>,
        address: u64,
        frame_count: u64,
        registers: Registers,
        inlined_function: bool,
    ) -> Result<StackFrame, DebugError> {
        let mut units = self.get_units();

        let unknown_function = format!("<unknown function @ {:#010x}>", address);

        while let Some(unit_info) = self.get_next_unit_info(&mut units) {
            if let Some(die_cursor_state) =
                &mut unit_info.get_function_die(address, inlined_function)
            {
                let function_name = die_cursor_state
                    .function_name(&unit_info)
                    .unwrap_or(unknown_function);

                log::debug!("Function name: {}", function_name);

                let variables = unit_info.get_function_variables(
                    core,
                    die_cursor_state,
                    u64::from(registers.get_canonical_frame_address().unwrap_or(0)),
                    u64::from(registers.get_return_address().unwrap_or(0)),
                )?;

                // Ready to go ...
                return Ok(StackFrame {
                    // MS DAP Specification requires the id to be unique accross all threads, so using the frame pointer as the id.
                    id: registers.get_canonical_frame_address().unwrap_or(0) as u64,
                    function_name,
                    source_location: self.get_source_location(address),
                    registers,
                    pc: address as u32,
                    variables,
                });
            }
        }

        Ok(StackFrame {
            id: frame_count,
            function_name: unknown_function,
            source_location: self.get_source_location(address),
            registers,
            pc: address as u32,
            variables: vec![],
        })
    }

    pub fn try_unwind<'probe, 'core>(
        &self,
        core: &'core mut Core<'probe>,
        address: u64,
    ) -> StackFrameIterator<'_, 'probe, 'core> {
        StackFrameIterator::new(self, core, address)
    }

    /// Find the program counter where a breakpoint should be set,
    /// given a source file, a line and optionally a column.
    pub fn get_breakpoint_location(
        &self,
        path: &Path,
        line: u64,
        column: Option<u64>,
    ) -> Result<Option<u64>, DebugError> {
        log::debug!(
            "Looking for breakpoint location for {}:{}:{}",
            path.display(),
            line,
            column
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_owned())
        );

        let mut unit_iter = self.dwarf.units();

        let mut locations = Vec::new();

        while let Some(unit_header) = unit_iter.next()? {
            let unit = self.dwarf.unit(unit_header)?;

            if let Some(ref line_program) = unit.line_program {
                let header = line_program.header();

                for file_name in header.file_names() {
                    let combined_path = self.get_path(&unit, header, file_name);

                    if combined_path.map(|p| p == path).unwrap_or(false) {
                        let mut rows = line_program.clone().rows();

                        while let Some((header, row)) = rows.next_row()? {
                            let row_path = row
                                .file(header)
                                .and_then(|file_entry| self.get_path(&unit, header, file_entry));

                            if row_path.map(|p| p != path).unwrap_or(true) {
                                continue;
                            }

                            if let Some(cur_line) = row.line() {
                                if cur_line.get() == line {
                                    locations.push((row.address(), row.column()));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Look for the break point location for the best match based on the column specified.
        match locations.len() {
            0 => Ok(None),
            1 => Ok(Some(locations[0].0)),
            n => {
                log::debug!("Found {} possible breakpoint locations", n);

                locations.sort_by({
                    |a, b| {
                        if a.1 != b.1 {
                            a.1.cmp(&b.1)
                        } else {
                            a.0.cmp(&b.0)
                        }
                    }
                });

                for loc in &locations {
                    log::debug!("col={:?}, addr=0x{:08x}", loc.1, loc.0);
                }

                match column {
                    Some(search_col) => {
                        let mut best_location = &locations[0];

                        let search_col = match NonZeroU64::new(search_col) {
                            None => gimli::read::ColumnType::LeftEdge,
                            Some(c) => gimli::read::ColumnType::Column(c),
                        };

                        for loc in &locations[1..] {
                            if loc.1 > search_col {
                                break;
                            }

                            if best_location.1 < loc.1 {
                                best_location = loc;
                            }
                        }

                        Ok(Some(best_location.0))
                    }
                    None => Ok(Some(locations[0].0)),
                }
            }
        }
    }

    /// Get the absolute path for an entry in a line program header
    fn get_path(
        &self,
        unit: &gimli::read::Unit<DwarfReader>,
        header: &LineProgramHeader<DwarfReader>,
        file_entry: &FileEntry<DwarfReader>,
    ) -> Option<PathBuf> {
        let file_name_attr_string = self.dwarf.attr_string(unit, file_entry.path_name()).ok()?;
        let dir_name_attr_string = file_entry
            .directory(header)
            .and_then(|dir| self.dwarf.attr_string(unit, dir).ok());

        let name_path = Path::new(from_utf8(&file_name_attr_string).ok()?);

        let dir_path =
            dir_name_attr_string.and_then(|dir_name| from_utf8(&dir_name).ok().map(PathBuf::from));

        let mut combined_path = match dir_path {
            Some(dir_path) => dir_path.join(name_path),
            None => name_path.to_owned(),
        };

        if combined_path.is_relative() {
            let comp_dir = unit
                .comp_dir
                .as_ref()
                .map(|dir| from_utf8(dir))
                .transpose()
                .ok()?
                .map(PathBuf::from);

            if let Some(comp_dir) = comp_dir {
                combined_path = comp_dir.join(&combined_path);
            }
        }

        Some(combined_path)
    }

    fn find_file_and_directory(
        &self,
        unit: &gimli::read::Unit<DwarfReader>,
        header: &LineProgramHeader<DwarfReader>,
        file_entry: &FileEntry<DwarfReader>,
    ) -> Option<(Option<String>, Option<PathBuf>)> {
        let combined_path = self.get_path(unit, header, file_entry)?;

        let file_name = combined_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());

        let directory = combined_path.parent().map(|p| p.to_path_buf());

        Some((file_name, directory))
    }
}

/// Reference to a DIE for a function
struct FunctionDie<'abbrev, 'unit> {
    function_die: FunctionDieType<'abbrev, 'unit>,

    is_inline: bool,
    abstract_die: Option<FunctionDieType<'abbrev, 'unit>>,
}

impl<'debugunit, 'abbrev, 'unit: 'debugunit> FunctionDie<'abbrev, 'unit> {
    fn new(die: FunctionDieType<'abbrev, 'unit>) -> Self {
        let tag = die.tag();

        match tag {
            gimli::DW_TAG_subprogram => {
                Self {
                    function_die: die,
                    is_inline: false,
                    abstract_die: None,
                }
            }
            other_tag => panic!("FunctionDie has to has to have Tag DW_TAG_subprogram, but tag is {:?}. This is a bug, please report it.", other_tag.static_string())
        }
    }

    fn new_inlined(
        concrete_die: FunctionDieType<'abbrev, 'unit>,
        abstract_die: FunctionDieType<'abbrev, 'unit>,
    ) -> Self {
        let tag = concrete_die.tag();

        match tag {
            gimli::DW_TAG_inlined_subroutine => {
                Self {
                    function_die: concrete_die,
                    is_inline: true,
                    abstract_die: Some(abstract_die),
                }
            }
            other_tag => panic!("FunctionDie has to has to have Tag DW_TAG_inlined_subroutine, but tag is {:?}. This is a bug, please report it.", other_tag.static_string())
        }
    }

    fn function_name(&self, unit: &UnitInfo<'_>) -> Option<String> {
        if let Some(fn_name_attr) = self.get_attribute(gimli::DW_AT_name) {
            match fn_name_attr.value() {
                gimli::AttributeValue::DebugStrRef(fn_name_ref) => {
                    let fn_name_raw = unit.debug_info.dwarf.string(fn_name_ref).unwrap();

                    Some(String::from_utf8_lossy(&fn_name_raw).to_string())
                }
                value => {
                    log::debug!("Unexpected attribute value for DW_AT_name: {:?}", value);
                    None
                }
            }
        } else {
            log::debug!("DW_AT_name attribute not found, unable to retrieve function name");
            None
        }
    }

    fn get_attribute(&self, attribute_name: gimli::DwAt) -> Option<GimliAttribute> {
        let attribute = self
            .function_die
            .attr(attribute_name)
            .expect(" Failed to parse entry");

        // For inlined function, the *abstract instance* has to be checked if we cannot find the
        // attribute on the *concrete instance*.
        if self.is_inline && attribute.is_none() {
            let origin = self.abstract_die.as_ref().unwrap();

            origin.attr(attribute_name).expect("Failed to parse entry")
        } else {
            attribute
        }
    }
}

struct UnitInfo<'debuginfo> {
    debug_info: &'debuginfo DebugInfo,
    unit: gimli::Unit<GimliReader, usize>,
}

impl<'debuginfo> UnitInfo<'debuginfo> {
    /// Get the DIE for the function containing the given address.
    fn get_function_die(&self, address: u64, find_inlined: bool) -> Option<FunctionDie> {
        log::trace!("Searching Function DIE for address {:#010x}", address);

        let mut entries_cursor = self.unit.entries();

        while let Ok(Some((_depth, current))) = entries_cursor.next_dfs() {
            if current.tag() == gimli::DW_TAG_subprogram {
                let mut ranges = self
                    .debug_info
                    .dwarf
                    .die_ranges(&self.unit, current)
                    .unwrap();

                while let Ok(Some(ranges)) = ranges.next() {
                    if (ranges.begin <= address) && (address < ranges.end) {
                        // Check if we are actually in an inlined function

                        let die = FunctionDie::new(current.clone());

                        if find_inlined {
                            log::debug!(
                                "Found DIE, now checking for inlined functions: name={:?}",
                                die.function_name(self)
                            );

                            return self
                                .find_inlined_function(address, current.offset())
                                .or_else(|| {
                                    log::debug!("No inlined function found!");
                                    Some(FunctionDie::new(current.clone()))
                                });
                        } else {
                            log::debug!("Found DIE: name={:?}", die.function_name(self));

                            return Some(die);
                        }
                    }
                }
            }
        }
        None
    }

    /// Check if the function located at the given offset contains an inlined function at the
    /// given address.
    fn find_inlined_function(&self, address: u64, offset: UnitOffset) -> Option<FunctionDie> {
        let mut current_depth = 0;

        let mut cursor = self.unit.entries_at_offset(offset).unwrap();

        while let Ok(Some((depth, current))) = cursor.next_dfs() {
            current_depth += depth;

            if current_depth < 0 {
                break;
            }

            if current.tag() == gimli::DW_TAG_inlined_subroutine {
                let mut ranges = self
                    .debug_info
                    .dwarf
                    .die_ranges(&self.unit, current)
                    .unwrap();

                while let Ok(Some(ranges)) = ranges.next() {
                    if (ranges.begin <= address) && (address < ranges.end) {
                        // Check if we are actually in an inlined function

                        // Find the abstract definition

                        if let Some(abstract_origin) = current.attr(DW_AT_abstract_origin).unwrap()
                        {
                            match abstract_origin.value() {
                                gimli::AttributeValue::UnitRef(unit_ref) => {
                                    let abstract_die = self.unit.entry(unit_ref).unwrap();

                                    return Some(FunctionDie::new_inlined(
                                        current.clone(),
                                        abstract_die.clone(),
                                    ));
                                }
                                other_value => panic!("Unsupported value: {:?}", other_value),
                            }
                        } else {
                            return None;
                        }
                    }
                }
            }
        }

        None
    }

    fn expr_to_piece(
        &self,
        core: &mut Core<'_>,
        expression: gimli::Expression<GimliReader>,
        frame_base: u64,
    ) -> Result<Vec<gimli::Piece<GimliReader, usize>>, DebugError> {
        let mut evaluation = expression.evaluation(self.unit.encoding());

        // go for evaluation
        let mut result = evaluation.evaluate()?;

        loop {
            use gimli::EvaluationResult::*;

            result = match result {
                Complete => break,
                RequiresMemory { address, size, .. } => {
                    let mut buff = vec![0u8; size as usize];
                    core.read(address as u32, &mut buff).map_err(|_| {
                        DebugError::Other(anyhow::anyhow!("Unexpected error while reading debug expressions from target memory. Please report this as a bug."))
                    })?;
                    match size {
                        1 => evaluation.resume_with_memory(gimli::Value::U8(buff[0]))?,
                        2 => {
                            let val = (u16::from(buff[0]) << 8) | (u16::from(buff[1]) as u16);
                            evaluation.resume_with_memory(gimli::Value::U16(val))?
                        }
                        4 => {
                            let val = (u32::from(buff[0]) << 24)
                                | (u32::from(buff[1]) << 16)
                                | (u32::from(buff[2]) << 8)
                                | u32::from(buff[3]);
                            evaluation.resume_with_memory(gimli::Value::U32(val))?
                        }
                        x => {
                            todo!(
                                "Requested memory with size {}, which is not supported yet.",
                                x
                            );
                        }
                    }
                }
                RequiresFrameBase => evaluation.resume_with_frame_base(frame_base).unwrap(),
                RequiresRegister {
                    register,
                    base_type,
                } => {
                    let raw_value = core.read_core_reg(register.0 as u16)?;

                    if base_type != gimli::UnitOffset(0) {
                        todo!(
                            "Support for units in RequiresRegister request is not yet implemented."
                        )
                    }

                    evaluation.resume_with_register(gimli::Value::Generic(raw_value as u64))?
                }
                RequiresRelocatedAddress(address_index) => {
                    if address_index.is_zero() {
                        // This is a rust-lang bug for statics ... https://github.com/rust-lang/rust/issues/32574.
                        evaluation.resume_with_relocated_address(u64::MAX)?
                    } else {
                        // The address_index as an offset from 0, so just pass it into the next step.
                        evaluation.resume_with_relocated_address(address_index)?
                    }
                }
                x => {
                    todo!("expr_to_piece {:?}", x)
                }
            }
        }
        Ok(evaluation.result())
    }

    fn process_tree_node_attributes(
        &self,
        tree_node: &mut gimli::EntriesTreeNode<GimliReader>,
        parent_variable: &mut Variable,
        child_variable: &mut Variable,
        core: &mut Core<'_>,
        frame_base: u64,
        program_counter: u64,
    ) -> Result<(), DebugError> {
        // We need to process the location attribute in advance of looping through all the attributes,
        // to ensure that location is known before we calculate type.
        self.extract_location(tree_node, parent_variable, child_variable, core, frame_base)?;
        // It often happens that intermediate nodes exist for structure reasons,
        // so we need to pass values like 'memory_location' from the parent down to the next level child nodes.
        if child_variable.memory_location.is_zero() {
            child_variable.memory_location = parent_variable.memory_location;
        }
        if parent_variable.member_index.is_some() {
            child_variable.member_index = parent_variable.member_index;
        }
        let attrs = &mut tree_node.entry().attrs();
        while let Some(attr) = attrs.next().unwrap() {
            match attr.name() {
                gimli::DW_AT_location | gimli::DW_AT_data_member_location => {
                    // The child_variable.location is calculated higher up by invoking self.extract_location.
                }
                gimli::DW_AT_name => {
                    child_variable.name = extract_name(self.debug_info, attr.value());
                }
                gimli::DW_AT_decl_file => {
                    child_variable.file = extract_file(self.debug_info, &self.unit, attr.value())
                        .unwrap_or_else(|| "<undefined>".to_string());
                }
                gimli::DW_AT_decl_line => {
                    child_variable.line = extract_line(self.debug_info, attr.value()).unwrap_or(0);
                }
                gimli::DW_AT_decl_column => {
                    // Unused.
                }
                gimli::DW_AT_containing_type => {
                    // TODO: Resolve Traits.
                }
                gimli::DW_AT_type => {
                    match attr.value() {
                        gimli::AttributeValue::UnitRef(unit_ref) => {
                            // Reference to a type, or an entry to another type or a type modifier which will point to another type.
                            let mut type_tree = self
                                .unit
                                .header
                                .entries_tree(&self.unit.abbreviations, Some(unit_ref))?;
                            let tree_node = type_tree.root().unwrap();
                            self.extract_type(
                                tree_node,
                                parent_variable,
                                child_variable,
                                core,
                                frame_base,
                                program_counter,
                            )?;
                        }
                        other_attribute_value => {
                            child_variable.set_value(format!(
                                "UNIMPLEMENTED: Attribute Value for DW_AT_type {:?}",
                                other_attribute_value
                            ));
                        }
                    }
                }
                gimli::DW_AT_enum_class => match attr.value() {
                    gimli::AttributeValue::Flag(is_enum_class) => {
                        if is_enum_class {
                            child_variable.set_value(child_variable.type_name.clone());
                        } else {
                            child_variable.set_value(format!(
                                "UNIMPLEMENTED: Flag Value for DW_AT_enum_class {:?}",
                                is_enum_class
                            ));
                        }
                    }
                    other_attribute_value => {
                        child_variable.set_value(format!(
                            "UNIMPLEMENTED: Attribute Value for DW_AT_enum_class: {:?}",
                            other_attribute_value
                        ));
                    }
                },
                gimli::DW_AT_const_value => match attr.value() {
                    gimli::AttributeValue::Udata(const_value) => {
                        child_variable.set_value(const_value.to_string());
                    }
                    other_attribute_value => {
                        child_variable.set_value(format!(
                            "UNIMPLEMENTED: Attribute Value for DW_AT_const_value: {:?}",
                            other_attribute_value
                        ));
                    }
                },
                gimli::DW_AT_alignment => {
                    // TODO: Implement this.
                }
                // TODO: Figure out when (if at all) we need to do anything with DW_AT_alignment for the purposes of decoding data values.
                gimli::DW_AT_artificial => {
                    // These are references for entries like discriminant values of `VariantParts`.
                    child_variable.name = "<artificial>".to_string();
                    child_variable.inclusion = VariableInclusion::Exclude;
                }
                gimli::DW_AT_discr => match attr.value() {
                    // This calculates the active discriminant value for the `VariantPart`.
                    gimli::AttributeValue::UnitRef(unit_ref) => {
                        let mut type_tree = self
                            .unit
                            .header
                            .entries_tree(&self.unit.abbreviations, Some(unit_ref))?;
                        let mut discriminant_node = type_tree.root().unwrap();
                        let mut discriminant_variable = Variable::new();
                        self.process_tree_node_attributes(
                            &mut discriminant_node,
                            parent_variable,
                            &mut discriminant_variable,
                            core,
                            frame_base,
                            program_counter,
                        )?;
                        discriminant_variable.extract_value(core);
                        parent_variable.role = VariantRole::VariantPart(
                            discriminant_variable
                                .get_value()
                                .parse()
                                .unwrap_or(u64::MAX) as u64,
                        );
                    }
                    other_attribute_value => {
                        child_variable.set_value(format!(
                            "UNIMPLEMENTED: Attribute Value for DW_AT_discr {:?}",
                            other_attribute_value
                        ));
                    }
                },
                // Property of variables that are of DW_TAG_subrange_type.
                gimli::DW_AT_lower_bound => match attr.value().udata_value() {
                    Some(lower_bound) => child_variable.range_lower_bound = lower_bound as i64,
                    None => {
                        child_variable.set_value(format!(
                            "UNIMPLEMENTED: Attribute Value for DW_AT_lower_bound: {:?}",
                            attr.value()
                        ));
                    }
                },
                // Property of variables that are of DW_TAG_subrange_type.
                gimli::DW_AT_upper_bound | gimli::DW_AT_count => match attr.value().udata_value() {
                    Some(upper_bound) => child_variable.range_upper_bound = upper_bound as i64,
                    None => {
                        child_variable.set_value(format!(
                            "UNIMPLEMENTED: Attribute Value for DW_AT_upper_bound: {:?}",
                            attr.value()
                        ));
                    }
                },
                gimli::DW_AT_external => {
                    // TODO: Implement globally visible variables.
                }
                gimli::DW_AT_declaration => {
                    // Unimplemented. Setting this value ensure we do not such variables to the variable tree.
                    child_variable.inclusion = VariableInclusion::Exclude;
                }
                gimli::DW_AT_encoding => {
                    // Ignore these. RUST data types handle this intrinsicly.
                }
                gimli::DW_AT_discr_value => {
                    // Processed by `extract_variant_discriminant()`.
                }
                gimli::DW_AT_byte_size => {
                    // Processed by `extract_byte_size()`.
                }
                gimli::DW_AT_abstract_origin => {
                    // TODO: DW_AT_abstract_origin attributes are only applicable to DW_TAG_subprogram (closures), and DW_TAG_inline_subroutine, and DW_TAG_formal_parameters.
                }
                gimli::DW_AT_linkage_name => {
                    // Unused.
                }
                other_attribute => {
                    child_variable.set_value(format!(
                        "UNIMPLEMENTED: Variable Attribute {:?} : {:?}, with children = {}",
                        other_attribute.static_string(),
                        tree_node
                            .entry()
                            .attr_value(other_attribute)
                            .unwrap()
                            .unwrap(),
                        tree_node.entry().has_children()
                    ));
                }
            }
        }
        Ok(())
    }

    fn process_tree(
        &self,
        parent_node: gimli::EntriesTreeNode<GimliReader>,
        parent_variable: &mut Variable,
        core: &mut Core<'_>,
        frame_base: u64,
        program_counter: u64,
    ) -> Result<(), DebugError> {
        let mut child_nodes = parent_node.children();
        while let Some(mut child_node) = child_nodes.next()? {
            match child_node.entry().tag() {
                gimli::DW_TAG_namespace => {
                    // Use these parents to extract `statics`.
                    let mut namespace_variable = Variable::new();
                    namespace_variable.name = if let Ok(Some(attr)) = child_node.entry().attr(gimli::DW_AT_name) {
                        extract_name(self.debug_info, attr.value())
                    } else {"<anonymous namespace>".to_string()};
                    namespace_variable.type_name = "namespace".to_string();
                    namespace_variable.memory_location = 0;
                    let mut namespace_children_nodes = child_node.children();
                    while let Some(mut namespace_child_node) = namespace_children_nodes.next()? {
                        match namespace_child_node.entry().tag() {
                            gimli::DW_TAG_variable => {
                                // We only want the TOP level variables of the namespace (statics).
                                let mut static_child_variable = Variable::new();
                                self.process_tree_node_attributes(&mut namespace_child_node, &mut namespace_variable, &mut static_child_variable, core, frame_base, program_counter)?;
                                static_child_variable.inclusion = VariableInclusion::Include;
                                namespace_variable.add_child_variable(&mut static_child_variable, core);
                            }
                            gimli::DW_TAG_namespace => {
                                // Recurse for additional namespace variables.
                                let mut namespace_child_variable = Variable::new();
                                namespace_child_variable.name = if let Ok(Some(attr)) = namespace_child_node.entry().attr(gimli::DW_AT_name) {
                                    format!("{}::{}", namespace_variable.name, extract_name(self.debug_info, attr.value()))
                                } else {"<anonymous namespace>".to_string()};
                                namespace_child_variable.type_name = "namespace".to_string();
                                namespace_child_variable.memory_location = 0;
                                self.process_tree(namespace_child_node, &mut namespace_child_variable, core, frame_base, program_counter)?;
                                namespace_variable.add_child_variable(&mut namespace_child_variable, core);
                            }
                            _ => {
                                // We only want namespace and variable children.
                            }
                        }
                    }
                    parent_variable.add_child_variable(&mut namespace_variable, core);
                }
                gimli::DW_TAG_variable |    // Typical top-level variables.
                gimli::DW_TAG_member |      // Members of structured types.
                gimli::DW_TAG_enumerator    // Possible values for enumerators, used by extract_type() when processing DW_TAG_enumeration_type.
                => {
                    let mut child_variable = Variable::new();
                    self.process_tree_node_attributes(&mut child_node, parent_variable, &mut child_variable, core, frame_base, program_counter)?;
                    if ! (child_variable.type_name.starts_with("PhantomData") // Do not process PhantomData nodes
                    || child_variable.inclusion == VariableInclusion::Exclude ) {
                        // Recursively process each child.
                        self.process_tree(child_node, &mut child_variable, core, frame_base, program_counter)?;
                        child_variable.inclusion = VariableInclusion::Include;
                        parent_variable.add_child_variable(&mut child_variable, core);
                    }
                }
                gimli::DW_TAG_variant_part => {
                    // We need to recurse through the children, to find the DW_TAG_variant with discriminant matching the DW_TAG_variant, 
                    // and ONLY add it's children to the parent variable. 
                    // The structure looks like this (there are other nodes in the structure that we use and discard before we get here):
                    // Level 1: --> An actual variable that has a variant value
                    //      Level 2: --> this DW_TAG_variant_part node (some child nodes arer used to calc the active Variant discriminant)
                    //          Level 3: --> Some DW_TAG_variant's that have discriminant values to be matched against the discriminant 
                    //              Level 4: --> The actual variables, with matching discriminant, which will be added to `parent_variable`
                    // TODO: Handle Level 3 nodes that belong to a DW_AT_discr_list, instead of having a discreet DW_AT_discr_value 
                    let mut child_variable = Variable::new();
                    // If there is a child with DW_AT_discr, the variable role will updated appropriately, otherwise we use 0 as the default ...
                    parent_variable.role = VariantRole::VariantPart(0);
                    self.process_tree_node_attributes(&mut child_node, parent_variable, &mut child_variable, core, frame_base, program_counter)?;
                    // Pass it along through intermediate nodes.
                    child_variable.role = parent_variable.role.clone();
                    // Recursively process each child.
                    self.process_tree(child_node, &mut child_variable, core, frame_base, program_counter)?;
                    // Make sure we pass children up, past the intermediate.
                    if child_variable.type_name.is_empty()
                    && child_variable.children.is_some()  {
                        for mut grand_child in child_variable.children.unwrap() {
                            parent_variable.add_child_variable(&mut grand_child, core);
                        }
                    }
                }
                gimli::DW_TAG_variant // variant is a child of a structure, and one of them should have a discriminant value to match the DW_TAG_variant_part 
                => {
                    let mut child_variable = Variable::new();
                    // We need to do this here, to identify "default" variants for when the rust lang compiler doesn't encode them explicitly ... only by absence of a DW_AT_discr_value
                    self.extract_variant_discriminant(&child_node, &mut child_variable, core, frame_base)?;
                    self.process_tree_node_attributes(&mut child_node, parent_variable, &mut child_variable, core, frame_base, program_counter)?;
                    if let VariantRole::Variant(discriminant) = child_variable.role {
                        // Only process the discriminant variants.
                        if parent_variable.role == VariantRole::VariantPart(discriminant) {
                            // Recursively process each relevant child.
                            self.process_tree(child_node, &mut child_variable, core, frame_base, program_counter)?;
                            // Make sure we pass children up, past the intermediate.
                            if child_variable.type_name.is_empty()
                            && child_variable.children.is_some()  {
                                for mut grand_child in child_variable.children.unwrap() {
                                    parent_variable.add_child_variable(&mut grand_child, core);
                                }
                            }
                        }
                    }
                }
                gimli::DW_TAG_subrange_type => {
                    // This tag is a child node fore parent types such as (array, vector, etc.).
                    // Recursively process each node, but pass the parent_variable so that new children are caught despite missing these tags.
                    let mut range_variable = Variable::new();
                    self.process_tree_node_attributes(&mut child_node, parent_variable, &mut range_variable, core, frame_base, program_counter)?;
                    // Pass the pertinent info up to the parent_variable.
                    parent_variable.type_name = range_variable.type_name;
                    parent_variable.range_lower_bound = range_variable.range_lower_bound;
                    parent_variable.range_upper_bound = range_variable.range_upper_bound;
                }
                gimli::DW_TAG_template_type_parameter => {
                    // The parent node for Rust generic type parameter
                    // These show up as a child of structures they belong to, but currently don't lead to the member value or type.
                    // Until rust lang implements this, we will ONLY process the ACTUAL structure member, to avoid a cluttered UI. 
                    // let mut template_type_variable = Variable::new();
                    // self.process_tree_node_attributes(&mut child_node, parent_variable, &mut template_type_variable, core, frame_base, program_counter)?;
                    // parent_variable.add_child_variable(&mut template_type_variable, core);
                    // self.process_tree(child_node, parent_variable, core, frame_base, program_counter)?;
                }
                gimli::DW_TAG_formal_parameter => {
                    // TODO: WIP Parameters for DW_TAG_inlined_subroutine
                    // let mut child_variable = Variable::new();
                    // self.process_tree_node_attributes(&mut child_node, parent_variable, &mut child_variable, core, frame_base)?;
                    // // Recursively process each child.
                    // self.process_tree(child_node, &mut child_variable, core, frame_base)?;
                    // parent_variable.add_child_variable(&mut child_variable, core);
                    self.process_tree(child_node, parent_variable, core, frame_base, program_counter)?;
                }
                gimli::DW_TAG_inlined_subroutine => {
                    // These are handled by `find_inlined_function()` as part of the stack frame iteration process.
                    self.process_tree(child_node, parent_variable, core, frame_base, program_counter)?;
                }
                gimli::DW_TAG_lexical_block => {
                    // Determine the low and high ranges for which this DIE and children are in scope. These can be specified discreetly, or in ranges. 
                    let mut in_scope =  false;
                    if let Ok(Some(low_pc_attr)) = child_node.entry().attr(gimli::DW_AT_low_pc) {
                        let low_pc = match low_pc_attr.value() {
                            gimli::AttributeValue::Addr(value) => value as u64,
                            _other => u64::MAX,
                        };
                        let high_pc = if let Ok(Some(high_pc_attr))
                            = child_node.entry().attr(gimli::DW_AT_high_pc) {
                                match high_pc_attr.value() {
                                    gimli::AttributeValue::Addr(addr) => addr,
                                    gimli::AttributeValue::Udata(unsigned_offset) => low_pc + unsigned_offset,
                                    _other => 0_u64,
                                }
                        } else { 0_u64};
                        if low_pc == u64::MAX || high_pc == 0_u64 {
                            // These have not been specified correctly ... something went wrong.
                            parent_variable.set_value("ERROR: Processing of variables failed because of invalid/unsupported scope information. Please log a bug at 'https://github.com/probe-rs/probe-rs/issues'".to_string());
                        }
                        if low_pc <= program_counter && program_counter < high_pc {
                            // We have established positive scope, so no need to continue.
                            in_scope = true;
                        };
                        // No scope info yet, so keep looking. 
                    };
                    // Searching for ranges has a bit more overhead, so ONLY do this if do not have scope confirmed yet.
                    if !in_scope {
                        if let Ok(Some(ranges))
                            = child_node.entry().attr(gimli::DW_AT_ranges) {
                                match ranges.value() {
                                    gimli::AttributeValue::RangeListsRef(raw_range_lists_offset) => {
                                        let range_lists_offset = self.debug_info.dwarf.ranges_offset_from_raw(&self.unit, raw_range_lists_offset);

                                        if let Ok(mut ranges) = self
                                            .debug_info
                                            .dwarf
                                            .ranges(&self.unit, range_lists_offset) {
                                                while let Ok(Some(ranges)) = ranges.next() {
                                                    // We have established positive scope, so no need to continue.
                                                    if ranges.begin <= program_counter && program_counter < ranges.end {
                                                        in_scope = true;
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                    other_range_attribute => {
                                        parent_variable.set_value(format!("Found unexpected scope attribute: {:?} for variable {:?}", other_range_attribute, parent_variable.name));
                                    }
                                }
                        }
                    }
                    if in_scope {
                        // This is IN scope.
                        // Recursively process each child, but pass the parent_variable, so that we don't create intermediate nodes for scope identifiers.
                        self.process_tree(child_node, parent_variable, core, frame_base, program_counter)?;
                    } else {
                        // Out of scope .
                    }
                }
                other => {
                    // One of two things are true here. Either we've encountered a DwTag that is implemented in `extract_type`, and whould be ignored, or we have encountered an UNIMPLEMENTED  DwTag.
                    match other {
                        gimli::DW_TAG_base_type |
                        gimli::DW_TAG_pointer_type |
                        gimli::DW_TAG_structure_type |
                        gimli::DW_TAG_enumeration_type |
                        gimli::DW_TAG_array_type |
                        gimli::DW_TAG_subroutine_type |
                        gimli::DW_TAG_subprogram |
                        gimli::DW_TAG_inlined_subroutine |
                        gimli::DW_TAG_union_type => {
                            // These will be processed elsewhere.
                        }
                        unimplemented => {
                            parent_variable.set_value(format!("UNIMPLEMENTED: Encountered unimplemented DwTag {:?} for Variable {:?}", unimplemented.static_string(), parent_variable));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn get_function_variables(
        &self,
        core: &mut Core<'_>,
        die_cursor_state: &mut FunctionDie,
        frame_base: u64,
        program_counter: u64,
    ) -> Result<Vec<Variable>, DebugError> {
        let abbrevs = &self.unit.abbreviations;
        let mut tree = self
            .unit
            .header
            .entries_tree(abbrevs, Some(die_cursor_state.function_die.offset()))?;
        let function_node = tree.root()?;
        let mut root_variable = Variable::new();
        root_variable.name = "<locals>".to_string();
        self.process_tree(
            function_node,
            &mut root_variable,
            core,
            frame_base,
            program_counter,
        )?;
        match root_variable.children {
            Some(function_variables) => Ok(function_variables),
            None => Ok(vec![]),
        }
    }

    /// Compute the discriminant value of a DW_TAG_variant variable. If it is not explicitly captured in the DWARF, then it is the default value.
    fn extract_variant_discriminant(
        &self,
        node: &gimli::EntriesTreeNode<GimliReader>,
        variable: &mut Variable,
        _core: &mut Core<'_>,
        _frame_base: u64,
    ) -> Result<(), DebugError> {
        if node.entry().tag() == gimli::DW_TAG_variant {
            variable.role = match node.entry().attr(gimli::DW_AT_discr_value) {
                Ok(optional_discr_value_attr) => {
                    match optional_discr_value_attr {
                        Some(discr_attr) => {
                            match discr_attr.value() {
                                gimli::AttributeValue::Data1(const_value) => {
                                    VariantRole::Variant(const_value as u64)
                                }
                                other_attribute_value => {
                                    variable.set_value(format!("UNIMPLEMENTED: Attribute Value for DW_AT_discr_value: {:?}", other_attribute_value));
                                    VariantRole::Variant(u64::MAX)
                                }
                            }
                        }
                        None => {
                            // In the case where the variable is a DW_TAG_variant, but has NO DW_AT_discr_value, then this is the "default" to be used.
                            VariantRole::Variant(0)
                        }
                    }
                }
                Err(_error) => {
                    variable.set_value(format!(
                        "ERROR: Retrieving DW_AT_discr_value for variable {:?}",
                        variable
                    ));
                    VariantRole::NonVariant
                }
            };
        }
        Ok(())
    }

    /// Compute the type (base to complex) of a variable. Only base types have values.
    /// Complex types are references to node trees, that require traversal in similar ways to other DIE's like functions.
    /// This means both [`get_function_variables()`] and [`extract_type()`] will call the recursive [`process_tree()`] method to build an integrated `tree` of variables with types and values.
    fn extract_type(
        &self,
        node: gimli::EntriesTreeNode<GimliReader>,
        parent_variable: &mut Variable,
        child_variable: &mut Variable,
        core: &mut Core<'_>,
        frame_base: u64,
        program_counter: u64,
    ) -> Result<(), DebugError> {
        child_variable.type_name = match node.entry().attr(gimli::DW_AT_name) {
            Ok(optional_name_attr) => match optional_name_attr {
                Some(name_attr) => extract_name(self.debug_info, name_attr.value()),
                None => "<unnamed type>".to_owned(),
            },
            Err(error) => {
                format!("ERROR: evaluating name: {:?} ", error)
            }
        };

        child_variable.byte_size = extract_byte_size(self.debug_info, node.entry());
        match node.entry().tag() {
            gimli::DW_TAG_base_type => {
                child_variable.children = None;

                if let Some(child_member_index) = child_variable.member_index {
                    // This is a member of an array type, and needs special handling.
                    let (location, has_overflowed) = child_variable
                        .memory_location
                        .overflowing_add(child_member_index as u64 * child_variable.byte_size);

                    if has_overflowed {
                        return Err(DebugError::Other(anyhow::anyhow!(
                            "Overflow calculating variable address"
                        )));
                    } else {
                        child_variable.memory_location = location;
                    }
                }
                Ok(())
            }
            gimli::DW_TAG_pointer_type => {
                // This needs to resolve the pointer before the regular recursion can continue.
                match node.entry().attr(gimli::DW_AT_type) {
                    Ok(optional_data_type_attribute) => {
                        match optional_data_type_attribute {
                            Some(data_type_attribute) => {
                                match data_type_attribute.value() {
                                    gimli::AttributeValue::UnitRef(unit_ref) => {
                                        // Reference to a type, or an node.entry() to another type or a type modifier which will point to another type.
                                        let mut referenced_variable = Variable::new();
                                        let mut type_tree = self.unit.header.entries_tree(
                                            &self.unit.abbreviations,
                                            Some(unit_ref),
                                        )?;
                                        let referenced_node = type_tree.root()?;
                                        referenced_variable.name = match node
                                            .entry()
                                            .attr(gimli::DW_AT_name)
                                        {
                                            Ok(optional_name_attr) => match optional_name_attr {
                                                Some(name_attr) => {
                                                    extract_name(self.debug_info, name_attr.value())
                                                }
                                                None => "".to_owned(),
                                            },
                                            Err(error) => {
                                                format!("ERROR: evaluating name: {:?} ", error)
                                            }
                                        };
                                        // Now, retrieve the location by reading the adddress pointed to by the parent variable.
                                        let mut buff = [0u8; 4];
                                        core.read(
                                            child_variable.memory_location as u32,
                                            &mut buff,
                                        )?;
                                        referenced_variable.memory_location =
                                            u32::from_le_bytes(buff) as u64;
                                        self.extract_type(
                                            referenced_node,
                                            parent_variable,
                                            &mut referenced_variable,
                                            core,
                                            frame_base,
                                            program_counter,
                                        )?;
                                        if !referenced_variable.type_name.eq("()") {
                                            // Halt further processing of unit types.
                                            referenced_variable.kind = VariableKind::Referenced;
                                            // Now add the referenced_variable as a child.
                                            referenced_variable.inclusion =
                                                VariableInclusion::Include;
                                            child_variable
                                                .add_child_variable(&mut referenced_variable, core);
                                        }
                                    }
                                    other_attribute_value => {
                                        child_variable.set_value(format!(
                                            "UNIMPLEMENTED: Attribute Value for DW_AT_type {:?}",
                                            other_attribute_value
                                        ));
                                    }
                                }
                            }
                            None => {
                                child_variable.set_value(format!(
                                    "ERROR: No Attribute Value for DW_AT_type for variable {:?}",
                                    child_variable.name
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        child_variable.set_value(format!(
                            "ERROR: Failed to decode pointer reference: {:?}",
                            error
                        ));
                    }
                }
                Ok(())
            }
            gimli::DW_TAG_structure_type => {
                // Recursively process a child types.
                if child_variable.memory_location != u64::MAX {
                    // Something is already broken, so don't dig any deeper.
                    self.process_tree(node, child_variable, core, frame_base, program_counter)?;
                }
                if child_variable.children.is_none() {
                    // Empty structs don't have values. Use the type_name as the display value.
                    child_variable.set_value(child_variable.type_name.clone());
                }
                Ok(())
            }
            gimli::DW_TAG_enumeration_type => {
                // Recursively process a child types.
                self.process_tree(node, child_variable, core, frame_base, program_counter)?;
                let enumerator_values = match child_variable.children.clone() {
                    Some(enumerator_values) => enumerator_values,
                    None => {
                        vec![]
                    }
                };
                // NOTE: hard-coding value of variable.byte_size to 1 ... replace with code if necessary.
                let mut buff = [0u8; 1];
                core.read(child_variable.memory_location as u32, &mut buff)?;
                let this_enum_const_value = u8::from_le_bytes(buff).to_string();
                let enumumerator_value =
                    match enumerator_values.into_iter().find(|enumerator_variable| {
                        enumerator_variable.get_value() == this_enum_const_value
                    }) {
                        Some(this_enum) => this_enum.name,
                        None => "<ERROR: Unresolved enum value>".to_string(),
                    };
                child_variable.set_value(format!(
                    "{}::{}",
                    child_variable.type_name, enumumerator_value
                ));
                // We don't need to keep these.
                child_variable.children = None;
                Ok(())
            }
            gimli::DW_TAG_array_type => {
                // This node is a pointer to the type of data stored in the array, with a direct child that contains the range information.
                match node.entry().attr(gimli::DW_AT_type) {
                    Ok(optional_data_type_attribute) => {
                        match optional_data_type_attribute {
                            Some(data_type_attribute) => {
                                match data_type_attribute.value() {
                                    gimli::AttributeValue::UnitRef(unit_ref) => {
                                        // First get the DW_TAG_subrange child of this node. It has a DW_AT_type that points to DW_TAG_base_type:__ARRAY_SIZE_TYPE__.
                                        let mut subrange_variable = Variable::new();
                                        self.process_tree(
                                            node,
                                            &mut subrange_variable,
                                            core,
                                            frame_base,
                                            program_counter,
                                        )?;
                                        child_variable.range_lower_bound =
                                            subrange_variable.range_lower_bound;
                                        child_variable.range_upper_bound =
                                            subrange_variable.range_upper_bound;
                                        if child_variable.range_lower_bound < 0
                                            || child_variable.range_upper_bound < 0
                                        {
                                            child_variable.set_value(format!(
                                                "UNIMPLEMENTED: Array has a sub-range of {}..{} for ",
                                                child_variable.range_lower_bound, child_variable.range_upper_bound)
                                            );
                                        }
                                        // - Next, process this DW_TAG_array_type's DW_AT_type full tree.
                                        // - We have to do this repeatedly, for every array member in the range.
                                        for array_member_index in child_variable.range_lower_bound
                                            ..child_variable.range_upper_bound
                                        {
                                            let mut array_member_variable = Variable::new();
                                            let mut array_member_type_tree =
                                                self.unit.header.entries_tree(
                                                    &self.unit.abbreviations,
                                                    Some(unit_ref),
                                                )?;
                                            let mut array_member_type_node =
                                                array_member_type_tree.root().unwrap();
                                            self.process_tree_node_attributes(
                                                &mut array_member_type_node,
                                                child_variable,
                                                &mut array_member_variable,
                                                core,
                                                frame_base,
                                                program_counter,
                                            )?;
                                            child_variable.type_name = format!(
                                                "[{};{}]",
                                                array_member_variable.name,
                                                subrange_variable.range_upper_bound
                                            );
                                            array_member_variable.member_index =
                                                Some(array_member_index);
                                            array_member_variable.name =
                                                format!("__{}", array_member_index);
                                            array_member_variable.kind = VariableKind::Indexed;
                                            array_member_variable.file =
                                                child_variable.file.clone();
                                            array_member_variable.line = child_variable.line;
                                            self.extract_type(
                                                array_member_type_node,
                                                child_variable,
                                                &mut array_member_variable,
                                                core,
                                                frame_base,
                                                program_counter,
                                            )?;
                                            array_member_variable.inclusion =
                                                VariableInclusion::Include;
                                            child_variable.add_child_variable(
                                                &mut array_member_variable,
                                                core,
                                            );
                                        }
                                    }
                                    other_attribute_value => {
                                        child_variable.set_value(format!(
                                            "UNIMPLEMENTED: Attribute Value for DW_AT_type {:?}",
                                            other_attribute_value
                                        ));
                                    }
                                }
                            }
                            None => {
                                child_variable.set_value(format!(
                                    "ERROR: No Attribute Value for DW_AT_type for variable {:?}",
                                    child_variable.name
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        child_variable.set_value(format!(
                            "ERROR: Failed to decode pointer reference: {:?}",
                            error
                        ));
                    }
                }
                Ok(())
            }
            gimli::DW_TAG_union_type => {
                // Recursively process a child types.
                // TODO: The DWARF does not currently hold information that allows decoding of which UNION arm is instantiated, so we have to display all available.
                self.process_tree(node, child_variable, core, frame_base, program_counter)?;
                if child_variable.children.is_none() {
                    // Empty structs don't have values.
                    child_variable.set_value(child_variable.type_name.clone());
                }
                Ok(())
            }
            gimli::DW_TAG_subroutine_type => {
                // The type_name will be found in the DW_AT_TYPE child of this entry.
                match node.entry().attr(gimli::DW_AT_type) {
                    Ok(optional_data_type_attribute) => match optional_data_type_attribute {
                        Some(data_type_attribute) => match data_type_attribute.value() {
                            gimli::AttributeValue::UnitRef(unit_ref) => {
                                let subroutine_type_node =
                                    self.unit.header.entry(&self.unit.abbreviations, unit_ref)?;
                                child_variable.type_name =
                                    match subroutine_type_node.attr(gimli::DW_AT_name) {
                                        Ok(optional_name_attr) => match optional_name_attr {
                                            Some(name_attr) => {
                                                extract_name(self.debug_info, name_attr.value())
                                            }
                                            None => "".to_owned(),
                                        },
                                        Err(error) => {
                                            format!(
                                                "ERROR: evaluating subroutine type name: {:?} ",
                                                error
                                            )
                                        }
                                    };
                            }
                            other_attribute_value => {
                                child_variable.set_value(format!(
                                    "UNIMPLEMENTED: Attribute Value for DW_AT_type {:?}",
                                    other_attribute_value
                                ));
                            }
                        },
                        None => {
                            child_variable.set_value("<No Return Value>".to_string());
                            child_variable.type_name = "".to_string();
                        }
                    },
                    Err(error) => {
                        child_variable.set_value(format!(
                            "ERROR: Failed to decode subroutine type reference: {:?}",
                            error
                        ));
                    }
                }
                Ok(())
            }
            // Do not expand this type.
            other => {
                child_variable.type_name =
                    format!("<UNIMPLEMENTED: type : {:?}>", other.static_string());
                child_variable.set_value(child_variable.type_name.clone());
                child_variable.children = None;
                Ok(())
            }
        }
    }

    /// Find the location using either DW_AT_location, or DW_AT_data_member_location, and store it in the &mut Variable. A value of 0 is a valid 0 reported from dwarf.
    fn extract_location(
        &self,
        node: &gimli::EntriesTreeNode<GimliReader>,
        parent_variable: &mut Variable,
        child_variable: &mut Variable,
        core: &mut Core<'_>,
        frame_base: u64,
    ) -> Result<(), DebugError> {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next().unwrap() {
            match attr.name() {
                gimli::DW_AT_location | gimli::DW_AT_data_member_location => {
                    match attr.value() {
                        gimli::AttributeValue::Exprloc(expression) => {
                            let pieces = match self.expr_to_piece(core, expression, frame_base) {
                                Ok(pieces) => pieces,
                                Err(err) => {
                                    child_variable.set_value(format!(
                                        "ERROR: expr_to_piece() failed with: {:?}",
                                        err
                                    ));
                                    vec![]
                                }
                            };
                            if pieces.is_empty() {
                                child_variable.memory_location = u64::MAX;
                                child_variable.set_value(format!(
                                    "ERROR: expr_to_piece() returned 0 results: {:?}",
                                    pieces
                                ));
                            } else if pieces.len() > 1 {
                                child_variable.memory_location = u64::MAX;
                                child_variable.set_value(format!("UNIMPLEMENTED: expr_to_piece() returned more than 1 result: {:?}", pieces));
                            } else {
                                match &pieces[0].location {
                                    Location::Empty => {
                                        child_variable.memory_location = 0_u64;
                                    }
                                    Location::Address { address } => {
                                        if *address == u32::MAX as u64 {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value("BUG: Cannot resolve due to rust-lang issue https://github.com/rust-lang/rust/issues/32574".to_string());
                                        } else {
                                            child_variable.memory_location = *address;
                                        }
                                    }
                                    Location::Value { value } => match value {
                                        gimli::Value::Generic(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::I8(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::U8(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::I16(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::U16(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::I32(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::U32(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::I64(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::U64(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::F32(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::F64(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                    },
                                    Location::Register { register: _ } => {
                                        // TODO: I commented the below, because it needs work to read the correct register, NOT just 0 // match core.read_core_reg(register.0)
                                        // let val = core
                                        //     .read_core_reg(register.0 as u16)
                                        //     .expect("Failed to read register from target");
                                        child_variable.memory_location = u64::MAX;
                                        child_variable.set_value("extract_location() found a register address as the location".to_owned());
                                    }
                                    l => {
                                        child_variable.memory_location = u64::MAX;
                                        child_variable.set_value(format!("UNIMPLEMENTED: extract_location() found a location type: {:?}", l));
                                    }
                                }
                            }
                        }
                        gimli::AttributeValue::Udata(offset_from_parent) => {
                            if parent_variable.memory_location != u64::MAX {
                                child_variable.memory_location =
                                    parent_variable.memory_location + offset_from_parent as u64;
                            } else {
                                child_variable.memory_location = offset_from_parent as u64;
                            }
                        }
                        other_attribute_value => {
                            child_variable.set_value(format!(
                                "ERROR: extract_location() Could not extract location from: {:?}",
                                other_attribute_value
                            ));
                        }
                    }
                }
                _other_attributes => {
                    // These will be handled elsewhere.
                }
            }
        }
        Ok(())
    }
}

fn extract_file(
    debug_info: &DebugInfo,
    unit: &gimli::Unit<GimliReader>,
    attribute_value: gimli::AttributeValue<GimliReader>,
) -> Option<String> {
    match attribute_value {
        gimli::AttributeValue::FileIndex(index) => unit.line_program.as_ref().and_then(|ilnp| {
            let header = ilnp.header();
            header.file(index).and_then(|file_entry| {
                file_entry.directory(header).map(|directory| {
                    format!(
                        "{}/{}",
                        extract_name(debug_info, directory),
                        extract_name(debug_info, file_entry.path_name())
                    )
                })
            })
        }),
        _ => None,
    }
}

/// If a DW_AT_byte_size attribute exists, return the u64 value, otherwise (including errors) return 0
fn extract_byte_size(
    _debug_info: &DebugInfo,
    di_entry: &DebuggingInformationEntry<GimliReader>,
) -> u64 {
    match di_entry.attr(gimli::DW_AT_byte_size) {
        Ok(optional_byte_size_attr) => match optional_byte_size_attr {
            Some(byte_size_attr) => match byte_size_attr.value() {
                gimli::AttributeValue::Udata(byte_size) => byte_size,
                other => {
                    log::warn!("UNIMPLEMENTED: DW_AT_byte_size value: {:?} ", other);
                    0
                }
            },
            None => 0,
        },
        Err(error) => {
            log::warn!(
                "Failed to extract byte_size: {:?} for debug_entry {:?}",
                error,
                di_entry.tag().static_string()
            );
            0
        }
    }
}
fn extract_line(
    _debug_info: &DebugInfo,
    attribute_value: gimli::AttributeValue<GimliReader>,
) -> Option<u64> {
    match attribute_value {
        gimli::AttributeValue::Udata(line) => Some(line),
        _ => None,
    }
}

fn extract_name(
    debug_info: &DebugInfo,
    attribute_value: gimli::AttributeValue<GimliReader>,
) -> String {
    match attribute_value {
        gimli::AttributeValue::DebugStrRef(name_ref) => {
            let name_raw = debug_info.dwarf.string(name_ref).unwrap();
            String::from_utf8_lossy(&name_raw).to_string()
        }
        gimli::AttributeValue::String(name) => String::from_utf8_lossy(&name).to_string(),
        other => format!("UNIMPLEMENTED: Evaluate name from {:?}", other),
    }
}

pub(crate) fn _print_all_attributes(
    core: &mut Core<'_>,
    frame_base: Option<u64>,
    dwarf: &gimli::Dwarf<DwarfReader>,
    unit: &gimli::Unit<DwarfReader>,
    tag: &gimli::DebuggingInformationEntry<DwarfReader>,
    print_depth: usize,
) {
    let mut attrs = tag.attrs();

    while let Some(attr) = attrs.next().unwrap() {
        for _ in 0..(print_depth) {
            print!("\t");
        }
        print!("{}: ", attr.name());

        use gimli::AttributeValue::*;

        match attr.value() {
            Addr(a) => println!("0x{:08x}", a),
            DebugStrRef(_) => {
                let val = dwarf.attr_string(unit, attr.value()).unwrap();
                println!("{}", std::str::from_utf8(&val).unwrap());
            }
            Exprloc(e) => {
                let mut evaluation = e.evaluation(unit.encoding());

                // go for evaluation
                let mut result = evaluation.evaluate().unwrap();

                loop {
                    use gimli::EvaluationResult::*;

                    result = match result {
                        Complete => break,
                        RequiresMemory { address, size, .. } => {
                            let mut buff = vec![0u8; size as usize];
                            core.read(address as u32, &mut buff)
                                .expect("Failed to read memory");
                            match size {
                                1 => evaluation
                                    .resume_with_memory(gimli::Value::U8(buff[0]))
                                    .unwrap(),
                                2 => {
                                    let val = u16::from(buff[0]) << 8 | u16::from(buff[1]);
                                    evaluation
                                        .resume_with_memory(gimli::Value::U16(val))
                                        .unwrap()
                                }
                                4 => {
                                    let val = u32::from(buff[0]) << 24
                                        | u32::from(buff[1]) << 16
                                        | u32::from(buff[2]) << 8
                                        | u32::from(buff[3]);
                                    evaluation
                                        .resume_with_memory(gimli::Value::U32(val))
                                        .unwrap()
                                }
                                x => {
                                    log::error!(
                                        "Requested memory with size {}, which is not supported yet.",
                                        x
                                    );
                                    unimplemented!();
                                }
                            }
                        }
                        RequiresFrameBase => evaluation
                            .resume_with_frame_base(frame_base.unwrap())
                            .unwrap(),
                        RequiresRegister {
                            register,
                            base_type,
                        } => {
                            let raw_value = core
                                .read_core_reg(register.0 as u16)
                                .expect("Failed to read memory");

                            if base_type != gimli::UnitOffset(0) {
                                unimplemented!(
                                    "Support for units in RequiresRegister request is not yet implemented."
                                )
                            }
                            evaluation
                                .resume_with_register(gimli::Value::Generic(raw_value as u64))
                                .unwrap()
                        }
                        RequiresRelocatedAddress(address_index) => {
                            if address_index.is_zero() {
                                // This is a rust-lang bug for statics ... https://github.com/rust-lang/rust/issues/32574;
                                evaluation.resume_with_relocated_address(u64::MAX).unwrap()
                            } else {
                                // Use the address_index as an offset from 0, so just pass it into the next step.
                                evaluation
                                    .resume_with_relocated_address(address_index)
                                    .unwrap()
                            }
                        }
                        x => {
                            println!("print_all_attributes {:?}", x);
                            // x
                            todo!()
                        }
                    }
                }

                let result = evaluation.result();

                println!("Expression: {:x?}", &result[0]);
            }
            LocationListsRef(_) => {
                println!("LocationList");
            }
            DebugLocListsBase(_) => {
                println!(" LocationList");
            }
            DebugLocListsIndex(_) => {
                println!(" LocationList");
            }
            _ => {
                println!("print_all_attributes {:?}", attr.value());
            }
        }
    }
}
