#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#![feature(collections, libc, iter_arith, const_fn)]
#[macro_use]
extern crate macros;
extern crate util;
extern crate exec;
extern crate bsdlike_getopts as getopts;
extern crate collections;
extern crate libc;
extern crate macho_bind;
extern crate deps;
use std::default::Default;
use std::vec::Vec;
use std::mem::{replace, size_of, transmute};
use std::str::FromStr;
use std::cmp::max;
use util::{VecStrExt, MCRef, Swap, VecCopyExt, SliceExt, OptionExt, copy_memory, into_cow, IntStuff, Endian};
use macho_bind::*;
use exec::{arch, VMA, SymbolValue, ByteSliceIterator, DepLib, SourceLib, ErrorKind, err, SymbolSource, Exec};
use std::{u64, u32, usize};
use deps::vec_map::VecMap;
use std::collections::{HashSet, HashMap};
use std::borrow::Cow;
use util::{ByteString, ByteStr, FieldLens, Ext, Narrow, CheckMath, TrivialState};
use std::cell::Cell;

pub mod dyldcache;
use dyldcache::DyldCache;

// dont bother with the unions
deriving_swap!(
#[repr(C)]
#[derive(Copy)]
pub struct x_nlist {
    pub n_strx: uint32_t,
    pub n_type: uint8_t,
    pub n_sect: uint8_t,
    pub n_desc: int16_t,
    pub n_value: uint32_t,
}
);
impl Clone for x_nlist { fn clone(&self) -> Self { *self } }
deriving_swap!(
#[repr(C)]
#[derive(Copy)]
pub struct x_nlist_64 {
    pub n_strx: uint32_t,
    pub n_type: uint8_t,
    pub n_sect: uint8_t,
    pub n_desc: uint16_t,
    pub n_value: uint64_t,
}
);
impl Clone for x_nlist_64 { fn clone(&self) -> Self { *self } }
impl Default for x_nlist_64 {
    fn default() -> Self {
        x_nlist_64 { n_strx: 0, n_type: 0, n_sect: 0, n_desc: 0, n_value: 0 }
    }
}

pub const EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE: u32 = 2;


pub fn u32_to_prot(ip: u32) -> exec::Prot {
    exec::Prot {
        r: (ip & VM_PROT_READ) != 0,
        w: (ip & VM_PROT_WRITE) != 0,
        x: (ip & VM_PROT_EXECUTE) != 0,
    }
}

#[inline(always)]
// probably 100% pointless optimization
fn copy_nlist_from_slice(slice: &[u8], end: Endian) -> x_nlist_64 {
    let len = slice.len();
    let is64 = if len == size_of::<x_nlist_64>() { true }
          else if len == size_of::<x_nlist>() { false }
          else { panic!() };
    unsafe {
        let source32: *const x_nlist = std::mem::transmute(slice);
        let source64: *const x_nlist_64 = std::mem::transmute(slice);
        let (mut strx, mut typ, mut sect, mut desc) =
            ((*source64).n_strx, (*source64).n_type,
             (*source64).n_sect, (*source64).n_desc);
        let value;
        if end.needs_swap() {
            strx.bswap(); typ.bswap(); sect.bswap(); desc.bswap();
            value = if is64 {
                let mut v = (*source64).n_value; v.bswap(); v
            } else {
                let mut v = (*source32).n_value; v.bswap(); v as u64
            }
        } else {
            value = if is64 { (*source64).n_value } else { (*source32).n_value as u64 }
        }
        x_nlist_64 {
            n_strx: strx,
            n_type: typ,
            n_sect: sect,
            n_desc: desc,
            n_value: value,
        }
    }
}

fn copy_nlist_to_vec(vec: &mut Vec<u8>, nl: &x_nlist_64, end: Endian, is64: bool) {
    if is64 {
        util::copy_to_vec(vec, nl, end);
    } else {
        let nl32 = x_nlist {
            n_strx: nl.n_strx,
            n_type: nl.n_type,
            n_sect: nl.n_sect,
            n_desc: nl.n_desc as i16,
            n_value: nl.n_value.narrow().unwrap(),
        };
        util::copy_to_vec(vec, &nl32, end);
    }
}


fn exec_sym_to_nlist_64(sym: &exec::Symbol, strx: u32, ind_strx: Option<u32>, arch: arch::Arch, is_text: &mut FnMut() -> bool) -> Result<x_nlist_64, &'static str> {
    // some stuff is missing, like common symbols
    let mut res: x_nlist_64 = Default::default();
    if sym.is_weak {
        res.n_type |= if let SymbolValue::Undefined = sym.val { N_WEAK_REF } else { N_WEAK_DEF } as u8
    }
    if sym.is_public {
        res.n_type |= N_EXT as u8;
    }
    match sym.val {
        SymbolValue::Addr(vma) => {
            res.n_value = vma.0;
        },
        SymbolValue::Abs(vma) => {
            res.n_value = vma.0;
            res.n_type |= N_ABS as u8;
        },
        SymbolValue::ThreadLocal(vma) => {
            return Err("can't handle thread loval");
        },
        SymbolValue::Undefined(source) => {
            res.n_value = 0;
            res.n_type |= N_UNDF as u8;
            let ord = match source {
                SourceLib::None => 0,
                SourceLib::Flat => {
                    res.n_desc |= N_REF_TO_WEAK as u16;
                    DYNAMIC_LOOKUP_ORDINAL
                },
                SourceLib::Self_ => SELF_LIBRARY_ORDINAL,
                SourceLib::MainExecutable => EXECUTABLE_ORDINAL,
                SourceLib::Ordinal(xord) => xord + 1,
            };
            res.n_desc |= (ord << 8) as u16;
        },
        SymbolValue::Resolver(vma, None) => {
            res.n_value = vma.0;
            res.n_desc |= N_SYMBOL_RESOLVER as u16;
        },
        SymbolValue::Resolver(vma, Some(_)) => {
            return Err("can't handle resolver with stub");
        },
        SymbolValue::ReExport(name) => {
            res.n_value = ind_strx.unwrap().ext();
            res.n_type |= N_INDR as u8;
        },
    }
    if res.n_value & 1 != 0 && arch == arch::ARM && is_text() {
        res.n_value -= 1;
        res.n_desc |= N_ARM_THUMB_DEF as u16;
    }
    Ok(res)
}

fn file_array(buf: &MCRef, name: &str, off: u32, count: u32, elm_size: usize) -> MCRef {
    file_array_64(buf, name, off as u64, count as u64, elm_size)
}
fn file_array_64(buf: &MCRef, name: &str, mut off: u64, mut count: u64, elm_size: usize) -> MCRef {
    let elm_size = elm_size as u64;
    let buf_len = buf.len() as u64;
    if off > buf_len {
        errln!("warning: {} (offset {}, {} * {}b-sized elements) starts past end of file ({}))", name, off, count, elm_size, buf_len);
        off = 0;
        count = 0;
    } else if count > (buf_len - off) / elm_size {
        errln!("warning: {} (offset {}, {} * {}b-sized elements) extends past end of file ({})); truncating", name, off, count, elm_size, buf_len);
        count = (buf_len - off) / elm_size;
    }
    buf.slice(off as usize, (off + count * elm_size) as usize).unwrap()
}


pub struct DscTabs {
    pub symtab: MCRef,
    pub strtab: MCRef,
    pub start: u32,
    pub count: u32,
}

#[derive(Debug)]
pub enum LoadDylibKind {
    Normal = LC_LOAD_DYLIB as isize,
    Weak = LC_LOAD_WEAK_DYLIB as isize,
    Reexport = LC_REEXPORT_DYLIB as isize,
    Upward = LC_LOAD_UPWARD_DYLIB as isize,
}

pub struct LoadDylib {
    path: ByteString,
    kind: LoadDylibKind,
    timestamp: u32,
    current_version: PackedVersion,
    compatibility_version: PackedVersion,
}

struct PackedVersion(u32);
impl std::fmt::Display for PackedVersion {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        write!(fmt, "{}.{}.{}", self.0 >> 16, (self.0 >> 8) & 255, self.0 & 255)
    }
}


#[derive(Default)]
pub struct MachO {
    pub eb: exec::ExecBase,
    pub is64: bool,
    pub mh: mach_header,
    pub hdr_offset: usize,
    pub load_commands: Vec<MCRef>,
    pub load_dylib: Vec<LoadDylib>,
    pub dyld_base: Option<VMA>,

    // old-style symbol table:
    pub nlist_size: usize,
    pub symtab: MCRef,
    pub localsym: MCRef,
    pub extdefsym: MCRef,
    pub undefsym: MCRef,
    pub strtab: MCRef,
    pub dsc_tabs: Option<DscTabs>,
    pub toc: MCRef,
    pub modtab: MCRef,
    pub extrefsym: MCRef,
    pub indirectsym: MCRef,
    pub extrel: MCRef,
    pub locrel: MCRef,
    // new-style
    pub dyld_info_is_only: bool,
    pub dyld_rebase: MCRef,
    pub dyld_bind: MCRef,
    pub dyld_weak_bind: MCRef,
    pub dyld_lazy_bind: MCRef,
    pub dyld_export: MCRef,
    // linkedit_data_commands
    pub segment_split_info: MCRef,
    pub function_starts: MCRef,
    pub data_in_code: MCRef,
    pub linker_optimization_hint: MCRef,
    pub dylib_code_sign_drs: MCRef,
    pub code_signature: MCRef,

    _linkedit_bits: Option<[LinkeditBit; 22]>,
}

#[derive(PartialEq, Eq, Copy, Clone)]
enum WhichBind {
    Bind = 0,
    WeakBind = 1,
    LazyBind = 2
}

struct LinkeditBit {
    name: &'static str,
    self_field: FieldLens<MachO, MCRef>,
    cmd_id: u32,
    cmd_off_field_off: usize,
    cmd_count_field_off: usize,
    elm_size: usize,
    is_symtab: bool,
}

macro_rules! lbit {
    ($self_field:ident, $cmd_id:ident, $cmd_type:ty, $off_field:ident, $size_field:ident, $divi:expr, $is_symtab:expr) => {
        LinkeditBit {
            name: stringify!($self_field),
            self_field: field_lens!(MachO, $self_field),
            cmd_id: $cmd_id,
            cmd_off_field_off: offset_of!($cmd_type, $off_field),
            cmd_count_field_off: offset_of!($cmd_type, $size_field),
            elm_size: $divi,
            is_symtab: $is_symtab,
        }
    };
    ($self_field:ident, $cmd_id:ident, $cmd_type:ty, $off_field:ident, $size_field:ident, $divi:expr) => { lbit!($self_field, $cmd_id, $cmd_type, $off_field, $size_field, $divi, false) }
}

fn make_linkedit_bits(is64: bool) -> [LinkeditBit; 22] {
    let nlist_size = if is64 { size_of::<nlist_64>() } else { size_of::<nlist>() };
    [
        // section relocations here?
        lbit!(dyld_rebase, LC_DYLD_INFO, dyld_info_command, rebase_off, rebase_size, 1),
        lbit!(dyld_bind, LC_DYLD_INFO, dyld_info_command, bind_off, bind_size, 1),
        lbit!(dyld_weak_bind, LC_DYLD_INFO, dyld_info_command, weak_bind_off, weak_bind_size, 1),
        lbit!(dyld_lazy_bind, LC_DYLD_INFO, dyld_info_command, lazy_bind_off, lazy_bind_size, 1),
        lbit!(dyld_export, LC_DYLD_INFO, dyld_info_command, export_off, export_size, 1),

        lbit!(locrel, LC_DYSYMTAB, dysymtab_command, locreloff, nlocrel, size_of::<relocation_info>()),
        lbit!(segment_split_info, LC_SEGMENT_SPLIT_INFO, linkedit_data_command, dataoff, datasize, 1),
        lbit!(function_starts, LC_FUNCTION_STARTS, linkedit_data_command, dataoff, datasize, 1),
        lbit!(data_in_code, LC_DATA_IN_CODE, linkedit_data_command, dataoff, datasize, 1),

        lbit!(linker_optimization_hint, LC_LINKER_OPTIMIZATION_HINT, linkedit_data_command, dataoff, datasize, 1),
        lbit!(dylib_code_sign_drs, LC_DYLIB_CODE_SIGN_DRS, linkedit_data_command, dataoff, datasize, 1),

        lbit!(symtab, LC_SYMTAB, symtab_command, symoff, nsyms, nlist_size), // ...not first anymore?
        lbit!(extrel, LC_DYSYMTAB, dysymtab_command, extreloff, nextrel, size_of::<relocation_info>()),
        lbit!(indirectsym, LC_DYSYMTAB, dysymtab_command, indirectsymoff, nindirectsyms, 4),
        lbit!(strtab, LC_SYMTAB, symtab_command, stroff, strsize, 1),


        // ld doesn't actually generate these anymore, so can't tell where it should go
        lbit!(modtab, LC_DYSYMTAB, dysymtab_command, modtaboff, nmodtab,
              if is64 { size_of::<dylib_module_64>() } else { size_of::<dylib_module>() }),
        lbit!(toc, LC_DYSYMTAB, dysymtab_command, tocoff, ntoc, size_of::<dylib_table_of_contents>()),
        lbit!(extrefsym, LC_DYSYMTAB, dysymtab_command, extrefsymoff, nextrefsyms, size_of::<dylib_reference>()),

        // added at the end by codesign
        lbit!(code_signature, LC_CODE_SIGNATURE, linkedit_data_command, dataoff, datasize, 1),

        // order of these compared to the rest doesn't affect anything
        lbit!(localsym, LC_DYSYMTAB, dysymtab_command, ilocalsym, nlocalsym, nlist_size, /* is_symtab */ true),
        lbit!(extdefsym, LC_DYSYMTAB, dysymtab_command, iextdefsym, nextdefsym, nlist_size, /* is_symtab */ true),
        lbit!(undefsym, LC_DYSYMTAB, dysymtab_command, iundefsym, nundefsym, nlist_size, /* is_symtab */ true),
    ]
}

impl exec::Exec for MachO {
    fn get_exec_base<'a>(&'a self) -> &'a exec::ExecBase {
        &self.eb
    }

    fn get_symbol_list<'a>(&'a self, source: SymbolSource, specific: Option<&std::any::Any>) -> Vec<exec::Symbol<'a>> {
        assert!(specific.is_none());
        match source {
            SymbolSource::All => {
                let mut out = Vec::new();
                let mut skip_redacted = false;
                if let Some(DscTabs { ref symtab, ref strtab, start, count }) = self.dsc_tabs {
                    self.push_nlist_symbols(symtab.get(), strtab.get(), start as usize, count as usize, false, &mut out);
                    skip_redacted = true;
                }
                self.push_nlist_symbols(self.symtab.get(), self.strtab.get(), 0, self.symtab.len() / self.nlist_size, skip_redacted, &mut out);
                out
            },
            SymbolSource::Imported => {
                let mut out = Vec::new();
                {
                let which = Cell::new(WhichBind::Bind);
                let mut cb = |state: &ParseDyldBindState<'a>| {
                    if state.already_bound_this_symbol { return; }
                    out.push(exec::Symbol {
                        name: some_or!(state.symbol, { return; }).into(),
                        is_public: true,
                        // XXX what about BIND_SYMBOL_FLAGS_*WEAK*?
                        is_weak: which.get() == WhichBind::WeakBind,
                        val: SymbolValue::Undefined(state.source_dylib),
                        size: None,
                        private: 0,
                    });
                };
                self.parse_dyld_bind(self.dyld_bind.get(), which.get(), &mut cb);
                which.set(WhichBind::WeakBind);
                self.parse_dyld_bind(self.dyld_weak_bind.get(), which.get(), &mut cb);
                which.set(WhichBind::LazyBind);
                self.parse_dyld_bind(self.dyld_lazy_bind.get(), which.get(), &mut cb);
                }
                out
            },
            SymbolSource::Exported => {
                let mut out = Vec::new();
                self.parse_dyld_export(self.dyld_export.get(), &mut |name: &ByteStr, addr: VMA, flags: u32, resolver: Option<VMA>, reexport: Option<(u64, &'a ByteStr)>, offset: usize| {
                    out.push(exec::Symbol {
                        name: name.to_owned().into(),
                        is_public: true,
                        is_weak: flags & EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION != 0,
                        val: if let Some((_, name)) = reexport {
                            SymbolValue::ReExport(name.into())
                        } else if let Some(resolver) = resolver {
                            SymbolValue::Resolver(resolver, Some(addr))
                        } else { match flags & EXPORT_SYMBOL_FLAGS_KIND_MASK {
                            EXPORT_SYMBOL_FLAGS_KIND_REGULAR | 3 => SymbolValue::Addr(addr),
                            EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL => SymbolValue::ThreadLocal(addr),
                            EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE => SymbolValue::Abs(addr),
                            _ => panic!("muri")
                        } },
                        size: None,
                        private: offset,
                    });
                });
                out
            },
        }
    }

    fn get_dep_libs(&self) -> Cow<[DepLib]> {
        let dls = self.load_dylib.iter().enumerate().map(|(i, ld)| DepLib {
            path: (&*ld.path).into(),
            private: i,
        }).collect::<Vec<_>>();
        dls.into()
    }
    fn describe_dep_lib(&self, dl: &DepLib) -> String {
        let ld = &self.load_dylib[dl.private];
        format!("{}{} timestamp={} cur={} compat={}",
                ld.path,
                match ld.kind {
                    LoadDylibKind::Normal   => "",
                    LoadDylibKind::Weak     => " [weak]",
                    LoadDylibKind::Reexport => " [reexport]",
                    LoadDylibKind::Upward   => " [upward]",
                },
                ld.timestamp,
                ld.current_version,
                ld.compatibility_version)
    }

    fn as_any(&self) -> &std::any::Any { self as &std::any::Any }
}

pub const X_CPU_TYPE_ANY: u32 = 0xffffffff;
pub const X_CPU_SUBTYPE_MULTIPLE: u32 = 0xffffffff;

fn mach_arch_desc(cputype: i32, cpusubtype: i32) -> Option<&'static str> {
    let cputype = cputype as u32;
    let cpusubtype = cpusubtype as u32;
    Some(match (cputype, cpusubtype & !0x80000000) {
        (CPU_TYPE_HPPA, CPU_SUBTYPE_HPPA_ALL) => "hppa",
        (CPU_TYPE_I386, CPU_SUBTYPE_I386_ALL) => "i386",
        (CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_ALL) => "x86_64",
        (CPU_TYPE_I860, CPU_SUBTYPE_I860_ALL) => "i860",
        (CPU_TYPE_MC680x0, CPU_SUBTYPE_MC680x0_ALL) => "m68k",
        (CPU_TYPE_MC88000, CPU_SUBTYPE_MC88000_ALL) => "m88k",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_ALL) => "ppc",
        (CPU_TYPE_POWERPC64, CPU_SUBTYPE_POWERPC_ALL) => "ppc64",
        (CPU_TYPE_SPARC, CPU_SUBTYPE_SPARC_ALL) => "sparc",
        (CPU_TYPE_ARM, CPU_SUBTYPE_ARM_ALL) => "arm",
        (X_CPU_TYPE_ANY, X_CPU_SUBTYPE_MULTIPLE) => "any",
        (CPU_TYPE_HPPA, CPU_SUBTYPE_HPPA_7100LC) => "hppa7100LC",
        (CPU_TYPE_MC680x0, CPU_SUBTYPE_MC68030_ONLY) => "m68030",
        (CPU_TYPE_MC680x0, CPU_SUBTYPE_MC68040) => "m68040",
        (CPU_TYPE_I386, CPU_SUBTYPE_486) => "i486",
        (CPU_TYPE_I386, CPU_SUBTYPE_486SX) => "i486SX",
        (CPU_TYPE_I386, CPU_SUBTYPE_PENT) => "pentium",
        (CPU_TYPE_I386, CPU_SUBTYPE_PENTPRO) => "pentpro",
        (CPU_TYPE_I386, CPU_SUBTYPE_PENTII_M3) => "pentIIm3",
        (CPU_TYPE_I386, CPU_SUBTYPE_PENTII_M5) => "pentIIm5",
        (CPU_TYPE_I386, CPU_SUBTYPE_PENTIUM_4) => "pentium4",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_601) => "ppc601",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_603) => "ppc603",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_603e) => "ppc603e",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_603ev) => "ppc603ev",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_604) => "ppc604",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_604e) => "ppc604e",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_750) => "ppc750",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_7400) => "ppc7400",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_7450) => "ppc7450",
        (CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_970) => "ppc970",
        (CPU_TYPE_POWERPC64, CPU_SUBTYPE_POWERPC_970) => "ppc970-64",
        (CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V4T) => "armv4t",
        (CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V5TEJ) => "armv5",
        (CPU_TYPE_ARM, CPU_SUBTYPE_ARM_XSCALE) => "xscale",
        (CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V6) => "armv6",
        (CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V7) => "armv7",
        (CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V7F) => "armv7f",
        (CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V7S) => "armv7s",
        (CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V7K) => "armv7k",
        (CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_ALL) => "arm64",
        (CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_V8) => "arm64v8",
        (X_CPU_TYPE_ANY, CPU_SUBTYPE_LITTLE_ENDIAN) => "little",
        (X_CPU_TYPE_ANY, CPU_SUBTYPE_BIG_ENDIAN) => "big",
        _ => return None,
    })
}

fn fixup_segment_overflow(seg: &mut exec::Segment, sixtyfour: bool) {
    if sixtyfour {
        if seg.vmsize > exec::VMA(u64::MAX) - seg.vmaddr {
            errln!("warning: vmaddr+vmsize overflow: {}+0x{:x}; truncating", seg.vmaddr, seg.vmsize);
            seg.vmsize = exec::VMA(u64::MAX) - seg.vmaddr;
        }
        if seg.filesize > u64::MAX - seg.fileoff {
            errln!("warning: fileoff+filesize overflow: 0x{:x}+0x{:x}; truncating", seg.fileoff, seg.filesize);
            seg.filesize = u64::MAX - seg.fileoff;
        }
    } else {
        if seg.vmsize > exec::VMA(u32::MAX as u64) - seg.vmaddr {
            errln!("warning: vmaddr+vmsize 32-bit overflow: {}+0x{:x}; we're ok though", seg.vmaddr, seg.vmsize);
        }
        if seg.filesize > u32::MAX as u64 - seg.fileoff {
            errln!("warning: fileoff+filesize 32-bit overflow: 0x{:x}+0x{:x}; we're ok though", seg.fileoff, seg.filesize);
        }

    }
}

fn seg_name_to_macho(seg: &exec::Segment, error_pfx: &str) -> [libc::c_char; 16] {
    let mut name: &ByteStr = if let Some(ref name) = seg.name { &**name } else { ByteStr::from_str("") };
    if name.len() > 16 {
        errln!("warning: {} name '{}' is too long, truncating", error_pfx, name);
        name = &name[..16];
    }
    let mut segname: [libc::c_char; 16] = [0; 16];
    for (i, b) in name.iter().enumerate() { segname[i] = *b as i8; }
    segname
}

trait OKOrTruncated<T> { fn ok_or_truncated(self) -> exec::ExecResult<T>; }
impl<T> OKOrTruncated<T> for Option<T> {
    fn ok_or_truncated(self) -> exec::ExecResult<T> {
        if let Some(a) = self { Ok(a) } else { err(ErrorKind::BadData, "truncated") }
    }
}

struct ParseDyldBindState<'s> {
    source_dylib: SourceLib,
    addr: Option<VMA>,
    seg_end: VMA,
    addend: i64,
    typ: u8,
    symbol: Option<&'s ByteStr>,
    already_bound_this_symbol: bool,
}

struct ReaggregatedSyms {
    localsym: Vec<u8>,
    extdefsym: Vec<u8>,
    undefsym: Vec<u8>,
    strtab: Vec<u8>,
}

impl MachO {
    pub fn new(mc: MCRef, do_lcs: bool, hdr_offset: usize) -> exec::ExecResult<MachO> {
        let mut me: MachO = Default::default();
        me.hdr_offset = hdr_offset;
        let mut lc_off = try!(hdr_offset.checked_add(size_of::<mach_header>()).ok_or_truncated());
        {
            let buf = mc.get();
            if buf.len() < lc_off { return err(ErrorKind::BadData, "truncated"); }
            let magic: u32 = util::copy_from_slice(&buf[hdr_offset..hdr_offset+4], util::BigEndian);
            let is64; let end;
            match magic {
                0xfeedface => { end = util::BigEndian; is64 = false; }
                0xfeedfacf => { end = util::BigEndian; is64 = true; }
                0xcefaedfe => { end = util::LittleEndian; is64 = false; }
                0xcffaedfe => { end = util::LittleEndian; is64 = true; }
                _ => return err(ErrorKind::BadData, "bad magic")
            }
            me.eb.endian = end;
            me.is64 = is64;
            me.mh = util::copy_from_slice(&buf[hdr_offset..lc_off], end);
            // useless 'reserved' field
            if is64 { lc_off += 4; }
        }
        me._linkedit_bits = Some(make_linkedit_bits(me.is64)); /* :( */
        me.parse_header();
        me.eb.whole_buf = Some(mc.clone());
        if do_lcs {
            me.parse_load_commands(lc_off, &mc);
        }
        Ok(me)
    }

    pub fn subtype_desc(&self) -> Option<&'static str> {
        mach_arch_desc(self.mh.cputype, self.mh.cpusubtype)
    }

    pub fn desc(&self) -> String {
        let ft_desc = match self.mh.filetype {
            MH_OBJECT => "object",
            MH_EXECUTE => "executable",
            MH_CORE => "core",
            MH_DYLIB => "dylib",
            MH_DYLINKER => "dylinker",
            MH_BUNDLE => "bundle",
            MH_DSYM => "dSYM",
            MH_KEXT_BUNDLE => "kext",
            _ => "<unknown filetype>"
        };
        let st_desc: Cow<str> = match self.subtype_desc() {
            Some(d) => d.into(),
            None => format!("<unknown cpu {}/{}>", self.mh.cputype, self.mh.cpusubtype).into()
        };
        format!("Mach-O {}/{}", ft_desc, st_desc)
    }

    fn linkedit_bits(&self) -> &[LinkeditBit] {
        self._linkedit_bits.as_ref().unwrap()
    }

    fn parse_header(&mut self) {
        self.eb.arch = match self.mh.cputype as u32 {
            CPU_TYPE_X86 => arch::X86,
            CPU_TYPE_X86_64 => arch::X86_64,
            CPU_TYPE_ARM => arch::ARM,
            CPU_TYPE_ARM64 => arch::AArch64,
            CPU_TYPE_POWERPC => arch::PowerPC,
            CPU_TYPE_POWERPC64 => arch::PowerPC,
            // Even if we don't know the arch, we can at least do something.
            _ => arch::UnknownArch,
        }
        // we don't really care about cpusubtype but could fill it in
    }

    fn parse_load_commands(&mut self, mut lc_off: usize, mc: &MCRef) {
        let self_ = self as *mut _;
        self.nlist_size = if self.is64 { size_of::<nlist_64>() } else { size_of::<nlist>() };
        let end = self.eb.endian;
        let hdr_offset = self.hdr_offset as u64;
        let whole = mc.get();
        let mut segi: usize = 0;
        for lci in 0..self.mh.ncmds {
            let lc_data = some_or!(whole.slice_opt(lc_off, lc_off + 8),
                                   { errln!("warning: load commands truncated (couldn't read LC header)"); return; });
            let lc: load_command = util::copy_from_slice(lc_data, end);
            let lc_mc = some_or!(mc.slice(lc_off, lc_off + lc.cmdsize as usize),
                                 { errln!("warning: load commands truncated (cmdsize {} too high?)", lc.cmdsize); return; });
            let lc_buf = lc_mc.get();
            let mut do_segment = |is64: bool, segs: &mut Vec<exec::Segment>, sects: &mut Vec<exec::Segment>| {
                branch!(if (is64) {
                    type segment_command_x = segment_command_64;
                    type section_x = section_64;
                } else {
                    type segment_command_x = segment_command;
                    type section_x = section;
                } then {
                    let mut off = size_of::<segment_command_x>();
                    let sc_data = some_or!(lc_buf.slice_opt(0, off),
                                           { errln!("warning: segment command too small; skipping"); return; });
                    let sc: segment_command_x = util::copy_from_slice(sc_data, end);
                    let segprot = u32_to_prot(sc.initprot as u32);
                    let data: Option<MCRef> = mc.slice(sc.fileoff as usize, (sc.fileoff+sc.filesize) as usize);
                    let mut seg = exec::Segment {
                        vmaddr: VMA(sc.vmaddr as u64),
                        vmsize: sc.vmsize as u64,
                        fileoff: sc.fileoff as u64,
                        filesize: sc.filesize as u64,
                        name: Some(util::from_cstr(&sc.segname).to_owned()),
                        prot: segprot,
                        data: data,
                        seg_idx: None,
                        private: lci.ext(),
                    };
                    if seg.fileoff == 0 { seg.fileoff = hdr_offset; }
                    fixup_segment_overflow(&mut seg, is64);
                    segs.push(seg);
                    for secti in 0..sc.nsects {
                        let s: section_x = util::copy_from_slice(&lc_buf[off..off + size_of::<section_x>()], end);
                        let mut seg = exec::Segment {
                            vmaddr: VMA(s.addr as u64),
                            vmsize: s.size as u64,
                            fileoff: s.offset as u64,
                            filesize: if s.offset != 0 { s.size as u64 } else { 0 },
                            name: Some(util::from_cstr(&s.sectname).to_owned()),
                            prot: segprot,
                            data: None,
                            seg_idx: Some(segi),
                            private: secti.ext(),
                        };
                        if seg.fileoff == 0 { seg.fileoff = hdr_offset; }
                        fixup_segment_overflow(&mut seg, is64);
                        sects.push(seg);
                        off += size_of::<section_x>();
                    }
                });
                segi += 1;
            };
            match lc.cmd {
                LC_SEGMENT => do_segment(false, &mut self.eb.segments, &mut self.eb.sections),
                LC_SEGMENT_64 => do_segment(true, &mut self.eb.segments, &mut self.eb.sections),
                LC_DYLD_INFO | LC_DYLD_INFO_ONLY | LC_SYMTAB | LC_DYSYMTAB | 
                LC_FUNCTION_STARTS | LC_DATA_IN_CODE | LC_DYLIB_CODE_SIGN_DRS |
                LC_SEGMENT_SPLIT_INFO | LC_LINKER_OPTIMIZATION_HINT | LC_CODE_SIGNATURE => {
                    for fb in self.linkedit_bits() {
                        if lc.cmd == fb.cmd_id || (lc.cmd == LC_DYLD_INFO_ONLY && fb.cmd_id == LC_DYLD_INFO) {
                            let mcref: &mut MCRef = unsafe { fb.self_field.get_mut_unsafe(self_) };
                            let (off_data, count_data) =
                                some_or!(         lc_buf.slice_opt(fb.cmd_off_field_off, fb.cmd_off_field_off+4)
                                         .and_tup(lc_buf.slice_opt(fb.cmd_count_field_off, fb.cmd_count_field_off+4)),
                                         { errln!("warning: load command too small for offset/size of {}", fb.name); continue; });
                            let mut off: u32 = util::copy_from_slice(off_data, end);
                            let count: u32 = util::copy_from_slice(count_data, end);
                            let buf = if fb.is_symtab {
                                off *= fb.elm_size as u32;
                                &self.symtab
                            } else {
                                self.eb.whole_buf.as_ref().unwrap()
                            };
                            *mcref = file_array(buf, fb.name, off, count, fb.elm_size);
                        }
                    }
                },
                LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB | LC_LOAD_UPWARD_DYLIB => {
                    if (lc.cmdsize as usize) < size_of::<dylib_command>() {
                        errln!("warning: LC_LOAD_DYLIB command too small");
                    } else {
                        let dc: dylib_command = util::copy_from_slice(&lc_buf[..size_of::<dylib_command>()], end);
                        let offset: u32 = unsafe { transmute(dc.dylib.name) };
                        let offset = offset as usize;
                        let name = if offset <= lc_buf.len() {
                            let rest = &lc_buf[offset..];
                            let n = util::from_cstr(rest);
                            if n.len() == rest.len() {
                                errln!("warning: LC_LOAD_DYLIB name runs off end");
                            }
                            n
                        } else {
                            errln!("warning: LC_LOAD_DYLIB invalid offset");
                            ByteStr::from_str("<err>")
                        };
                        self.load_dylib.push(LoadDylib {
                            path: ByteString::new(name),
                            kind: unsafe { transmute(lc.cmd) },
                            timestamp: dc.dylib.timestamp,
                            current_version: PackedVersion(dc.dylib.current_version),
                            compatibility_version: PackedVersion(dc.dylib.compatibility_version),
                        });
                    }
                },
                _ => ()
            }
            lc_off += lc.cmdsize as usize;
            self.load_commands.push(lc_mc.clone()); // unnecessary clone
        }
        self.update_dyld_base();
    }

    pub fn update_dyld_base(&mut self) {
        for seg in &self.eb.segments {
            if seg.fileoff == self.hdr_offset.ext() && seg.filesize != 0 {
                self.dyld_base = Some(seg.vmaddr);
                break;
            }
        }
    }

    fn push_nlist_symbols<'a>(&self, symtab: &[u8], strtab: &'a [u8], start: usize, count: usize, skip_redacted: bool, out: &mut Vec<exec::Symbol<'a>>) {
        let mut off = start * self.nlist_size;
        for _ in start..start+count {
            let slice = &symtab[off..off + self.nlist_size];
            let nl = copy_nlist_from_slice(slice, self.eb.endian);

            let _n_pext = (nl.n_type as u32 & N_PEXT) != 0;
            let _n_stab = (nl.n_type as u32 & N_STAB) >> 5;
            let n_type = nl.n_type as u32 & N_TYPE;
            let weak = (nl.n_desc as u32 & (N_WEAK_REF | N_WEAK_DEF)) != 0;
            let public = (nl.n_type as u32 & N_EXT) != 0;
            let name = if nl.n_strx == 0 { ByteStr::from_str("") }
                                    else { util::from_cstr(&strtab[nl.n_strx as usize..]) };
            let vma = VMA(nl.n_value as u64);
            let vma = if nl.n_desc as u32 & N_ARM_THUMB_DEF != 0 { vma | 1 } else { vma };
            let is_obj = self.mh.filetype == MH_OBJECT;
            let ord = (nl.n_desc >> 8) as u32;
            let val =
                if nl.n_desc as u32 & N_SYMBOL_RESOLVER != 0 && is_obj {
                    SymbolValue::Resolver(vma, None)
                } else if n_type == N_UNDF {
                    SymbolValue::Undefined(if is_obj {
                        SourceLib::None
                    } else if (nl.n_desc as u32 & N_REF_TO_WEAK != 0) || ord == DYNAMIC_LOOKUP_ORDINAL {
                        SourceLib::Flat
                    } else if ord == SELF_LIBRARY_ORDINAL {
                        SourceLib::Self_
                    } else if ord == EXECUTABLE_ORDINAL {
                        SourceLib::MainExecutable
                    } else {
                        SourceLib::Ordinal((ord - 1) as u32)
                    })
                } else if n_type == N_INDR {
                    assert!(nl.n_value <= 0xfffffffe); // XXX why?
                    let indr_name = util::from_cstr(&strtab[nl.n_value as usize..]);
                    SymbolValue::ReExport(into_cow(indr_name))
                } else if n_type == N_ABS {
                    SymbolValue::Abs(vma)
                } else {
                    SymbolValue::Addr(vma)
                };
            if !(skip_redacted && name == ByteStr::from_bytes(b"<redacted>")) {
                out.push(exec::Symbol {
                    name: into_cow(name),
                    is_public: public,
                    is_weak: weak,
                    val: val,
                    size: None,
                    private: off,
                })
            }
            off += self.nlist_size;
        }
    }

    pub fn page_size(&self) -> u64 {
        0x1000 // XXX
    }

    pub fn rewhole(&mut self) {
        let new_size = self.eb.segments.iter().map(|seg| seg.fileoff + seg.filesize).max().unwrap_or(0);
        let mut mm = util::MemoryMap::with_fd_size(None, new_size as usize);
        {
            let buf = mm.get_mut();
            for seg in &self.eb.segments {
                let data = seg.get_data();
                assert_eq!(seg.filesize, data.len() as u64);
                copy_memory(data, &mut buf[seg.fileoff as usize..seg.fileoff as usize + seg.filesize as usize]);
            }
        }
        self.eb.whole_buf = Some(MCRef::with_mm(mm));
    }

    pub fn reallocate(&mut self) -> exec::ExecResult<()> {
        self.code_signature = MCRef::default();
        self.xsym_to_symtab();
        let page_size = self.page_size();

        let (linkedit, linkedit_allocs) = self.reallocate_linkedit();

        let mut linkedit_idx: Option<usize> = None;
        let mut text_idx: Option<usize> = None;
        for (i, seg) in self.eb.segments.iter_mut().enumerate() {
            if seg.name.as_ref().map(|s| &s[..]) == Some(ByteStr::from_str("__LINKEDIT")) {
                linkedit_idx = Some(i);
                seg.vmsize = (linkedit.len() as u64).align_to(page_size);
                seg.filesize = linkedit.len() as u64;
                seg.data = Some(MCRef::with_data(&linkedit[..]));
            } else if seg.fileoff == self.hdr_offset.ext() && seg.filesize > 0 {
                text_idx = Some(i);
            }
        }
        let text_idx = some_or!(text_idx, {
            return err(ErrorKind::BadData, "no text segment?");
        });
        if linkedit_idx.is_none() && linkedit.len() > 0 {
            return err(ErrorKind::Other, "allocating new segments in VM space not supported yet");
        }

        let initial_cmds = self.update_cmds(0, &linkedit_allocs);
        let cmds_len: usize = initial_cmds.iter().map(Vec::len).sum();
        let (text_fileoff, text_filesize) = {
            let text_seg = &self.eb.segments[text_idx];
            (text_seg.fileoff, text_seg.filesize)
        };
        // do we have enough space for the LCs?
        let cmds_space_end = max(self.mh.sizeofcmds as usize,
                                 self.eb.sections.iter()
                                                 .filter(|sect| sect.filesize != 0 &&
                                                                sect.seg_idx == Some(text_idx))
                                                 .map(|sect| sect.fileoff - text_fileoff)
                                                 .min().unwrap_or(0).narrow().unwrap());
        if cmds_space_end as u64 > text_filesize {
            return err(ErrorKind::BadData, "load commands go past __TEXT");
        }
        let header_size = if self.is64 { size_of::<mach_header_64>() } else { size_of::<mach_header>() };
        let cmds_space = some_or!(cmds_space_end.check_sub(header_size),
                                  return err(ErrorKind::BadData,
                                             "sizeofcmds too small"));
        let cmds_push = cmds_len.check_sub(cmds_space).unwrap_or(0);
        if cmds_push > 0 {
            return err(ErrorKind::Other, "need to slide in this case; not supported yet");
        }
        self.eb.segments[0].filesize += cmds_push as u64;

        self.reallocate_seg_offsets();

        let cmds = self.update_cmds(if let Some(li) = linkedit_idx { self.eb.segments[li].fileoff as usize } else { 0 }, &linkedit_allocs);
        assert_eq!(cmds_len, cmds.iter().map(Vec::len).sum());

        self.mh.ncmds = cmds.len() as u32;
        self.mh.sizeofcmds = cmds_len as u32;

        // update text/LC segment
        {
            let seg = &mut self.eb.segments[text_idx];
            let mut sbuf = Vec::new();
            sbuf.resize(seg.filesize as usize, 0);
            copy_memory(&seg.get_data()[cmds_space..], &mut sbuf[cmds_space + cmds_push..]);
            util::copy_to_slice(&mut sbuf[..size_of::<mach_header>()], &self.mh, self.eb.endian);
            let mut off = header_size;
            for cmd in &cmds {
                copy_memory(&cmd[..], &mut sbuf[off..off+cmd.len()]);
                off += cmd.len();
            }
            let smc = MCRef::with_data(&sbuf[..]);
            seg.data = Some(smc.clone());
            // update self.load_commands
            off = 0;
            self.load_commands.clear();
            for cmd in &cmds {
                self.load_commands.push(smc.slice(off, off+cmd.len()).unwrap());
                off += cmd.len();
            }
        }
        Ok(())
    }

    fn reallocate_linkedit(&self) -> (Vec<u8>, Vec<(usize, usize)>) {
        let mut linkedit: Vec<u8> = Vec::new();
        let mut allocs: Vec<(usize, usize)> = Vec::new();
        for fb in self.linkedit_bits() {
            let mcref: &MCRef = fb.self_field.get(self);
            let buf = mcref.get();
            if fb.is_symtab {
                allocs.push((mcref.offset_in(&self.symtab).unwrap(), buf.len()));
            } else {
                allocs.push((linkedit.len(), buf.len()));
                linkedit.extend_slice(buf);
            }
        }
        (linkedit, allocs)
    }

    fn reallocate_seg_offsets(&mut self) {
        let page_size = self.page_size();
        for sect in &mut self.eb.sections {
            if sect.filesize != 0 {
                sect.fileoff -= self.eb.segments[sect.seg_idx.expect("sect not belonging to seg?")].fileoff;
            }
        }
        let mut off: u64 = 0;
        for seg in &mut self.eb.segments {
            seg.fileoff = off;
            off += seg.filesize.align_to(page_size);
        }
        for sect in &mut self.eb.sections {
            if sect.filesize == 0 {
                sect.fileoff = 0;
            } else {
                sect.fileoff += self.eb.segments[sect.seg_idx.unwrap()].fileoff;
            }
        }
    }

    fn xsym_to_symtab(&mut self) {
        let mut new_vec = self.localsym.get().to_owned();
        new_vec.extend_slice(self.extdefsym.get());
        new_vec.extend_slice(self.undefsym.get());
        let mc = MCRef::with_data(&new_vec[..]);
        self.symtab = mc;
        let parts = [
            (0, self.localsym.len()),
            (self.localsym.len(), self.extdefsym.len()),
            (self.localsym.len() + self.extdefsym.len(), self.undefsym.len()),
        ];
        self.symtab_to_xsym(&parts);
    }

    fn symtab_to_xsym(&mut self, parts: &[(usize, usize); 3]) {
        let mc = &self.symtab;
        self.localsym = mc.slice(parts[0].0, parts[0].0+parts[0].1).unwrap();
        self.extdefsym = mc.slice(parts[1].0, parts[1].0+parts[1].1).unwrap();
        self.undefsym = mc.slice(parts[2].0, parts[2].0+parts[2].1).unwrap();
    }

    fn update_cmds(&self, linkedit_off: usize, linkedit_allocs: &[(usize, usize)]) -> Vec<Vec<u8>> {
        let mut cmds: Vec<Vec<u8>> = Vec::new();
        let (mut existing_segs, extra_segs) = self.update_seg_cmds();
        let mut insert_extra_segs_idx: Option<usize> = None;
        let end = self.eb.endian;

        let mut dyld_info_cmd_id = LC_DYLD_INFO_ONLY;
        for cmd in &self.load_commands {
            let cmd = cmd.get();
            let cmd_id: u32 = util::copy_from_slice(&cmd[..4], self.eb.endian);
            if cmd_id == LC_DYLD_INFO {
                dyld_info_cmd_id = cmd_id;
            }
        }

        let mut bit_cmds = [
            (LC_DYLD_INFO, size_of::<dyld_info_command>()),
            (LC_SYMTAB, size_of::<symtab_command>()),
            (LC_DYSYMTAB, size_of::<dysymtab_command>()),
            (LC_DYLIB_CODE_SIGN_DRS, size_of::<linkedit_data_command>()),
            (LC_SEGMENT_SPLIT_INFO, size_of::<linkedit_data_command>()),
            (LC_FUNCTION_STARTS, size_of::<linkedit_data_command>()),
            (LC_DATA_IN_CODE, size_of::<linkedit_data_command>()),
            (LC_LINKER_OPTIMIZATION_HINT, size_of::<linkedit_data_command>()),
            // for consistency, code_signature is here, but reallocate() nukes it
            (LC_CODE_SIGNATURE, size_of::<linkedit_data_command>()),
        ].iter().map(|&(cmd, cmdsize)| {
            let mut buf: Vec<u8> = Vec::new();
            buf.resize(cmdsize, 0);
            util::copy_to_slice(&mut buf[0..8], &load_command {
                cmd: if cmd == LC_DYLD_INFO { dyld_info_cmd_id } else { cmd },
                cmdsize: cmdsize as u32,
            }, end);
            let mut got_any = false;
            for (fb, &(mut off, len)) in self.linkedit_bits().iter().zip(linkedit_allocs) {
                if cmd == fb.cmd_id {
                    if len == 0 {
                        off = 0;
                    } else if fb.is_symtab {
                        off /= fb.elm_size;
                    } else {
                        off += linkedit_off;
                    }
                    util::copy_to_slice(&mut buf[fb.cmd_off_field_off..fb.cmd_off_field_off+4], &(off as u32), end);
                    util::copy_to_slice(&mut buf[fb.cmd_count_field_off..fb.cmd_count_field_off+4], &((len / fb.elm_size) as u32), end);
                    if len > 0 {
                        got_any = true;
                    }
                }
            }
            if got_any { Some(buf) } else { None }
        }).collect::<Vec<_>>();


        for (lci, cmd) in self.load_commands.iter().enumerate() {
            let cmd = cmd.get();
            let cmd_id: u32 = util::copy_from_slice(&cmd[..4], self.eb.endian);
            if cmd_id != LC_SEGMENT && cmd_id != LC_SEGMENT_64 && insert_extra_segs_idx == None {
                insert_extra_segs_idx = Some(cmds.len());
            }
            match cmd_id {
                LC_SEGMENT | LC_SEGMENT_64 => {
                    if let Some(new_cmd) = existing_segs.remove(lci) {
                        cmds.push(new_cmd);
                    }
                },
                _ => {
                    if let Some(idx) = match cmd_id {
                        LC_DYLD_INFO | LC_DYLD_INFO_ONLY => Some(0),
                        LC_SYMTAB => Some(1),
                        LC_DYSYMTAB => Some(2),
                        LC_DYLIB_CODE_SIGN_DRS => Some(3),
                        LC_SEGMENT_SPLIT_INFO => Some(4),
                        LC_FUNCTION_STARTS => Some(5),
                        LC_DATA_IN_CODE => Some(6),
                        LC_LINKER_OPTIMIZATION_HINT => Some(7),
                        LC_CODE_SIGNATURE => Some(8),
                        _ => None
                    } {
                        if let Some(new_cmd) = bit_cmds[idx].take() {
                            cmds.push(new_cmd);
                        }
                        continue;
                    }
                    cmds.push(cmd.to_owned());
                },
            }
        }
        for cmd in bit_cmds {
            if let Some(cmd) = cmd { cmds.push(cmd); }
        }
        let mut insert_extra_segs_idx = insert_extra_segs_idx.unwrap_or(cmds.len());
        for (_, new_cmd) in existing_segs.into_iter() {
            cmds.insert(insert_extra_segs_idx, new_cmd);
            insert_extra_segs_idx += 1;
        }
        for new_cmd in extra_segs.into_iter() {
            cmds.insert(insert_extra_segs_idx, new_cmd);
            insert_extra_segs_idx += 1;
        }
        cmds
    }

    fn update_seg_cmds(&self) -> (VecMap<Vec<u8>>, Vec<Vec<u8>>) {
        let mut existing_segs = VecMap::new();
        let mut extra_segs = Vec::new();
        for (segi, seg) in self.eb.segments.iter().enumerate() {
            let lci = seg.private;
            let cmd = if self.is64 { LC_SEGMENT_64 } else { LC_SEGMENT };
            let segname = seg_name_to_macho(&seg, "update_seg_cmds: segment");
            let mut new_cmd = Vec::<u8>::new();
            let olcbuf = if lci != usize::MAX { Some(self.load_commands[lci].get()) } else { None };
            branch!(if (self.is64) {
                type segment_command_x = segment_command_64;
                type section_x = section_64;
                type size_x = u64;
            } else {
                type segment_command_x = segment_command;
                type section_x = section;
                type size_x = u32;
            } then {
                let mut sc: segment_command_x = if let Some(ref lcbuf) = olcbuf {
                    util::copy_from_slice(&lcbuf[..size_of::<segment_command_x>()], self.eb.endian)
                } else {
                    segment_command_x {
                        maxprot: 7,
                        ..Default::default()
                    }
                };
                sc.cmd = cmd;
                sc.segname = segname;
                assert!(seg.vmaddr.0 <= !0 as size_x as u64);
                sc.vmaddr = seg.vmaddr.0 as size_x;
                sc.vmsize = seg.vmsize as size_x;
                sc.fileoff = seg.fileoff as size_x;
                sc.filesize = seg.filesize as size_x;
                sc.initprot = 0;
                if seg.prot.r { sc.initprot |= VM_PROT_READ as i32; }
                if seg.prot.w { sc.initprot |= VM_PROT_WRITE as i32; }
                if seg.prot.x { sc.initprot |= VM_PROT_EXECUTE as i32; }

                util::copy_to_vec(&mut new_cmd, &sc, self.eb.endian);

                let mut nsects: usize = 0;
                for sect in self.eb.sections.iter().filter(|sect| sect.seg_idx == Some(segi)) {
                    let mut snc: section_x = if sect.private != usize::MAX {
                        let off = size_of::<segment_command_x>() + sect.private * size_of::<section_x>();
                        util::copy_from_slice(&olcbuf.unwrap()[off..off+size_of::<section_x>()], self.eb.endian)
                    } else {
                        Default::default()
                    };
                    snc.segname = segname;
                    snc.sectname = seg_name_to_macho(&sect, "update_seg_cmds: section");
                    snc.addr = sect.vmaddr.0 as size_x;
                    snc.size = sect.vmsize as size_x;
                    if sect.filesize != sect.vmsize && sect.filesize != 0 {
                        errln!("warning: update_seg_cmds: section {} filesize != vmsize, using vmsize", sect.pretty_name());
                    }
                    snc.offset = sect.fileoff as u32;
                    util::copy_to_vec(&mut new_cmd, &snc, self.eb.endian);
                    nsects += 1;
                }
                sc.cmdsize = (size_of::<segment_command_x>() + nsects * size_of::<section_x>()) as u32;
                sc.nsects = nsects.narrow().unwrap();
            });
            if lci == usize::MAX {
                extra_segs.push(new_cmd);
            } else {
                existing_segs.insert(lci, new_cmd);
            }
        }
        (existing_segs, extra_segs)
    }

    fn parse_dyld_bind<'a>(&'a self, mut slice: &'a [u8], which: WhichBind, cb: &mut FnMut(&ParseDyldBindState<'a>)) {
        let pointer_size = if self.is64 { 8 } else { 4 };
        let leb = |slice_: &mut &[u8], signed| -> Option<u64> {
            let mut it = ByteSliceIterator(slice_);
            exec::read_leb128_inner_noisy(&mut it, signed, "parse_dyld_bind")
        };
        macro_rules! leb { ($signed:expr) => {
            if let Some(num) = leb(&mut slice, $signed) { num } else { return }
        } }
        let advance = |state: &mut ParseDyldBindState, amount: u64| {
            if let Some(addr) = state.addr {
                if amount > state.seg_end - addr {
                    errln!("warning: parse_dyld_bind: going out of range of segment");
                    state.addr = Some(addr + amount);
                } else {
                    state.addr = None;
                }
            }
        };
        let mut bind_advance = |state: &mut _, amount: u64| {
            cb(state);
            state.already_bound_this_symbol = true;
            advance(state, amount);
        };
        let mut state = ParseDyldBindState {
            source_dylib: SourceLib::None,
            addr: None,
            seg_end: VMA::default(),
            addend: 0,
            typ: if which == WhichBind::LazyBind { BIND_TYPE_POINTER as u8 } else { 0 },
            symbol: None,
            already_bound_this_symbol: false,
        };
        let set_dylib_ordinal = |state: &mut ParseDyldBindState, ord: u64| {
            let count = self.load_dylib.len().ext();
            state.source_dylib = if ord == 0 || ord > count {
                errln!("parse_dyld_bind: dylib ordinal out of range (ord={}, count={})", ord, count);
                SourceLib::None
            } else {
                SourceLib::Ordinal((ord - 1).narrow().unwrap())
            };
        };
        while !slice.is_empty() {
            let byte = slice[0];
            slice = &slice[1..];
            let immediate = byte & (BIND_IMMEDIATE_MASK as u8);
            let opcode = byte & (BIND_OPCODE_MASK as u8);
            match opcode as u32 {
                BIND_OPCODE_DONE => (),
                BIND_OPCODE_SET_DYLIB_ORDINAL_IMM => set_dylib_ordinal(&mut state, immediate as u64),
                BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB => set_dylib_ordinal(&mut state, leb!(false)),
                BIND_OPCODE_SET_DYLIB_SPECIAL_IMM => state.source_dylib = match immediate {
                    0x0 => SourceLib::Self_,
                    0xf => SourceLib::MainExecutable,
                    0xe => SourceLib::Flat,
                    _ => {
                        errln!("warning: parse_dyld_bind: unknown special source dylib -{}", 0x10 - immediate);
                        SourceLib::None
                    },
                },
                BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM => {
                    let name = some_or!(util::from_cstr_strict(slice), {
                        errln!("parse_dyld_bind: BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM: bad string");
                        break;
                    });
                    state.symbol = Some(name);
                    state.already_bound_this_symbol = false;
                    slice = &slice[name.len()+1..];
                },
                BIND_OPCODE_SET_TYPE_IMM => {
                    state.typ = immediate;
                    if immediate < 1 || immediate > 3 {
                        errln!("warning: parse_dyld_bind: unknown BIND_OPCODE_SET_TYPE_IMM type {}", immediate);
                    }
                },
                BIND_OPCODE_SET_ADDEND_SLEB => state.addend = leb!(true) as i64,
                BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                    let offset = leb!(false);
                    let seg = some_or!(self.eb.segments.get(immediate.ext()), {
                        errln!("parse_dyld_bind: BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB: bad segment index {}", immediate);
                        continue;
                    });
                    state.addr = Some(seg.vmaddr);
                    state.seg_end = seg.vmaddr + seg.vmsize;
                    advance(&mut state, offset);
                },
                BIND_OPCODE_ADD_ADDR_ULEB => advance(&mut state, leb!(false)),
                BIND_OPCODE_DO_BIND => bind_advance(&mut state, pointer_size),
                BIND_OPCODE_DO_BIND_ADD_ADDR_ULEB => bind_advance(&mut state, leb!(false)),
                BIND_OPCODE_DO_BIND_ADD_ADDR_IMM_SCALED =>
                    bind_advance(&mut state, (immediate as u64) * pointer_size + pointer_size),
                BIND_OPCODE_DO_BIND_ULEB_TIMES_SKIPPING_ULEB => {
                    let count = leb!(false);
                    let skip = leb!(false);
                    for _ in 0..count {
                        bind_advance(&mut state, skip);
                    }
                },
                _ => {
                    errln!("parse_dyld_bind: unknown bind opcode (byte=0x{:x})", byte);
                    break;
                }
            }
        }
    }
    fn parse_dyld_export<'a>(&'a self, dyld_export: &'a [u8], cb: &mut FnMut(&ByteStr, VMA, u32, Option<VMA>, Option<(u64, &'a ByteStr)>, usize)) {
        if dyld_export.is_empty() { return; }
        let mut seen = HashSet::with_hasher(TrivialState);
        let mut todo = vec![(0usize, ByteString::from_str(""))];
        let base_addr = some_or!(self.dyld_base, {
            errln!("warning: parse_dyld_export: no load command segment, lol");
            return;
        });
        while let Some((offset, prefix)) = todo.pop() {
            let mut slice = &dyld_export[offset..];
            let mut it = ByteSliceIterator(&mut slice);
            macro_rules! leb { ($it:expr) => {some_or!(exec::read_leb128_inner_noisy(&mut $it, false, "parse_dyld_export"), { continue; })} }
            let terminal_size = leb!(it);
            if terminal_size > it.0.len() as u64 {
                errln!("warning: parse_dyld_export: terminal_size too big");
                continue;
            }
            let mut following = &it.0[terminal_size as usize..];
            *it.0 = &it.0[..terminal_size as usize];
            if !it.0.is_empty() {
                let flags = leb!(it);
                if flags > std::u32::MAX as u64 {
                    errln!("warning: parse_dyld_export: way too many flags");
                    continue;
                }
                let flags = flags as u32;
                let kind = flags & EXPORT_SYMBOL_FLAGS_KIND_MASK;
                if kind == 3 {
                    errln!("warning: parse_dyld_export: unexpected symbol kind 3");
                }
                if flags > 0x1f {
                    errln!("warning: parse_dyld_export: unknown flags (flags=0x{:x})", flags);
                }
                let addr = (if kind == EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE { VMA(0) } else { base_addr })
                           + leb!(it);
                let resolver = if flags & EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER != 0 {
                    Some(base_addr + leb!(it))
                } else { None };
                let reexport = if flags & EXPORT_SYMBOL_FLAGS_REEXPORT != 0 {
                    if resolver.is_some() {
                        errln!("warning: parse_dyld_export: resolver /and/ reexport?");
                        continue;
                    }
                    let ord = leb!(it);
                    let name = some_or!(util::from_cstr_strict(it.0), {
                        errln!("warning: parse_dyld_export: invalid reexport name");
                        continue;
                    });
                    *it.0 = &it.0[name.len()+1..];
                    Some((ord, name))
                } else { None };
                cb(&prefix, addr, flags, resolver, reexport, offset);
                if !it.0.is_empty() {
                    errln!("warning: parse_dyld_export: excess terminal data");
                }
            }
            let mut it = ByteSliceIterator(&mut following);
            let edge_count = some_or!(it.next(), {
                errln!("warning: parse_dyld_export: ran into end before edge count");
                continue;
            });
            for _ in 0..edge_count {
                let this_prefix = some_or!(util::from_cstr_strict(it.0), {
                    errln!("warning: parse_dyld_export: invalid prefix");
                    continue;
                });
                *it.0 = &it.0[this_prefix.len()+1..];
                let offset = leb!(it);
                if offset > dyld_export.len() as u64 {
                    errln!("warning: parse_dyld_export: invalid limb offset {}", offset);
                } else if !seen.insert(offset) {
                    errln!("warning: parse_dyld_export: offset {} already seen, whoa, might loop", offset);
                } else {
                    todo.push((offset as usize, prefix.clone() + this_prefix));
                }
            }
        }
    }

    pub fn reaggregate_nlist_syms_from_cache<'a>(&'a self) -> Result<ReaggregatedSyms, &'static str> {
        // Three sources: the localSymbol section of the cache (dsc_tabs), our own symtab, and the export table
        let end = self.eb.endian;
        let is64 = self.is64;
        let arch = self.eb.arch;
        let mut res = ReaggregatedSyms {
            localsym: Vec::new(),
            extdefsym: Vec::new(),
            undefsym: Vec::new(),
            strtab: vec![b'\0'],
        };
        let mut str_to_strtab_pos: HashMap<ByteString, u32, _> = util::new_fnv_hashmap();
        // Why have this map?
        // 1. just in case a <redacted> is the only symbol we have for something, which shouldn't
        //    ever happen, but...
        // 2. to account for exports that are still in the symbol table.  dsc_extractor assumes it
        //    only needs to care about reexports; I think absolute symbols should also be in that
        //    list, but it's more robust to manually check for overlap.
        let mut seen_symbols_by_addr: HashMap<u64, Vec<ByteString>, _> = util::new_fnv_hashmap();
        {
        let strtab = &mut res.strtab;
        let add_string = |s: &ByteStr| -> u32 {
            *str_to_strtab_pos.entry(s.to_owned()).or_insert_with(|| {
                let pos = strtab.len();
                if pos >= (std::u32::MAX as usize) - s.len() {
                    errln!("add_string: strtab way too big");
                    return 0;
                }
                strtab.extend_from_slice(&*s);
                strtab.push(b'\0');
                pos as u32
            })
        };
        {
            // this whole thing is similar to get_symbol_list, but i want to copy directly
            let strx_to_name = |strtab: &'a [u8], strx: u64| -> &ByteStr {
                // todo: fix push_nlist_symbols to use this kind of logic
                if strx == 0 {
                    ByteStr::from_str("")
                } else if strx >= strtab.len() as u64 {
                    errln!("reaggregate_nlist_syms_from_cache: strx out of range");
                    ByteStr::from_str("<?>")
                } else {
                    util::from_cstr(&strtab[strx as usize..])
                }
            };

            let do_nlist = |symtab: &[u8], strtab: &[u8], start, count| {
                let input = &symtab[start..start+(count*self.nlist_size)];
                for nlb in input.chunks(self.nlist_size) {
                    let nl: x_nlist_64 = copy_nlist_from_slice(nlb, end);
                    let mut addr = nl.n_value;
                    if nl.n_desc as u32 & N_ARM_THUMB_DEF != 0 { addr |= 1; }
                    let name = strx_to_name(strtab, nl.n_strx as u64);
                    if let Some(names) = seen_symbols_by_addr.get_mut(&addr) {
                        // so there are existing symbols here...
                        if name == ByteStr::from_str("<redacted>") ||
                           names.iter().any(|n| &**n == name) {
                            continue;
                        }
                    }
                    nl.n_strx = add_string(name);
                    let n_type = nl.n_type as u32 & N_TYPE;
                    if n_type == N_INDR {
                        let imp_name = strx_to_name(strtab, nl.n_value);
                        nl.n_value = add_string(imp_name) as u64;
                    }
                    let which = if n_type == N_UNDF {
                        &mut res.undefsym
                    } else if nl.n_type as u32 & N_EXT != 0 {
                        &mut res.extdefsym
                    } else {
                        &mut res.localsym
                    };
                    copy_nlist_to_vec(which, &nl, end, is64);
                }
            };
            do_nlist(self.symtab.get(), self.strtab.get(), 0, self.symtab.len() / self.nlist_size);
            // must come second due to redacted check
            if let Some(DscTabs { ref symtab, ref strtab, start, count }) = self.dsc_tabs {
                do_nlist(symtab.get(), strtab.get(), start as usize, count as usize);
            }
        }
        // for the conversion I may as well just use it
        for sym in self.get_symbol_list(SymbolSource::Exported, None) {
            let name = &*sym.name;
            // this is not quite right due to different types
            if let Some(addr) = match sym.val {
                SymbolValue::Addr(vma) => Some(vma.0),
                SymbolValue::Abs(vma) => Some(vma.0),
                SymbolValue::ThreadLocal(vma) => Some(vma.0),
                SymbolValue::Resolver(vma, _) => Some(vma.0),
                _ => None
            } {
                if let Some(names) = seen_symbols_by_addr.get_mut(&addr) {
                    if names.iter().any(|n| &**n == name) {
                        continue;
                    }
                }
            }
            let nl = try!(exec_sym_to_nlist_64(
                &sym,
                add_string(name),
                if let SymbolValue::ReExport(ref imp_name) = sym.val {
                    Some(add_string(imp_name))
                } else { None },
                arch,
                &mut || { // is_text
                    // cheat because absolute symbols are probably not text :$
                    false
                }
            ));
            copy_nlist_to_vec(if let SymbolValue::Undefined(_) = sym.val {
                &mut res.undefsym
            } else if sym.is_public {
                &mut res.extdefsym
            } else {
                &mut res.localsym
            }, &nl, end, is64);
        }
        } // release str_to_strtab_pos
        Ok(res)
    }

    pub fn unbind(&mut self) {
        // helps IDA

    }
    pub fn fix_objc_from_cache(&mut self, dc: &DyldCache) {

    }
    pub fn extract_as_necessary(&mut self) -> exec::ExecResult<()> {
        if self.hdr_offset != 0 {
            // we're in a cache...
            let res = try!(self.reaggregate_nlist_syms_from_cache()
                           .map_err(|e| err(ErrorKind::other, e)));
            self.localsym = MCRef::with_data(&res.localsym);
            self.extdefsym = MCRef::with_data(&res.extdefsym);
            self.undefsym = MCRef::with_data(&res.undefsym);
            self.strtab = MCRef::with_data(&res.strtab);
            self.xsym_to_symtab();
            let dc = DyldCache::new(self.eb.whole_buf.clone(), false).unwrap();
            self.fix_objc_from_cache(&dc);
            self.unbind();
        }
        try!(self.reallocate());
        self.rewhole();
        Ok(())
    }
}

pub struct MachOProber;

impl exec::ExecProber for MachOProber {
    fn name(&self) -> &str {
        "macho"
    }
    fn probe(&self, _eps: &Vec<&'static exec::ExecProber>, buf: MCRef) -> Vec<exec::ProbeResult> {
        if let Ok(m) = MachO::new(buf, false, 0) {
            vec!(exec::ProbeResult {
                desc: m.desc(),
                arch: m.eb.arch,
                likely: true,
                cmd: vec!["macho".to_string()],
            })
        } else {
            vec!()
        }
    }
   fn create(&self, _eps: &Vec<&'static exec::ExecProber>, buf: MCRef, args: Vec<String>) -> exec::ExecResult<(Box<exec::Exec>, Vec<String>)> {
        let m = try!(exec::usage_to_invalid_args(util::do_getopts_or_usage(&*args, "macho ...", 0, std::usize::MAX, &mut vec!(
            // ...
        ))));
        let mo: MachO = try!(MachO::new(buf, true, 0));
        Ok((Box::new(mo) as Box<exec::Exec>, m.free))
    }
}

pub struct FatMachOProber;

impl FatMachOProber {
    fn probe_cb(&self, mc: &MCRef, cb: &mut FnMut(u64, fat_arch)) -> bool {
        let buf = mc.get();
        if buf.len() < 8 { return false }
        let fh: fat_header = util::copy_from_slice(&buf[..8], util::BigEndian);
        if fh.magic != FAT_MAGIC as u32 { return false }
        let nfat = fh.nfat_arch as u64;
        let mut off: usize = 8;
        if (buf.len() as u64) < (off as u64) + (nfat * size_of::<fat_arch>() as u64) {
            errln!("fatmacho: no room for {} fat archs", nfat);
            return false;
        }
        for i in 0..nfat {
            let fa: fat_arch = util::copy_from_slice(&buf[off..off + size_of::<fat_arch>()], util::BigEndian);
            if (fa.offset as u64) + (fa.size as u64) > (buf.len() as u64) {
                errln!("fatmacho: bad arch cputype={},{} offset={} size={} (truncated?)",
                       fa.cputype, fa.cpusubtype, fa.offset, fa.size);
            } else {
                cb(i, fa);
            }
            off += size_of::<fat_arch>();
        }
        true
    }
}

impl exec::ExecProber for FatMachOProber {
    fn name(&self) -> &str {
        "fat"
    }
    fn probe(&self, eps: &Vec<exec::ExecProberRef>, mc: MCRef) -> Vec<exec::ProbeResult> {
        let mut result = Vec::new();
        let ok = self.probe_cb(&mc, &mut |i, fa| {
            let arch = match mach_arch_desc(fa.cputype, fa.cpusubtype) {
                Some(desc) => desc.to_string(),
                None => format!("{}", i),
            };
            let off = fa.offset as usize;
            let size = fa.size as usize;
            for pr in exec::probe_all(eps, mc.slice(off, off + size).unwrap()).into_iter() {
                let npr = exec::ProbeResult {
                    desc: format!("(slice #{}) {}", i, pr.desc),
                    arch: pr.arch,
                    likely: pr.likely,
                    cmd: { let mut s = vec!("fat", "--arch", &*arch).strings(); s.extend_from_slice(&*pr.cmd); s },
                };
                result.push(npr);
            }
        });
        if !ok { return vec!()}
        result
    }

    fn create(&self, eps: &Vec<exec::ExecProberRef>, mc: MCRef, args: Vec<String>) -> exec::ExecResult<(Box<exec::Exec>, Vec<String>)> {
        let top = "fat (--arch ARCH | -s SLICE)";
        let mut optgrps = vec!(
            getopts::optopt("", "arch", "choose by arch (OS X standard names)", "arch"),
            getopts::optopt("s", "slice", "choose by slice number", ""),
        );
        let mut m = try!(exec::usage_to_invalid_args(util::do_getopts_or_usage(&*args, top, 0, std::usize::MAX, &mut optgrps)));
        let slice_num = m.opt_str("slice");
        let arch = m.opt_str("arch");
        if slice_num.is_some() == arch.is_some() {
            return exec::usage_to_invalid_args(Err(util::usage(top, &mut optgrps)));
        }
        let slice_i = slice_num.map_or(0u64, |s| FromStr::from_str(&*s).unwrap());
        let mut result = None;
        let ok = self.probe_cb(&mc, &mut |i, fa| {
            if if let (&None, &Some(ref arch_)) = (&result, &arch) {
                mach_arch_desc(fa.cputype, fa.cpusubtype).map_or(false, |d| d == &**arch_)
            } else {
                i == slice_i
            }
            {
                let off = fa.offset as usize;
                let size = fa.size as usize;
                result = Some(exec::create(eps, mc.slice(off, off + size).unwrap(), replace(&mut m.free, vec!())));
            }
        });
        if !ok {
            return err(ErrorKind::BadData, "invalid fat mach-o");
        }
        match result {
            Some(e) => e,
            None => err(ErrorKind::Other, "no fat arch matched the arguments specified")
        }
    }
}


//#[test]

