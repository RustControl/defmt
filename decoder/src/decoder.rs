//! Decodes [`defmt`](https://github.com/knurling-rs/defmt) log frames
//!
//! This is an implementation detail of [`probe-run`](https://github.com/knurling-rs/probe-run) and
//! not meant to be consumed by other tools at the moment so all the API is unstable.

// NOTE: always runs on the host
#![cfg(feature = "unstable")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, doc(cfg(unstable)))]

use core::convert::{TryFrom, TryInto};
use core::fmt::{self, Write as _};
use core::ops::Range;
use std::collections::BTreeMap;
use std::{
    error::Error,
    io, mem,
    sync::{
        atomic::{self, AtomicBool},
        Arc,
    },
};

use byteorder::{ReadBytesExt, LE};
use colored::Colorize;

use crate::DEFMT_VERSION;
pub use defmt_parser::Level;
use defmt_parser::{get_max_bitfield_range, DisplayHint, Fragment, Parameter, ParserMode, Type};

#[derive(PartialEq, Eq, Debug)]
pub enum Tag {
    /// Defmt-controlled format string for primitive types.
    Prim,
    /// Format string created by `#[derive(Format)]`.
    Derived,
    /// A user-defined format string from a `write!` invocation.
    Write,
    /// An interned string, for use with `{=istr}`.
    Str,
    /// Defines the global timestamp format.
    Timestamp,

    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Tag {
    fn to_level(&self) -> Option<Level> {
        match self {
            Tag::Trace => Some(Level::Trace),
            Tag::Debug => Some(Level::Debug),
            Tag::Info => Some(Level::Info),
            Tag::Warn => Some(Level::Warn),
            Tag::Error => Some(Level::Error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct TableEntry {
    string: StringEntry,
    raw_symbol: String,
}

impl TableEntry {
    pub fn new(string: StringEntry, raw_symbol: String) -> Self {
        Self { string, raw_symbol }
    }

    #[cfg(test)]
    fn new_without_symbol(tag: Tag, string: String) -> Self {
        Self {
            string: StringEntry::new(tag, string),
            raw_symbol: "<unknown>".to_string(),
        }
    }
}

#[derive(Debug)]
pub struct StringEntry {
    tag: Tag,
    string: String,
}

impl StringEntry {
    pub fn new(tag: Tag, string: String) -> Self {
        Self { tag, string }
    }
}

/// Interner table that holds log levels and maps format strings to indices
#[derive(Debug)]
pub struct Table {
    timestamp: Option<TableEntry>,
    entries: BTreeMap<usize, TableEntry>,
}

/// Checks if the version encoded in the symbol table is compatible with this version of the
/// `decoder` crate
pub fn check_version(version: &str) -> Result<(), String> {
    enum Kind {
        // "1" or "0.1"
        Semver,
        // commit hash "e739d0ac703dfa629a159be329e8c62a1c3ed206"
        Git,
    }

    impl Kind {
        fn of(version: &str) -> Kind {
            if version.contains('.') || version.parse::<u64>().is_ok() {
                // "1" or "0.1"
                Kind::Semver
            } else {
                // "e739d0ac703dfa629a159be329e8c62a1c3ed206" (should be)
                Kind::Git
            }
        }
    }

    if version != DEFMT_VERSION {
        let mut msg = format!(
            "defmt version mismatch: firmware is using {}, `probe-run` supports {}\nsuggestion: ",
            version, DEFMT_VERSION
        );

        match (Kind::of(version), Kind::of(DEFMT_VERSION)) {
            (Kind::Git, Kind::Git) => {
                write!(
                    msg,
                    "pin _all_ `defmt` related dependencies to revision {0}; modify Cargo.toml files as shown below

[dependencies]
defmt = {{ git = \"https://github.com/knurling-rs/defmt\", rev = \"{0}\" }}
defmt-rtt = {{ git = \"https://github.com/knurling-rs/defmt\", rev = \"{0}\" }}
# ONLY pin this dependency if you are using the `print-defmt` feature
panic-probe = {{ git = \"https://github.com/knurling-rs/defmt\", features = [\"print-defmt\"], rev = \"{0}\" }}",
                    DEFMT_VERSION
                )
                .ok();
            }
            (Kind::Git, Kind::Semver) => {
                msg.push_str("migrate your firmware to a crates.io version of defmt (check https://https://defmt.ferrous-systems.com) OR
`cargo install` a _git_ version of `probe-run`: `cargo install --git https://github.com/knurling-rs/probe-run --branch main`");
            }
            (Kind::Semver, Kind::Git) => {
                msg.push_str(
                    "`cargo install` a non-git version of `probe-run`: `cargo install probe-run`",
                );
            }
            (Kind::Semver, Kind::Semver) => {
                write!(
                    msg,
                    "`cargo install` a different non-git version of `probe-run` that supports defmt {}",
                    version,
                )
                .ok();
            }
        }

        return Err(msg);
    }

    Ok(())
}

impl Table {
    /// NOTE caller must verify that defmt symbols are compatible with this version of the `decoder`
    /// crate using the `check_version` function
    pub fn new(entries: BTreeMap<usize, TableEntry>) -> Self {
        Self {
            entries,
            timestamp: None,
        }
    }

    pub fn set_timestamp_entry(&mut self, timestamp: TableEntry) {
        self.timestamp = Some(timestamp);
    }

    fn _get(&self, index: usize) -> Result<(Option<Level>, &str), ()> {
        let entry = self.entries.get(&index).ok_or(())?;
        Ok((entry.string.tag.to_level(), &entry.string.string))
    }

    fn get_with_level(&self, index: usize) -> Result<(Level, &str), ()> {
        let (lvl, format) = self._get(index)?;
        Ok((lvl.ok_or(())?, format))
    }

    fn get_without_level(&self, index: usize) -> Result<&str, ()> {
        let (lvl, format) = self._get(index)?;
        if lvl.is_none() {
            Ok(format)
        } else {
            Err(())
        }
    }

    pub fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.entries.iter().filter_map(move |(idx, entry)| {
            if entry.string.tag.to_level().is_some() {
                Some(*idx)
            } else {
                None
            }
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates over the raw symbols of the table entries
    pub fn raw_symbols(&self) -> impl Iterator<Item = &str> + '_ {
        self.entries.values().map(|s| &*s.raw_symbol)
    }
}

/// A log frame
#[derive(Debug, PartialEq)]
pub struct Frame<'t> {
    level: Level,
    index: u64,
    timestamp_format: Option<&'t str>,
    timestamp_args: Vec<Arg<'t>>,
    // Format string
    format: &'t str,
    args: Vec<Arg<'t>>,
}

impl<'t> Frame<'t> {
    /// Returns a struct that will format this log frame (including message, timestamp, level,
    /// etc.).
    pub fn display(&'t self, colored: bool) -> DisplayFrame<'t> {
        DisplayFrame {
            frame: self,
            colored,
        }
    }

    pub fn display_timestamp(&'t self) -> Option<DisplayMessage<'t>> {
        self.timestamp_format.map(|fmt| DisplayMessage {
            format: fmt,
            args: &self.timestamp_args,
        })
    }

    /// Returns a struct that will format the message contained in this log frame.
    pub fn display_message(&'t self) -> DisplayMessage<'t> {
        DisplayMessage {
            format: self.format,
            args: &self.args,
        }
    }

    pub fn level(&self) -> Level {
        self.level
    }

    pub fn index(&self) -> u64 {
        self.index
    }
}

pub struct DisplayMessage<'t> {
    format: &'t str,
    args: &'t [Arg<'t>],
}

impl fmt::Display for DisplayMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let args = format_args(self.format, self.args, None);
        f.write_str(&args)
    }
}

/// Prints a `Frame` when formatted via `fmt::Display`, including all included metadata (level,
/// timestamp, ...).
pub struct DisplayFrame<'t> {
    frame: &'t Frame<'t>,
    colored: bool,
}

impl fmt::Display for DisplayFrame<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = if self.colored {
            match self.frame.level {
                Level::Trace => "TRACE".dimmed().to_string(),
                Level::Debug => "DEBUG".normal().to_string(),
                Level::Info => "INFO".green().to_string(),
                Level::Warn => "WARN".yellow().to_string(),
                Level::Error => "ERROR".red().to_string(),
            }
        } else {
            match self.frame.level {
                Level::Trace => "TRACE".to_string(),
                Level::Debug => "DEBUG".to_string(),
                Level::Info => "INFO".to_string(),
                Level::Warn => "WARN".to_string(),
                Level::Error => "ERROR".to_string(),
            }
        };

        let timestamp = self
            .frame
            .timestamp_format
            .map(|fmt| format!("{} ", format_args(&fmt, &self.frame.timestamp_args, None,)))
            .unwrap_or_default();
        let args = format_args(&self.frame.format, &self.frame.args, None);

        write!(f, "{}{} {}", timestamp, level, args)
    }
}

#[derive(Debug)]
struct Bool(AtomicBool);

impl Bool {
    #[allow(clippy::declare_interior_mutable_const)]
    const FALSE: Self = Self(AtomicBool::new(false));

    fn set(&self, value: bool) {
        self.0.store(value, atomic::Ordering::Relaxed);
    }
}

impl fmt::Display for Bool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0.load(atomic::Ordering::Relaxed))
    }
}

impl PartialEq for Bool {
    fn eq(&self, other: &Self) -> bool {
        self.0
            .load(atomic::Ordering::Relaxed)
            .eq(&other.0.load(atomic::Ordering::Relaxed))
    }
}

// NOTE follows `parser::Type`
#[derive(Debug, PartialEq)]
enum Arg<'t> {
    /// Bool
    Bool(Arc<Bool>),
    F32(f32),
    F64(f64),
    /// U8, U16, U24 and U32
    Uxx(u128),
    /// I8, I16, I24 and I32
    Ixx(i128),
    /// Str
    Str(String),
    /// Interned string
    IStr(&'t str),
    /// Format
    Format {
        format: &'t str,
        args: Vec<Arg<'t>>,
    },
    FormatSlice {
        elements: Vec<FormatSliceElement<'t>>,
    },
    /// Slice or Array of bytes.
    Slice(Vec<u8>),
    /// Char
    Char(char),

    /// `fmt::Debug` / `fmt::Display` formatted on-target.
    Preformatted(String),
}

#[derive(Debug, PartialEq)]
struct FormatSliceElement<'t> {
    // this will usually be the same format string for all elements; except when the format string
    // is an enum -- in that case `format` will be the variant
    format: &'t str,
    args: Vec<Arg<'t>>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// More data is needed to decode the next frame.
    UnexpectedEof,

    Malformed,
}

impl From<io::Error> for DecodeError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            DecodeError::UnexpectedEof
        } else {
            DecodeError::Malformed
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEof => f.write_str("unexpected end of stream"),
            DecodeError::Malformed => f.write_str("malformed data"),
        }
    }
}

impl Error for DecodeError {}

fn read_leb128(bytes: &mut &[u8]) -> Result<u64, DecodeError> {
    match leb128::read::unsigned(bytes) {
        Ok(val) => Ok(val),
        Err(leb128::read::Error::Overflow) => Err(DecodeError::Malformed),
        Err(leb128::read::Error::IoError(io)) => Err(io.into()),
    }
}

/// decode the data sent by the device using the previosuly stored metadata
///
/// * bytes: contains the data sent by the device that logs.
///          contains the [log string index, timestamp, optional fmt string args]
/// * table: contains the mapping of log string indices to their format strings, as well as the log level.
pub fn decode<'t>(
    mut bytes: &[u8],
    table: &'t Table,
) -> Result<(Frame<'t>, /*consumed: */ usize), DecodeError> {
    let len = bytes.len();
    let index = read_leb128(&mut bytes)?;

    let mut decoder = Decoder {
        table,
        bytes,
        format_list: None,
        bools_tbd: Vec::new(),
        below_enum: false,
    };

    let mut timestamp_format = None;
    let mut timestamp_args = Vec::new();
    if let Some(entry) = table.timestamp.as_ref() {
        let format = &entry.string.string;
        timestamp_format = Some(&**format);
        timestamp_args = decoder.decode_format(format)?;
    }

    let (level, format) = table
        .get_with_level(index as usize)
        .map_err(|_| DecodeError::Malformed)?;

    let args = decoder.decode_format(format)?;
    if !decoder.bools_tbd.is_empty() {
        // Flush end of compression block.
        decoder.read_and_unpack_bools()?;
    }

    let frame = Frame {
        level,
        index,
        timestamp_format,
        timestamp_args,
        format,
        args,
    };

    let consumed = len - decoder.bytes.len();
    Ok((frame, consumed))
}

/// Note that this will not change the Bitfield params in place, i.e. if `params` was sorted before
/// a call to this function, it won't be afterwards.
fn merge_bitfields(params: &mut Vec<Parameter>) {
    if params.is_empty() {
        return;
    }

    let mut merged_bitfields = Vec::new();

    let max_index: usize = *params.iter().map(|param| &param.index).max().unwrap();

    for index in 0..=max_index {
        let mut bitfields_with_index = params
            .iter()
            .filter(
                |param| matches!((param.index, &param.ty), (i, Type::BitField(_)) if i == index),
            )
            .peekable();

        if bitfields_with_index.peek().is_some() {
            let (smallest, largest) = get_max_bitfield_range(bitfields_with_index).unwrap();

            // create new merged bitfield for this index
            merged_bitfields.push(Parameter {
                index,
                ty: Type::BitField(Range {
                    start: smallest,
                    end: largest,
                }),
                hint: None, // don't care
            });

            // remove old bitfields with this index
            // TODO refactor when `drain_filter()` is stable
            let mut i = 0;
            while i != params.len() {
                match &params[i].ty {
                    Type::BitField(_) => {
                        if params[i].index == index {
                            params.remove(i);
                        } else {
                            i += 1; // we haven't removed a bitfield -> move i forward
                        }
                    }
                    _ => {
                        i += 1; // we haven't removed a bitfield -> move i forward
                    }
                }
            }
        }
    }

    // add merged bitfields to unsorted params
    params.append(&mut merged_bitfields);
}

struct Decoder<'t, 'b> {
    table: &'t Table,
    bytes: &'b [u8],
    format_list: Option<FormatList<'t>>,
    // below an enum tags must be included
    below_enum: bool,
    bools_tbd: Vec<Arc<Bool>>,
}

const MAX_NUM_BOOL_FLAGS: usize = 8;

impl<'t, 'b> Decoder<'t, 'b> {
    /// Reads a byte of packed bools and unpacks them into `args` at the given indices.
    fn read_and_unpack_bools(&mut self) -> Result<(), DecodeError> {
        let bool_flags = self.bytes.read_u8()?;
        let mut flag_index = self.bools_tbd.len();

        for bool in self.bools_tbd.iter() {
            flag_index -= 1;

            // read out the leftmost unread bit and turn it into a boolean
            let flag_mask = 1 << flag_index;
            let nth_flag = (bool_flags & flag_mask) != 0;

            bool.set(nth_flag);
        }

        self.bools_tbd.clear();

        Ok(())
    }

    /// Sort and deduplicate `params` so that they can be interpreted correctly during decoding
    fn prepare_params(&self, params: &mut Vec<Parameter>) {
        // deduplicate bitfields by merging them by index
        merge_bitfields(params);

        // sort & dedup to ensure that format string args can be addressed by index too
        params.sort_by(|a, b| a.index.cmp(&b.index));
        params.dedup_by(|a, b| a.index == b.index);
    }

    /// Gets a format string from
    /// - the `FormatList`, if it's in `Use` mode, or
    /// - from `bytes` and `table` if the `FormatList` is in `Build` mode or was not provided
    fn get_format(&mut self) -> Result<&'t str, DecodeError> {
        if let Some(FormatList::Use { formats, cursor }) = self.format_list.as_mut() {
            if let Some(format) = formats.get(*cursor) {
                *cursor += 1;
                return Ok(format);
            }
        }

        let index = read_leb128(&mut self.bytes)?;
        let format = self
            .table
            .get_without_level(index as usize)
            .map_err(|_| DecodeError::Malformed)?;

        if let Some(FormatList::Build { formats }) = self.format_list.as_mut() {
            if !self.below_enum {
                formats.push(format)
            }
        }
        Ok(format)
    }

    fn get_variant(&mut self, format: &'t str) -> Result<&'t str, DecodeError> {
        assert!(format.contains('|'));
        // NOTE nesting of enums, like "A|B(C|D)" is not possible; indirection is
        // required: "A|B({:?})" where "{:?}" -> "C|D"
        let num_variants = format.chars().filter(|c| *c == '|').count();

        let discriminant: usize = if u8::try_from(num_variants).is_ok() {
            self.bytes.read_u8()?.into()
        } else if u16::try_from(num_variants).is_ok() {
            self.bytes.read_u16::<LE>()?.into()
        } else if u32::try_from(num_variants).is_ok() {
            self.bytes
                .read_u32::<LE>()?
                .try_into()
                .map_err(|_| DecodeError::Malformed)?
        } else if u64::try_from(num_variants).is_ok() {
            self.bytes
                .read_u64::<LE>()?
                .try_into()
                .map_err(|_| DecodeError::Malformed)?
        } else {
            return Err(DecodeError::Malformed);
        };

        format
            .split('|')
            .nth(discriminant)
            .ok_or(DecodeError::Malformed)
    }

    fn decode_format_slice(
        &mut self,
        num_elements: usize,
    ) -> Result<Vec<FormatSliceElement<'t>>, DecodeError> {
        if num_elements == 0 {
            return Ok(vec![]);
        }

        let format = self.get_format()?;

        // let variant_format = if
        let is_enum = format.contains('|');
        let below_enum = self.below_enum;

        if is_enum {
            self.below_enum = true;
        }

        let mut elements = Vec::with_capacity(num_elements);
        let mut formats = vec![];
        let mut cursor = 0;
        for i in 0..num_elements {
            let is_first = i == 0;

            let format = if is_enum {
                self.get_variant(format)?
            } else {
                format
            };

            let args = if let Some(list) = &mut self.format_list {
                match list {
                    FormatList::Use { .. } => self.decode_format(format)?,

                    FormatList::Build { formats } => {
                        if is_first {
                            cursor = formats.len();
                            self.decode_format(format)?
                        } else {
                            let formats = formats.clone();
                            let old = mem::replace(
                                &mut self.format_list,
                                Some(FormatList::Use { formats, cursor }),
                            );
                            let args = self.decode_format(format)?;
                            self.format_list = old;
                            args
                        }
                    }
                }
            } else if is_first {
                let mut old =
                    mem::replace(&mut self.format_list, Some(FormatList::Build { formats }));
                let args = self.decode_format(format)?;
                mem::swap(&mut self.format_list, &mut old);
                formats = match old {
                    Some(FormatList::Build { formats, .. }) => formats,
                    _ => unreachable!(),
                };
                args
            } else {
                let formats = formats.clone();
                let old = mem::replace(
                    &mut self.format_list,
                    Some(FormatList::Use { formats, cursor: 0 }),
                );
                let args = self.decode_format(format)?;
                self.format_list = old;
                args
            };

            elements.push(FormatSliceElement { format, args });
        }

        if is_enum {
            self.below_enum = below_enum;
        }

        Ok(elements)
    }

    /// Decodes arguments from the stream, according to `format`.
    fn decode_format(&mut self, format: &str) -> Result<Vec<Arg<'t>>, DecodeError> {
        let mut args = vec![]; // will contain the deserialized arguments on return
        let mut params = defmt_parser::parse(format, defmt_parser::ParserMode::ForwardsCompatible)
            .map_err(|_| DecodeError::Malformed)?
            .iter()
            .filter_map(|frag| match frag {
                Fragment::Parameter(param) => Some(param.clone()),
                Fragment::Literal(_) => None,
            })
            .collect::<Vec<_>>();

        self.prepare_params(&mut params);

        for param in &params {
            match &param.ty {
                Type::U8 => {
                    let data = self.bytes.read_u8()?;
                    args.push(Arg::Uxx(data as u128));
                }

                Type::Bool => {
                    let arc = Arc::new(Bool::FALSE);
                    args.push(Arg::Bool(arc.clone()));
                    self.bools_tbd.push(arc.clone());
                    if self.bools_tbd.len() == MAX_NUM_BOOL_FLAGS {
                        // reached end of compression block: sprinkle values into args
                        self.read_and_unpack_bools()?;
                    }
                }

                Type::FormatSlice => {
                    let num_elements = read_leb128(&mut self.bytes)? as usize;
                    let elements = self.decode_format_slice(num_elements)?;
                    args.push(Arg::FormatSlice { elements });
                }
                Type::Format => {
                    let format = self.get_format()?;

                    if format.contains('|') {
                        // enum
                        let variant = self.get_variant(format)?;
                        let below_enum = self.below_enum;
                        self.below_enum = true;
                        let inner_args = self.decode_format(variant)?;
                        self.below_enum = below_enum;
                        args.push(Arg::Format {
                            format: variant,
                            args: inner_args,
                        });
                    } else {
                        let inner_args = self.decode_format(format)?;
                        args.push(Arg::Format {
                            format,
                            args: inner_args,
                        });
                    }
                }
                Type::I16 => {
                    let data = self.bytes.read_i16::<LE>()?;
                    args.push(Arg::Ixx(data as i128));
                }
                Type::I32 => {
                    let data = self.bytes.read_i32::<LE>()?;
                    args.push(Arg::Ixx(data as i128));
                }
                Type::I64 => {
                    let data = self.bytes.read_i64::<LE>()?;
                    args.push(Arg::Ixx(data as i128));
                }
                Type::I128 => {
                    let data = self.bytes.read_i128::<LE>()?;
                    args.push(Arg::Ixx(data));
                }
                Type::I8 => {
                    let data = self.bytes.read_i8()?;
                    args.push(Arg::Ixx(data as i128));
                }
                Type::Isize => {
                    // Signed isize is encoded in zigzag-encoding.
                    let unsigned = read_leb128(&mut self.bytes)?;
                    args.push(Arg::Ixx(zigzag_decode(unsigned) as i128))
                }
                Type::U16 => {
                    let data = self.bytes.read_u16::<LE>()?;
                    args.push(Arg::Uxx(data as u128));
                }
                Type::U24 => {
                    let data_low = self.bytes.read_u8()?;
                    let data_high = self.bytes.read_u16::<LE>()?;
                    let data = data_low as u128 | (data_high as u128) << 8;
                    args.push(Arg::Uxx(data as u128));
                }
                Type::U32 => {
                    let data = self.bytes.read_u32::<LE>()?;
                    args.push(Arg::Uxx(data as u128));
                }
                Type::U64 => {
                    let data = self.bytes.read_u64::<LE>()?;
                    args.push(Arg::Uxx(data as u128));
                }
                Type::U128 => {
                    let data = self.bytes.read_u128::<LE>()?;
                    args.push(Arg::Uxx(data as u128));
                }
                Type::Usize => {
                    let unsigned = read_leb128(&mut self.bytes)?;
                    args.push(Arg::Uxx(unsigned as u128))
                }
                Type::F32 => {
                    let data = self.bytes.read_u32::<LE>()?;
                    args.push(Arg::F32(f32::from_bits(data)));
                }
                Type::F64 => {
                    let data = self.bytes.read_u64::<LE>()?;
                    args.push(Arg::F64(f64::from_bits(data)));
                }
                Type::BitField(range) => {
                    let mut data: u128;
                    let lowest_byte = range.start / 8;
                    // -1 because `range` is range-exclusive
                    let highest_byte = (range.end - 1) / 8;
                    let size_after_truncation = highest_byte - lowest_byte + 1; // in octets

                    match size_after_truncation {
                        1 => {
                            data = self.bytes.read_u8()? as u128;
                        }
                        2 => {
                            data = self.bytes.read_u16::<LE>()? as u128;
                        }
                        3 => {
                            data = self.bytes.read_u24::<LE>()? as u128;
                        }
                        4 => {
                            data = self.bytes.read_u32::<LE>()? as u128;
                        }
                        5..=8 => {
                            data = self.bytes.read_u64::<LE>()? as u128;
                        }
                        9..=16 => {
                            data = self.bytes.read_u128::<LE>()? as u128;
                        }
                        _ => {
                            unreachable!();
                        }
                    }

                    data <<= lowest_byte * 8;

                    args.push(Arg::Uxx(data));
                }
                Type::Str => {
                    let str_len = read_leb128(&mut self.bytes)? as usize;
                    let mut arg_str_bytes = vec![];

                    // note: went for the suboptimal but simple solution; optimize if necessary
                    for _ in 0..str_len {
                        arg_str_bytes.push(self.bytes.read_u8()?);
                    }

                    // convert to utf8 (no copy)
                    let arg_str =
                        String::from_utf8(arg_str_bytes).map_err(|_| DecodeError::Malformed)?;

                    args.push(Arg::Str(arg_str));
                }
                Type::IStr => {
                    let str_index = read_leb128(&mut self.bytes)? as usize;

                    let string = self
                        .table
                        .get_without_level(str_index as usize)
                        .map_err(|_| DecodeError::Malformed)?;

                    args.push(Arg::IStr(string));
                }
                Type::U8Slice => {
                    // only supports byte slices
                    let num_elements = read_leb128(&mut self.bytes)? as usize;
                    let mut arg_slice = vec![];

                    // note: went for the suboptimal but simple solution; optimize if necessary
                    for _ in 0..num_elements {
                        arg_slice.push(self.bytes.read_u8()?);
                    }
                    args.push(Arg::Slice(arg_slice.to_vec()));
                }
                Type::U8Array(len) => {
                    let mut arg_slice = vec![];
                    // note: went for the suboptimal but simple solution; optimize if necessary
                    for _ in 0..*len {
                        arg_slice.push(self.bytes.read_u8()?);
                    }
                    args.push(Arg::Slice(arg_slice.to_vec()));
                }
                Type::FormatArray(len) => {
                    let elements = self.decode_format_slice(*len)?;
                    args.push(Arg::FormatSlice { elements });
                }
                Type::Char => {
                    let data = self.bytes.read_u32::<LE>()?;
                    let c = std::char::from_u32(data).ok_or(DecodeError::Malformed)?;
                    args.push(Arg::Char(c));
                }
                Type::Debug | Type::Display => {
                    // UTF-8 stream without a prefix length, terminated with `0xFF`.

                    let end = self
                        .bytes
                        .iter()
                        .position(|b| *b == 0xff)
                        .ok_or(DecodeError::UnexpectedEof)?;
                    let data = core::str::from_utf8(&self.bytes[..end])
                        .map_err(|_| DecodeError::Malformed)?;
                    self.bytes = &self.bytes[end + 1..];

                    args.push(Arg::Preformatted(data.into()));
                }
            }
        }

        Ok(args)
    }
}

/// List of format strings; used when decoding a `FormatSlice` (`{:[?]}`) argument
#[derive(Debug)]
enum FormatList<'t> {
    /// Build the list; used when decoding the first element
    Build { formats: Vec<&'t str> },
    /// Use the list; used when decoding the rest of elements
    Use {
        formats: Vec<&'t str>,
        cursor: usize,
    },
}

fn format_args(format: &str, args: &[Arg], parent_hint: Option<&DisplayHint>) -> String {
    format_args_real(format, args, parent_hint).unwrap() // cannot fail, we only write to a `String`
}

fn format_args_real(
    format: &str,
    args: &[Arg],
    parent_hint: Option<&DisplayHint>,
) -> Result<String, fmt::Error> {
    fn format_u128(
        x: u128,
        hint: Option<&DisplayHint>,
        buf: &mut String,
    ) -> Result<(), fmt::Error> {
        match hint {
            Some(DisplayHint::Binary) => write!(buf, "{:#b}", x)?,
            Some(DisplayHint::Hexadecimal {
                is_uppercase: false,
            }) => write!(buf, "{:#x}", x)?,
            Some(DisplayHint::Hexadecimal { is_uppercase: true }) => write!(buf, "{:#X}", x)?,
            Some(DisplayHint::Microseconds) => {
                let seconds = x / 1_000_000;
                let micros = x % 1_000_000;
                write!(buf, "{}.{:06}", seconds, micros)?;
            }
            _ => write!(buf, "{}", x)?,
        }
        Ok(())
    }

    fn format_i128(
        x: i128,
        hint: Option<&DisplayHint>,
        buf: &mut String,
    ) -> Result<(), fmt::Error> {
        match hint {
            Some(DisplayHint::Binary) => write!(buf, "{:#b}", x)?,
            Some(DisplayHint::Hexadecimal {
                is_uppercase: false,
            }) => write!(buf, "{:#x}", x)?,
            Some(DisplayHint::Hexadecimal { is_uppercase: true }) => write!(buf, "{:#X}", x)?,
            _ => write!(buf, "{}", x)?,
        }
        Ok(())
    }

    fn format_bytes(
        bytes: &[u8],
        hint: Option<&DisplayHint>,
        buf: &mut String,
    ) -> Result<(), fmt::Error> {
        match hint {
            Some(DisplayHint::Ascii) => {
                // byte string literal syntax: b"Hello\xffworld"
                buf.push_str("b\"");
                for byte in bytes {
                    match byte {
                        // special escaping
                        b'\t' => buf.push_str("\\t"),
                        b'\n' => buf.push_str("\\n"),
                        b'\r' => buf.push_str("\\r"),
                        b' ' => buf.push(' '),
                        b'\"' => buf.push_str("\\\""),
                        b'\\' => buf.push_str("\\\\"),
                        _ => {
                            if byte.is_ascii_graphic() {
                                buf.push(*byte as char);
                            } else {
                                // general escaped form
                                write!(buf, "\\x{:02x}", byte).ok();
                            }
                        }
                    }
                }
                buf.push('\"');
            }
            Some(DisplayHint::Hexadecimal { .. }) | Some(DisplayHint::Binary { .. }) => {
                // `core::write!` doesn't quite produce the output we want, for example
                // `write!("{:#04x?}", bytes)` produces a multi-line output
                // `write!("{:02x?}", bytes)` is single-line but each byte doesn't include the "0x" prefix
                buf.push('[');
                let mut is_first = true;
                for byte in bytes {
                    if !is_first {
                        buf.push_str(", ");
                    }
                    is_first = false;
                    format_u128(*byte as u128, hint, buf)?;
                }
                buf.push(']');
            }
            _ => write!(buf, "{:?}", bytes)?,
        }
        Ok(())
    }

    fn format_str(s: &str, hint: Option<&DisplayHint>, buf: &mut String) -> Result<(), fmt::Error> {
        if hint == Some(&DisplayHint::Debug) {
            write!(buf, "{:?}", s)?;
        } else {
            buf.push_str(s);
        }
        Ok(())
    }

    let params = defmt_parser::parse(format, ParserMode::ForwardsCompatible).unwrap();
    let mut buf = String::new();
    for param in params {
        match param {
            Fragment::Literal(lit) => {
                buf.push_str(&lit);
            }
            Fragment::Parameter(param) => {
                let hint = param.hint.as_ref().or(parent_hint);

                match &args[param.index] {
                    Arg::Bool(x) => write!(buf, "{}", x)?,
                    Arg::F32(x) => write!(buf, "{}", ryu::Buffer::new().format(*x))?,
                    Arg::F64(x) => write!(buf, "{}", ryu::Buffer::new().format(*x))?,
                    Arg::Uxx(x) => {
                        match param.ty {
                            Type::BitField(range) => {
                                let left_zeroes = mem::size_of::<u128>() * 8 - range.end as usize;
                                let right_zeroes = left_zeroes + range.start as usize;
                                // isolate the desired bitfields
                                let bitfields = (*x << left_zeroes) >> right_zeroes;

                                if let Some(DisplayHint::Ascii) = hint {
                                    let bstr = bitfields
                                        .to_be_bytes()
                                        .iter()
                                        .skip(right_zeroes / 8)
                                        .copied()
                                        .collect::<Vec<u8>>();
                                    format_bytes(&bstr, hint, &mut buf)?
                                } else {
                                    format_u128(bitfields as u128, hint, &mut buf)?;
                                }
                            }
                            _ => format_u128(*x as u128, hint, &mut buf)?,
                        }
                    }
                    Arg::Ixx(x) => format_i128(*x as i128, hint, &mut buf)?,
                    Arg::Str(x) | Arg::Preformatted(x) => format_str(x, hint, &mut buf)?,
                    Arg::IStr(x) => format_str(x, hint, &mut buf)?,
                    Arg::Format { format, args } => buf.push_str(&format_args(format, args, hint)),
                    Arg::FormatSlice { elements } => {
                        match hint {
                            // Filter Ascii Hints, which contains u8 byte slices
                            Some(DisplayHint::Ascii)
                                if elements.iter().filter(|e| e.format == "{=u8}").count() != 0 =>
                            {
                                let vals = elements
                                    .iter()
                                    .map(|e| match e.args.as_slice() {
                                        [Arg::Uxx(v)] => {
                                            u8::try_from(*v).expect("the value must be in u8 range")
                                        }
                                        _ => panic!("FormatSlice should only contain one argument"),
                                    })
                                    .collect::<Vec<u8>>();
                                format_bytes(&vals, hint, &mut buf)?
                            }
                            _ => {
                                buf.write_str("[")?;
                                let mut is_first = true;
                                for element in elements {
                                    if !is_first {
                                        buf.write_str(", ")?;
                                    }
                                    is_first = false;
                                    buf.write_str(&format_args(
                                        element.format,
                                        &element.args,
                                        hint,
                                    ))?;
                                }
                                buf.write_str("]")?;
                            }
                        }
                    }
                    Arg::Slice(x) => format_bytes(x, hint, &mut buf)?,
                    Arg::Char(c) => write!(buf, "{}", c)?,
                }
            }
        }
    }
    Ok(buf)
}

fn zigzag_decode(unsigned: u64) -> i64 {
    (unsigned >> 1) as i64 ^ -((unsigned & 1) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{Frame, Level, Table};
    use crate::decoder::{merge_bitfields, Arg};
    use std::collections::BTreeMap;

    // helper function to initiate decoding and assert that the result is as expected.
    //
    // format:       format string to be expanded
    // bytes:        arguments + metadata
    // expectation:  the expected result
    fn decode_and_expect(format: &str, bytes: &[u8], expectation: &str) {
        let mut entries = BTreeMap::new();
        entries.insert(
            bytes[0] as usize,
            TableEntry::new_without_symbol(Tag::Info, format.to_string()),
        );

        let table = Table {
            entries,
            timestamp: Some(TableEntry::new_without_symbol(
                Tag::Timestamp,
                "{=u8:µs}".to_owned(),
            )),
        };

        let frame = super::decode(&bytes, &table).unwrap().0;
        assert_eq!(frame.display(false).to_string(), expectation.to_owned());
    }

    #[test]
    fn decode() {
        let mut entries = BTreeMap::new();
        entries.insert(
            0,
            TableEntry::new_without_symbol(Tag::Info, "Hello, world!".to_owned()),
        );
        entries.insert(
            1,
            TableEntry::new_without_symbol(Tag::Debug, "The answer is {=u8}!".to_owned()),
        );
        // [IDX, TS, 42]
        //           ^^
        //entries.insert(2, "The answer is {0:u8} {1:u16}!".to_owned());

        let table = Table {
            entries,
            timestamp: None,
        };

        let bytes = [0];
        //     index ^

        assert_eq!(
            super::decode(&bytes, &table),
            Ok((
                Frame {
                    index: 0,
                    level: Level::Info,
                    timestamp_format: None,
                    timestamp_args: vec![],
                    format: "Hello, world!",
                    args: vec![],
                },
                bytes.len(),
            ))
        );

        let bytes = [
            1,  // index
            42, // argument
        ];

        assert_eq!(
            super::decode(&bytes, &table),
            Ok((
                Frame {
                    index: 1,
                    level: Level::Debug,
                    timestamp_format: None,
                    timestamp_args: vec![],
                    format: "The answer is {=u8}!",
                    args: vec![Arg::Uxx(42)],
                },
                bytes.len(),
            ))
        );

        // TODO Format ({:?})
    }

    #[test]
    fn all_integers() {
        const FMT: &str =
            "Hello, {=u8} {=u16} {=u24} {=u32} {=u64} {=u128} {=i8} {=i16} {=i32} {=i64} {=i128}!";
        let mut entries = BTreeMap::new();
        entries.insert(0, TableEntry::new_without_symbol(Tag::Info, FMT.to_owned()));

        let table = Table {
            entries,
            timestamp: None,
        };

        let bytes = [
            0,  // index
            42, // u8
            0xff, 0xff, // u16
            0, 0, 1, // u24
            0xff, 0xff, 0xff, 0xff, // u32
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // u64
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, // u128
            0xff, // i8
            0xff, 0xff, // i16
            0xff, 0xff, 0xff, 0xff, // i32
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // i64
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, // i128
        ];

        assert_eq!(
            super::decode(&bytes, &table),
            Ok((
                Frame {
                    index: 0,
                    level: Level::Info,
                    timestamp_format: None,
                    timestamp_args: vec![],
                    format: FMT,
                    args: vec![
                        Arg::Uxx(42),                      // u8
                        Arg::Uxx(u16::max_value().into()), // u16
                        Arg::Uxx(0x10000),                 // u24
                        Arg::Uxx(u32::max_value().into()), // u32
                        Arg::Uxx(u64::max_value().into()), // u64
                        Arg::Uxx(u128::max_value()),       // u128
                        Arg::Ixx(-1),                      // i8
                        Arg::Ixx(-1),                      // i16
                        Arg::Ixx(-1),                      // i32
                        Arg::Ixx(-1),                      // i64
                        Arg::Ixx(-1),                      // i128
                    ],
                },
                bytes.len(),
            ))
        );
    }

    #[test]
    fn indices() {
        let mut entries = BTreeMap::new();
        entries.insert(
            0,
            TableEntry::new_without_symbol(Tag::Info, "The answer is {0=u8} {0=u8}!".to_owned()),
        );
        entries.insert(
            1,
            TableEntry::new_without_symbol(
                Tag::Info,
                "The answer is {1=u16} {0=u8} {1=u16}!".to_owned(),
            ),
        );

        let table = Table {
            entries,
            timestamp: None,
        };
        let bytes = [
            0,  // index
            42, // argument
        ];

        assert_eq!(
            super::decode(&bytes, &table),
            Ok((
                Frame {
                    index: 0,
                    level: Level::Info,
                    timestamp_format: None,
                    timestamp_args: vec![],
                    format: "The answer is {0=u8} {0=u8}!",
                    args: vec![Arg::Uxx(42)],
                },
                bytes.len(),
            ))
        );

        let bytes = [
            1,  // index
            42, // u8
            0xff, 0xff, // u16
        ];

        assert_eq!(
            super::decode(&bytes, &table),
            Ok((
                Frame {
                    index: 1,
                    level: Level::Info,
                    timestamp_format: None,
                    timestamp_args: vec![],
                    format: "The answer is {1=u16} {0=u8} {1=u16}!",
                    args: vec![Arg::Uxx(42), Arg::Uxx(0xffff)],
                },
                bytes.len(),
            ))
        );
    }

    #[test]
    fn format() {
        let mut entries = BTreeMap::new();
        entries.insert(
            0,
            TableEntry::new_without_symbol(Tag::Info, "x={=?}".to_owned()),
        );
        entries.insert(
            1,
            TableEntry::new_without_symbol(Tag::Derived, "Foo {{ x: {=u8} }}".to_owned()),
        );

        let table = Table {
            entries,
            timestamp: None,
        };

        let bytes = [
            0,  // index
            1,  // index of the struct
            42, // Foo.x
        ];

        assert_eq!(
            super::decode(&bytes, &table),
            Ok((
                Frame {
                    index: 0,
                    level: Level::Info,
                    timestamp_format: None,
                    timestamp_args: vec![],
                    format: "x={=?}",
                    args: vec![Arg::Format {
                        format: "Foo {{ x: {=u8} }}",
                        args: vec![Arg::Uxx(42)]
                    }],
                },
                bytes.len(),
            ))
        );
    }

    #[test]
    fn display() {
        let mut entries = BTreeMap::new();
        entries.insert(
            0,
            TableEntry::new_without_symbol(Tag::Info, "x={=?}".to_owned()),
        );
        entries.insert(
            1,
            TableEntry::new_without_symbol(Tag::Derived, "Foo {{ x: {=u8} }}".to_owned()),
        );

        let table = Table {
            entries,
            timestamp: Some(TableEntry::new_without_symbol(
                Tag::Timestamp,
                "{=u8:µs}".to_owned(),
            )),
        };

        let bytes = [
            0,  // index
            2,  // timestamp
            1,  // index of the struct
            42, // Foo.x
        ];

        let frame = super::decode(&bytes, &table).unwrap().0;
        assert_eq!(
            frame.display(false).to_string(),
            "0.000002 INFO x=Foo { x: 42 }"
        );
    }

    #[test]
    fn bools_simple() {
        let bytes = [
            0,          // index
            2,          // timestamp
            true as u8, // the logged bool value
        ];

        decode_and_expect("my bool={=bool}", &bytes, "0.000002 INFO my bool=true");
    }

    #[test]
    fn bools_max_capacity() {
        let bytes = [
            0,           // index
            2,           // timestamp
            0b0110_0001, // the first 8 logged bool values
        ];

        decode_and_expect(
            "bool capacity {=bool} {=bool} {=bool} {=bool} {=bool} {=bool} {=bool} {=bool}",
            &bytes,
            "0.000002 INFO bool capacity false true true false false false false true",
        );
    }

    #[test]
    fn bools_more_than_fit_in_one_byte() {
        let bytes = [
            0,           // index
            2,           // timestamp
            0b0110_0001, // the first 8 logged bool values
            0b1,         // the final logged bool value
        ];

        decode_and_expect(
            "bool overflow {=bool} {=bool} {=bool} {=bool} {=bool} {=bool} {=bool} {=bool} {=bool}",
            &bytes,
            "0.000002 INFO bool overflow false true true false false false false true true",
        );

        // Ensure that bools are compressed into the first byte even when there's non-bool values
        // between them.
        let bytes = [
            0,           // index
            2,           // timestamp
            0xff,        // the logged u8
            0b0110_0001, // the first 8 logged bool values
            0b1,         // the final logged bool value
        ];

        decode_and_expect(
            "bool overflow {=bool} {=u8} {=bool} {=bool} {=bool} {=bool} {=bool} {=bool} {=bool} {=bool}",
            &bytes,
            "0.000002 INFO bool overflow false 255 true true false false false false true true",
        );

        // Ensure that bools are compressed into the first byte even when there's a non-bool value
        // right between between the two compression blocks.
        let bytes = [
            0,           // index
            2,           // timestamp
            0b0110_0001, // the first 8 logged bool values
            0xff,        // the logged u8
            0b1,         // the final logged bool value
        ];

        decode_and_expect(
            "bool overflow {=bool} {=bool} {=bool} {=bool} {=bool} {=bool} {=bool} {=bool} {=u8} {=bool}",
            &bytes,
            "0.000002 INFO bool overflow false true true false false false false true 255 true",
        );
    }

    #[test]
    fn bools_mixed() {
        let bytes = [
            0,       // index
            2,       // timestamp
            9 as u8, // a uint in between
            0b101,   // 3 packed bools
        ];

        decode_and_expect(
            "hidden bools {=bool} {=u8} {=bool} {=bool}",
            &bytes,
            "0.000002 INFO hidden bools true 9 false true",
        );
    }

    #[test]
    fn bools_mixed_no_trailing_bool() {
        let bytes = [
            0,   // index
            2,   // timestamp
            9,   // a u8 in between
            0b0, // 3 packed bools
        ];

        decode_and_expect(
            "no trailing bools {=bool} {=u8}",
            &bytes,
            "0.000002 INFO no trailing bools false 9",
        );
    }

    #[test]
    fn bools_bool_struct() {
        /*
        emulate
        #[derive(Format)]
        struct Flags {
            a: bool,
            b: bool,
            c: bool,
        }

        defmt::info!("{:bool} {:?}", true, Flags {a: true, b: false, c: true });
        */

        let mut entries = BTreeMap::new();
        entries.insert(
            0,
            TableEntry::new_without_symbol(Tag::Info, "{=bool} {=?}".to_owned()),
        );
        entries.insert(
            1,
            TableEntry::new_without_symbol(
                Tag::Derived,
                "Flags {{ a: {=bool}, b: {=bool}, c: {=bool} }}".to_owned(),
            ),
        );

        let table = Table {
            entries,
            timestamp: Some(TableEntry::new_without_symbol(
                Tag::Timestamp,
                "{=u8:µs}".to_owned(),
            )),
        };

        let bytes = [
            0,      // index
            2,      // timestamp
            1,      // index of Flags { a: {:bool}, b: {:bool}, c: {:bool} }
            0b1101, // 4 packed bools
        ];

        let frame = super::decode(&bytes, &table).unwrap().0;
        assert_eq!(
            frame.display(false).to_string(),
            "0.000002 INFO true Flags { a: true, b: false, c: true }"
        );
    }

    #[test]
    fn bitfields() {
        let bytes = [
            0,           // index
            2,           // timestamp
            0b1110_0101, // u8
        ];
        decode_and_expect(
            "x: {0=0..4:b}, y: {0=3..8:b}",
            &bytes,
            "0.000002 INFO x: 0b101, y: 0b11100",
        );
    }

    #[test]
    fn bitfields_reverse_order() {
        let bytes = [
            0,           // index
            2,           // timestamp
            0b1101_0010, // u8
        ];
        decode_and_expect(
            "x: {0=0..7:b}, y: {0=3..5:b}",
            &bytes,
            "0.000002 INFO x: 0b1010010, y: 0b10",
        );
    }

    #[test]
    fn bitfields_different_indices() {
        let bytes = [
            0,           // index
            2,           // timestamp
            0b1111_0000, // u8
            0b1110_0101, // u8
        ];
        decode_and_expect(
            "#0: {0=0..5:b}, #1: {1=3..8:b}",
            &bytes,
            "0.000002 INFO #0: 0b10000, #1: 0b11100",
        );
    }

    #[test]
    fn bitfields_u16() {
        let bytes = [
            0, // index
            2, // timestamp
            0b1111_0000,
            0b1110_0101, // u16
        ];
        decode_and_expect("x: {0=7..12:b}", &bytes, "0.000002 INFO x: 0b1011");
    }

    #[test]
    fn bitfields_mixed_types() {
        let bytes = [
            0, // index
            2, // timestamp
            0b1111_0000,
            0b1110_0101, // u16
            0b1111_0001, // u8
        ];
        decode_and_expect(
            "#0: {0=7..12:b}, #1: {1=0..5:b}",
            &bytes,
            "0.000002 INFO #0: 0b1011, #1: 0b10001",
        );
    }

    #[test]
    fn bitfields_mixed() {
        let bytes = [
            0, // index
            2, // timestamp
            0b1111_0000,
            0b1110_0101, // u16 bitfields
            42,          // u8
            0b1111_0001, // u8 bitfields
        ];
        decode_and_expect(
            "#0: {0=7..12:b}, #1: {1=u8}, #2: {2=0..5:b}",
            &bytes,
            "0.000002 INFO #0: 0b1011, #1: 42, #2: 0b10001",
        );
    }

    #[test]
    fn bitfields_across_boundaries() {
        let bytes = [
            0, // index
            2, // timestamp
            0b1101_0010,
            0b0110_0011, // u16
        ];
        decode_and_expect(
            "bitfields {0=0..7:b} {0=9..14:b}",
            &bytes,
            "0.000002 INFO bitfields 0b1010010 0b10001",
        );
    }

    #[test]
    fn bitfields_across_boundaries_diff_indices() {
        let bytes = [
            0, // index
            2, // timestamp
            0b1101_0010,
            0b0110_0011, // u16
            0b1111_1111, // truncated u16
        ];
        decode_and_expect(
            "bitfields {0=0..7:b} {0=9..14:b} {1=8..10:b}",
            &bytes,
            "0.000002 INFO bitfields 0b1010010 0b10001 0b11",
        );
    }

    #[test]
    fn bitfields_truncated_front() {
        let bytes = [
            0,           // index
            2,           // timestamp
            0b0110_0011, // truncated(!) u16
        ];
        decode_and_expect(
            "bitfields {0=9..14:b}",
            &bytes,
            "0.000002 INFO bitfields 0b10001",
        );
    }

    #[test]
    fn bitfields_non_truncated_u32() {
        let bytes = [
            0,           // index
            2,           // timestamp
            0b0110_0011, // -
            0b0000_1111, //  |
            0b0101_1010, //  | u32
            0b1100_0011, // -
        ];
        decode_and_expect(
            "bitfields {0=0..2:b} {0=28..31:b}",
            &bytes,
            "0.000002 INFO bitfields 0b11 0b100",
        );
    }

    #[test]
    fn bitfields_u128() {
        let bytes = [
            0,           // index
            2,           // timestamp
            0b1110_0101, // 120..127
            0b1110_0101, // 112..119
            0b0000_0000, // 104..111
            0b0000_0000, // 96..103
            0b0000_0000, // 88..95
            0b0000_0000, // 80..87
            0b0000_0000, // 72..79
            0b0000_0000, // 64..71
            0b0000_0000, // 56..63
            0b0000_0000, // 48..55
            0b0000_0000, // 40..47
            0b0000_0000, // 32..39
            0b0000_0000, // 24..31
            0b0000_0000, // 16..23
            0b0000_0000, // 8..15
            0b0000_0000, // 0..7
        ];
        decode_and_expect("x: {0=119..124:b}", &bytes, "0.000002 INFO x: 0b1011");
    }

    #[test]
    fn slice() {
        let bytes = [
            0, // index
            2, // timestamp
            2, // length of the slice
            23, 42, // slice content
        ];
        decode_and_expect("x={=[u8]}", &bytes, "0.000002 INFO x=[23, 42]");
    }

    #[test]
    fn slice_with_trailing_args() {
        let bytes = [
            0, // index
            2, // timestamp
            2, // length of the slice
            23, 42, // slice content
            1,  // trailing arg
        ];

        decode_and_expect(
            "x={=[u8]} trailing arg={=u8}",
            &bytes,
            "0.000002 INFO x=[23, 42] trailing arg=1",
        );
    }

    #[test]
    fn string_hello_world() {
        let bytes = [
            0, // index
            2, // timestamp
            5, // length of the string
            b'W', b'o', b'r', b'l', b'd',
        ];

        decode_and_expect("Hello {=str}", &bytes, "0.000002 INFO Hello World");
    }

    #[test]
    fn string_with_trailing_data() {
        let bytes = [
            0, // index
            2, // timestamp
            5, // length of the string
            b'W', b'o', b'r', b'l', b'd', 125, // trailing data
        ];

        decode_and_expect(
            "Hello {=str} {=u8}",
            &bytes,
            "0.000002 INFO Hello World 125",
        );
    }

    #[test]
    fn char_data() {
        let bytes = [
            0, // index
            2, // timestamp
            0x61, 0x00, 0x00, 0x00, // char 'a'
            0x9C, 0xF4, 0x01, 0x00, // Purple heart emoji
        ];

        decode_and_expect(
            "Supports ASCII {=char} and Unicode {=char}",
            &bytes,
            "0.000002 INFO Supports ASCII a and Unicode 💜",
        );
    }

    #[test]
    fn option() {
        let mut entries = BTreeMap::new();
        entries.insert(
            4,
            TableEntry::new_without_symbol(Tag::Info, "x={=?}".to_owned()),
        );
        entries.insert(
            3,
            TableEntry::new_without_symbol(Tag::Derived, "None|Some({=?})".to_owned()),
        );
        entries.insert(
            2,
            TableEntry::new_without_symbol(Tag::Derived, "{=u8}".to_owned()),
        );

        let table = Table {
            entries,
            timestamp: Some(TableEntry::new_without_symbol(
                Tag::Timestamp,
                "{=u8:µs}".to_owned(),
            )),
        };

        let bytes = [
            4,  // string index (INFO)
            0,  // timestamp
            3,  // string index (enum)
            1,  // Some discriminant
            2,  // string index (u8)
            42, // Some.0
        ];

        let frame = super::decode(&bytes, &table).unwrap().0;
        assert_eq!(frame.display(false).to_string(), "0.000000 INFO x=Some(42)");

        let bytes = [
            4, // string index (INFO)
            1, // timestamp
            3, // string index (enum)
            0, // None discriminant
        ];

        let frame = super::decode(&bytes, &table).unwrap().0;
        assert_eq!(frame.display(false).to_string(), "0.000001 INFO x=None");
    }

    #[test]
    fn merge_bitfields_simple() {
        let mut params = vec![
            Parameter {
                index: 0,
                ty: Type::BitField(0..3),
                hint: None,
            },
            Parameter {
                index: 0,
                ty: Type::BitField(4..7),
                hint: None,
            },
        ];

        merge_bitfields(&mut params);
        assert_eq!(
            params,
            vec![Parameter {
                index: 0,
                ty: Type::BitField(0..7),
                hint: None,
            }]
        );
    }

    #[test]
    fn merge_bitfields_overlap() {
        let mut params = vec![
            Parameter {
                index: 0,
                ty: Type::BitField(1..3),
                hint: None,
            },
            Parameter {
                index: 0,
                ty: Type::BitField(2..5),
                hint: None,
            },
        ];

        merge_bitfields(&mut params);
        assert_eq!(
            params,
            vec![Parameter {
                index: 0,
                ty: Type::BitField(1..5),
                hint: None,
            }]
        );
    }

    #[test]
    fn merge_bitfields_multiple_indices() {
        let mut params = vec![
            Parameter {
                index: 0,
                ty: Type::BitField(0..3),
                hint: None,
            },
            Parameter {
                index: 1,
                ty: Type::BitField(1..3),
                hint: None,
            },
            Parameter {
                index: 1,
                ty: Type::BitField(4..5),
                hint: None,
            },
        ];

        merge_bitfields(&mut params);
        assert_eq!(
            params,
            vec![
                Parameter {
                    index: 0,
                    ty: Type::BitField(0..3),
                    hint: None,
                },
                Parameter {
                    index: 1,
                    ty: Type::BitField(1..5),
                    hint: None,
                }
            ]
        );
    }

    #[test]
    fn merge_bitfields_overlap_non_consecutive_indices() {
        let mut params = vec![
            Parameter {
                index: 0,
                ty: Type::BitField(0..3),
                hint: None,
            },
            Parameter {
                index: 1,
                ty: Type::U8,
                hint: None,
            },
            Parameter {
                index: 2,
                ty: Type::BitField(1..4),
                hint: None,
            },
            Parameter {
                index: 2,
                ty: Type::BitField(4..5),
                hint: None,
            },
        ];

        merge_bitfields(&mut params);
        // note: current implementation appends merged bitfields to the end. this is not a must
        assert_eq!(
            params,
            vec![
                Parameter {
                    index: 1,
                    ty: Type::U8,
                    hint: None,
                },
                Parameter {
                    index: 0,
                    ty: Type::BitField(0..3),
                    hint: None,
                },
                Parameter {
                    index: 2,
                    ty: Type::BitField(1..5),
                    hint: None,
                }
            ]
        );
    }
}