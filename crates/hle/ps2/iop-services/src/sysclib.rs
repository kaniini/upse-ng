// SPDX-License-Identifier: LGPL-2.1-or-later

use crate::services::{read_bytes, read_c_string, read_u32, write_memory, write_u32};
use crate::{ServiceContext, ServiceError, ServiceMemory};

const V0: usize = 2;
const SP: usize = 29;
const FP: usize = 30;
const RA: usize = 31;
const MAX_MEMORY_OPERATION: usize = 16 * 1024 * 1024;
const MAX_STRING: usize = 4096;
const MAX_FORMATTED: usize = 16 * 1024;
const FORMAT_LEFT: u8 = 1 << 0;
const FORMAT_PLUS: u8 = 1 << 1;
const FORMAT_SPACE: u8 = 1 << 2;
const FORMAT_ALTERNATE: u8 = 1 << 3;
const FORMAT_ZERO: u8 = 1 << 4;

pub(crate) fn dispatch<M: ServiceMemory>(
    ordinal: u16,
    context: &mut ServiceContext,
    memory: &mut M,
    strtok_next: &mut Option<u32>,
) -> Result<u32, ServiceError> {
    let [a0, a1, a2, _a3] = context.arguments();
    match ordinal {
        4 => save_jump(context, memory, a0),
        5 => restore_jump(context, memory, a0, a1),
        6 => Ok(map_character(a0, u8::to_ascii_uppercase)),
        7 => Ok(map_character(a0, u8::to_ascii_lowercase)),
        8 => Ok(ctype_flags(a0)),
        9 | 39 => Ok(0),
        10 => memchr(memory, a0, a1, a2),
        11 | 15 => compare_memory(memory, a0, a1, a2),
        12 => copy_memory(memory, a0, a1, a2),
        13 => move_memory(memory, a0, a1, a2),
        14 | 41 => set_memory(memory, a0, a1, a2),
        16 => copy_memory(memory, a1, a0, a2),
        17 => set_memory(memory, a0, 0, a1),
        19 | 42 => sprintf(context, memory),
        20 => strcat(memory, a0, a1),
        21 | 25 => strchr(memory, a0, a1, false),
        22 => compare_strings(memory, a0, a1, None),
        23 => copy_string(memory, a0, a1, None),
        24 => string_span(memory, a0, a1, false),
        26 | 32 => strchr(memory, a0, a1, true),
        27 => Ok(u32::try_from(read_c_string(memory, a0, MAX_STRING)?.len()).unwrap_or(u32::MAX)),
        28 => strncat(memory, a0, a1, a2),
        29 => compare_strings(memory, a0, a1, Some(a2)),
        30 => copy_string(memory, a0, a1, Some(a2)),
        31 => strpbrk(memory, a0, a1),
        33 => string_span(memory, a0, a1, true),
        34 => strstr(memory, a0, a1),
        35 => strtok(memory, a0, a1, strtok_next),
        36 => parse_integer(memory, a0, 10, true),
        37 => atob(memory, a0, a1),
        38 => parse_integer(memory, a0, a2, false),
        40 => copy_words(memory, a0, a1, a2),
        43 => strtok_r(memory, a0, a1, a2),
        44 => Ok(u32::MAX),
        _ => Err(ServiceError::InvalidArgument {
            operation: "sysclib",
            detail: "local ordinal is not implemented",
        }),
    }
}

pub(crate) fn dispatch_stdio<M: ServiceMemory>(
    ordinal: u16,
    context: &ServiceContext,
    memory: &mut M,
    tty: &mut Vec<String>,
) -> Result<u32, ServiceError> {
    let [a0, a1, _a2, _] = context.arguments();
    match ordinal {
        4 => {
            let text = format_guest(context, memory, 0)?;
            let length = text.len();
            tty.push(text);
            Ok(u32::try_from(length).unwrap_or(u32::MAX))
        }
        5 | 8 | 10 | 13 => Ok(u32::MAX),
        6 => {
            let byte = a0.to_le_bytes()[0];
            tty.push(char::from(byte).to_string());
            Ok(u32::from(byte))
        }
        7 => {
            let mut text = read_c_string(memory, a0, MAX_STRING)?;
            text.push('\n');
            tty.push(text);
            Ok(0)
        }
        9 | 14 => {
            let text = format_guest(context, memory, 1)?;
            let length = text.len();
            if a0 == 1 || a0 == 2 {
                tty.push(text);
                Ok(u32::try_from(length).unwrap_or(u32::MAX))
            } else {
                Ok(u32::MAX)
            }
        }
        11 => {
            if a0 == 1 || a0 == 2 {
                tty.push(char::from(a1.to_le_bytes()[0]).to_string());
                Ok(a1 & 0xff)
            } else {
                Ok(u32::MAX)
            }
        }
        12 => {
            if a0 == 1 || a0 == 2 {
                let text = read_c_string(memory, a1, MAX_STRING)?;
                let length = text.len();
                tty.push(text);
                Ok(u32::try_from(length).unwrap_or(u32::MAX))
            } else {
                Ok(u32::MAX)
            }
        }
        _ => Err(ServiceError::InvalidArgument {
            operation: "stdio",
            detail: "local ordinal is not implemented",
        }),
    }
}

pub(crate) fn dispatch_kprintf<M: ServiceMemory>(
    context: &ServiceContext,
    memory: &M,
    tty: &mut Vec<String>,
) -> Result<u32, ServiceError> {
    let text = format_guest(context, memory, 0)?;
    let length = text.len();
    tty.push(text);
    Ok(u32::try_from(length).unwrap_or(u32::MAX))
}

fn save_jump<M: ServiceMemory>(
    context: &ServiceContext,
    memory: &mut M,
    address: u32,
) -> Result<u32, ServiceError> {
    for (index, register) in (16..=23).chain([SP, FP, RA]).enumerate() {
        let offset = u32::try_from(index * 4).unwrap_or(u32::MAX);
        write_u32(
            memory,
            address.wrapping_add(offset),
            context.register(register).unwrap_or(0),
        )?;
    }
    Ok(0)
}

fn restore_jump<M: ServiceMemory>(
    context: &mut ServiceContext,
    memory: &M,
    address: u32,
    value: u32,
) -> Result<u32, ServiceError> {
    for (index, register) in (16..=23).chain([SP, FP, RA]).enumerate() {
        let offset = u32::try_from(index * 4).unwrap_or(u32::MAX);
        context.set_register(register, read_u32(memory, address.wrapping_add(offset))?);
    }
    let value = if value == 0 { 1 } else { value };
    context.pc = context.register(RA).unwrap_or(0);
    context.set_register(V0, value);
    Ok(value)
}

fn map_character(value: u32, map: fn(&u8) -> u8) -> u32 {
    let byte = value.to_le_bytes()[0];
    u32::from(map(&byte))
}

fn ctype_flags(value: u32) -> u32 {
    let byte = value.to_le_bytes()[0];
    let character = char::from(byte);
    u32::from(character.is_ascii_control())
        | (u32::from(character.is_ascii_whitespace()) << 1)
        | (u32::from(character.is_ascii_digit()) << 2)
        | (u32::from(character.is_ascii_uppercase()) << 3)
        | (u32::from(character.is_ascii_lowercase()) << 4)
        | (u32::from(character.is_ascii_punctuation()) << 5)
        | (u32::from(character.is_ascii_hexdigit()) << 6)
}

fn checked_size(size: u32) -> Result<usize, ServiceError> {
    let size =
        usize::try_from(size).map_err(|_| ServiceError::ResourceLimit("memory operation"))?;
    if size > MAX_MEMORY_OPERATION {
        return Err(ServiceError::ResourceLimit("memory operation"));
    }
    Ok(size)
}

fn memchr<M: ServiceMemory>(
    memory: &M,
    address: u32,
    value: u32,
    size: u32,
) -> Result<u32, ServiceError> {
    let bytes = read_bytes(memory, address, checked_size(size)?)?;
    Ok(bytes
        .iter()
        .position(|byte| *byte == value.to_le_bytes()[0])
        .and_then(|offset| address.checked_add(u32::try_from(offset).ok()?))
        .unwrap_or(0))
}

fn compare_memory<M: ServiceMemory>(
    memory: &M,
    left: u32,
    right: u32,
    size: u32,
) -> Result<u32, ServiceError> {
    let size = checked_size(size)?;
    let left = read_bytes(memory, left, size)?;
    let right = read_bytes(memory, right, size)?;
    let result = left
        .iter()
        .zip(right)
        .find_map(|(left, right)| (left != &right).then(|| i32::from(*left) - i32::from(right)))
        .unwrap_or(0);
    Ok(u32::from_ne_bytes(result.to_ne_bytes()))
}

fn copy_memory<M: ServiceMemory>(
    memory: &mut M,
    destination: u32,
    source: u32,
    size: u32,
) -> Result<u32, ServiceError> {
    let bytes = read_bytes(memory, source, checked_size(size)?)?;
    write_memory(memory, destination, &bytes)?;
    Ok(destination)
}

fn move_memory<M: ServiceMemory>(
    memory: &mut M,
    destination: u32,
    source: u32,
    size: u32,
) -> Result<u32, ServiceError> {
    copy_memory(memory, destination, source, size)
}

fn set_memory<M: ServiceMemory>(
    memory: &mut M,
    destination: u32,
    value: u32,
    size: u32,
) -> Result<u32, ServiceError> {
    let bytes = vec![value.to_le_bytes()[0]; checked_size(size)?];
    write_memory(memory, destination, &bytes)?;
    Ok(destination)
}

fn sprintf<M: ServiceMemory>(
    context: &ServiceContext,
    memory: &mut M,
) -> Result<u32, ServiceError> {
    let destination = context.arguments()[0];
    let text = format_guest(context, memory, 1)?;
    let mut bytes = text.into_bytes();
    let length = bytes.len();
    bytes.push(0);
    write_memory(memory, destination, &bytes)?;
    Ok(u32::try_from(length).unwrap_or(u32::MAX))
}

fn strcat<M: ServiceMemory>(
    memory: &mut M,
    destination: u32,
    source: u32,
) -> Result<u32, ServiceError> {
    let destination_text = read_c_string(memory, destination, MAX_STRING)?;
    let source_text = read_c_string(memory, source, MAX_STRING)?;
    let offset =
        u32::try_from(destination_text.len()).map_err(|_| ServiceError::ResourceLimit("string"))?;
    let mut bytes = source_text.into_bytes();
    bytes.push(0);
    write_memory(memory, destination.wrapping_add(offset), &bytes)?;
    Ok(destination)
}

fn strchr<M: ServiceMemory>(
    memory: &M,
    address: u32,
    value: u32,
    reverse: bool,
) -> Result<u32, ServiceError> {
    let string = read_c_string(memory, address, MAX_STRING)?;
    let needle = value.to_le_bytes()[0];
    let position = if reverse {
        string.as_bytes().iter().rposition(|byte| *byte == needle)
    } else {
        string.as_bytes().iter().position(|byte| *byte == needle)
    };
    Ok(position
        .and_then(|offset| address.checked_add(u32::try_from(offset).ok()?))
        .unwrap_or_else(|| {
            if needle == 0 {
                address.wrapping_add(u32::try_from(string.len()).unwrap_or(u32::MAX))
            } else {
                0
            }
        }))
}

fn compare_strings<M: ServiceMemory>(
    memory: &M,
    left: u32,
    right: u32,
    limit: Option<u32>,
) -> Result<u32, ServiceError> {
    let mut left = read_c_string(memory, left, MAX_STRING)?.into_bytes();
    let mut right = read_c_string(memory, right, MAX_STRING)?.into_bytes();
    if let Some(limit) = limit {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        left.truncate(limit);
        right.truncate(limit);
    }
    let result = left.cmp(&right) as i32;
    Ok(u32::from_ne_bytes(result.to_ne_bytes()))
}

fn copy_string<M: ServiceMemory>(
    memory: &mut M,
    destination: u32,
    source: u32,
    limit: Option<u32>,
) -> Result<u32, ServiceError> {
    let source = read_c_string(memory, source, MAX_STRING)?;
    let mut bytes = source.into_bytes();
    if let Some(limit) = limit {
        let limit = checked_size(limit)?;
        bytes.truncate(limit);
        bytes.resize(limit, 0);
    } else {
        bytes.push(0);
    }
    write_memory(memory, destination, &bytes)?;
    Ok(destination)
}

fn string_span<M: ServiceMemory>(
    memory: &M,
    address: u32,
    set: u32,
    accept: bool,
) -> Result<u32, ServiceError> {
    let string = read_c_string(memory, address, MAX_STRING)?;
    let set = read_c_string(memory, set, MAX_STRING)?;
    let count = string
        .bytes()
        .take_while(|byte| set.as_bytes().contains(byte) == accept)
        .count();
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

fn strncat<M: ServiceMemory>(
    memory: &mut M,
    destination: u32,
    source: u32,
    limit: u32,
) -> Result<u32, ServiceError> {
    let destination_text = read_c_string(memory, destination, MAX_STRING)?;
    let source_text = read_c_string(memory, source, MAX_STRING)?;
    let limit = checked_size(limit)?;
    let offset = u32::try_from(destination_text.len()).unwrap_or(u32::MAX);
    let mut bytes = source_text.into_bytes();
    bytes.truncate(limit);
    bytes.push(0);
    write_memory(memory, destination.wrapping_add(offset), &bytes)?;
    Ok(destination)
}

fn strpbrk<M: ServiceMemory>(memory: &M, address: u32, set: u32) -> Result<u32, ServiceError> {
    let string = read_c_string(memory, address, MAX_STRING)?;
    let set = read_c_string(memory, set, MAX_STRING)?;
    Ok(string
        .bytes()
        .position(|byte| set.as_bytes().contains(&byte))
        .and_then(|offset| address.checked_add(u32::try_from(offset).ok()?))
        .unwrap_or(0))
}

fn strstr<M: ServiceMemory>(memory: &M, haystack: u32, needle: u32) -> Result<u32, ServiceError> {
    let haystack_text = read_c_string(memory, haystack, MAX_STRING)?;
    let needle_text = read_c_string(memory, needle, MAX_STRING)?;
    Ok(haystack_text
        .find(&needle_text)
        .and_then(|offset| haystack.checked_add(u32::try_from(offset).ok()?))
        .unwrap_or(0))
}

fn strtok<M: ServiceMemory>(
    memory: &mut M,
    string: u32,
    delimiters: u32,
    next: &mut Option<u32>,
) -> Result<u32, ServiceError> {
    let start = if string == 0 {
        next.unwrap_or(0)
    } else {
        string
    };
    tokenize(memory, start, delimiters, next)
}

fn strtok_r<M: ServiceMemory>(
    memory: &mut M,
    string: u32,
    delimiters: u32,
    next_address: u32,
) -> Result<u32, ServiceError> {
    let mut next = if string == 0 {
        Some(read_u32(memory, next_address)?)
    } else {
        Some(string)
    };
    let result = tokenize(memory, next.unwrap_or(0), delimiters, &mut next)?;
    write_u32(memory, next_address, next.unwrap_or(0))?;
    Ok(result)
}

fn tokenize<M: ServiceMemory>(
    memory: &mut M,
    start: u32,
    delimiters: u32,
    next: &mut Option<u32>,
) -> Result<u32, ServiceError> {
    if start == 0 {
        *next = None;
        return Ok(0);
    }
    let text = read_c_string(memory, start, MAX_STRING)?;
    let delimiters = read_c_string(memory, delimiters, MAX_STRING)?;
    let bytes = text.as_bytes();
    let first = bytes
        .iter()
        .position(|byte| !delimiters.as_bytes().contains(byte));
    let Some(first) = first else {
        *next = None;
        return Ok(0);
    };
    let end = bytes[first..]
        .iter()
        .position(|byte| delimiters.as_bytes().contains(byte))
        .map(|offset| first + offset);
    let token = start.wrapping_add(u32::try_from(first).unwrap_or(u32::MAX));
    if let Some(end) = end {
        let end_address = start.wrapping_add(u32::try_from(end).unwrap_or(u32::MAX));
        write_memory(memory, end_address, &[0])?;
        *next = end_address.checked_add(1);
    } else {
        *next = None;
    }
    Ok(token)
}

fn parse_integer<M: ServiceMemory>(
    memory: &M,
    address: u32,
    base: u32,
    signed: bool,
) -> Result<u32, ServiceError> {
    let text = read_c_string(memory, address, MAX_STRING)?;
    let trimmed = text.trim_start();
    let (negative, digits) = trimmed
        .strip_prefix('-')
        .map_or((false, trimmed), |digits| (true, digits));
    let base = if base == 0 {
        if digits.starts_with("0x") || digits.starts_with("0X") {
            16
        } else if digits.starts_with('0') {
            8
        } else {
            10
        }
    } else {
        base
    };
    if !(2..=36).contains(&base) {
        return Ok(0);
    }
    let digits = if base == 16 {
        digits
            .strip_prefix("0x")
            .or_else(|| digits.strip_prefix("0X"))
            .unwrap_or(digits)
    } else {
        digits
    };
    let mut value = 0_u32;
    for character in digits.chars() {
        let Some(digit) = character.to_digit(base) else {
            break;
        };
        value = value.wrapping_mul(base).wrapping_add(digit);
    }
    if signed && negative {
        value = value.wrapping_neg();
    }
    Ok(value)
}

fn atob<M: ServiceMemory>(memory: &mut M, output: u32, text: u32) -> Result<u32, ServiceError> {
    let value = parse_integer(memory, text, 0, true)?;
    write_u32(memory, output, value)?;
    Ok(1)
}

fn copy_words<M: ServiceMemory>(
    memory: &mut M,
    destination: u32,
    source: u32,
    count: u32,
) -> Result<u32, ServiceError> {
    let size = count
        .checked_mul(4)
        .ok_or(ServiceError::ResourceLimit("word copy"))?;
    copy_memory(memory, destination, source, size)
}

fn format_guest<M: ServiceMemory>(
    context: &ServiceContext,
    memory: &M,
    format_argument: usize,
) -> Result<String, ServiceError> {
    let format_address = argument(context, memory, format_argument)?;
    let format = read_c_string(memory, format_address, MAX_STRING)?;
    let mut output = String::new();
    let mut characters = format.chars().peekable();
    let mut next_argument = format_argument + 1;
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        if characters.peek() == Some(&'%') {
            characters.next();
            output.push('%');
            continue;
        }
        let mut spec = FormatSpec::default();
        while let Some(character) = characters.peek().copied() {
            match character {
                '-' => spec.flags |= FORMAT_LEFT,
                '+' => spec.flags |= FORMAT_PLUS,
                ' ' => spec.flags |= FORMAT_SPACE,
                '#' => spec.flags |= FORMAT_ALTERNATE,
                '0' => spec.flags |= FORMAT_ZERO,
                _ => break,
            }
            characters.next();
        }
        spec.width = parse_format_number(&mut characters);
        if characters.peek() == Some(&'.') {
            characters.next();
            spec.precision = Some(parse_format_number(&mut characters).unwrap_or(0));
        }
        while characters
            .peek()
            .is_some_and(|character| "hlLzjt".contains(*character))
        {
            characters.next();
        }
        let specifier = characters.next().unwrap_or('%');
        let value = argument(context, memory, next_argument)?;
        next_argument += 1;
        output.push_str(&format_value(specifier, value, spec, memory)?);
        if output.len() > MAX_FORMATTED {
            return Err(ServiceError::ResourceLimit("formatted output"));
        }
    }
    Ok(output)
}

fn format_value<M: ServiceMemory>(
    specifier: char,
    value: u32,
    spec: FormatSpec,
    memory: &M,
) -> Result<String, ServiceError> {
    Ok(match specifier {
        'd' | 'i' => {
            let signed = i32::from_ne_bytes(value.to_ne_bytes());
            format_integer(
                u64::from(signed.unsigned_abs()),
                10,
                false,
                if signed.is_negative() {
                    Some('-')
                } else if spec.has(FORMAT_PLUS) {
                    Some('+')
                } else if spec.has(FORMAT_SPACE) {
                    Some(' ')
                } else {
                    None
                },
                "",
                spec,
            )
        }
        'u' => format_integer(u64::from(value), 10, false, None, "", spec),
        'x' => format_integer(
            u64::from(value),
            16,
            false,
            None,
            if spec.has(FORMAT_ALTERNATE) && value != 0 {
                "0x"
            } else {
                ""
            },
            spec,
        ),
        'p' => format_integer(u64::from(value), 16, false, None, "0x", spec),
        'X' => format_integer(
            u64::from(value),
            16,
            true,
            None,
            if spec.has(FORMAT_ALTERNATE) && value != 0 {
                "0X"
            } else {
                ""
            },
            spec,
        ),
        'o' => format_integer(
            u64::from(value),
            8,
            false,
            None,
            if spec.has(FORMAT_ALTERNATE) && value != 0 {
                "0"
            } else {
                ""
            },
            spec,
        ),
        'c' => apply_format_width(char::from(value.to_le_bytes()[0]).to_string(), spec, false),
        's' => {
            let mut text = if value == 0 {
                "(null)".to_owned()
            } else {
                read_c_string(memory, value, MAX_STRING)?
            };
            if let Some(precision) = spec.precision {
                let end = text
                    .char_indices()
                    .nth(precision)
                    .map_or(text.len(), |(index, _)| index);
                text.truncate(end);
            }
            apply_format_width(text, spec, false)
        }
        other => {
            let mut text = String::from('%');
            text.push(other);
            text
        }
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct FormatSpec {
    flags: u8,
    width: Option<usize>,
    precision: Option<usize>,
}

impl FormatSpec {
    const fn has(self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

fn parse_format_number<I>(characters: &mut std::iter::Peekable<I>) -> Option<usize>
where
    I: Iterator<Item = char>,
{
    let mut value = None::<usize>;
    while let Some(digit) = characters
        .peek()
        .and_then(|character| character.to_digit(10))
    {
        characters.next();
        value = Some(
            value
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(usize::try_from(digit).unwrap_or(usize::MAX))
                .min(MAX_FORMATTED),
        );
    }
    value
}

fn format_integer(
    value: u64,
    radix: u32,
    uppercase: bool,
    sign: Option<char>,
    prefix: &str,
    spec: FormatSpec,
) -> String {
    let mut digits = match (radix, uppercase) {
        (8, _) => format!("{value:o}"),
        (16, false) => format!("{value:x}"),
        (16, true) => format!("{value:X}"),
        _ => value.to_string(),
    };
    if spec.precision == Some(0) && value == 0 {
        digits.clear();
    }
    if let Some(precision) = spec.precision
        && digits.len() < precision
    {
        digits.insert_str(0, &"0".repeat(precision - digits.len()));
    }
    let mut text = String::new();
    if let Some(sign) = sign {
        text.push(sign);
    }
    text.push_str(prefix);
    text.push_str(&digits);
    apply_format_width(text, spec, true)
}

fn apply_format_width(mut text: String, spec: FormatSpec, numeric: bool) -> String {
    let width = spec.width.unwrap_or(0).min(MAX_FORMATTED);
    let padding = width.saturating_sub(text.len());
    if padding == 0 {
        return text;
    }
    if spec.has(FORMAT_LEFT) {
        text.push_str(&" ".repeat(padding));
    } else if numeric && spec.has(FORMAT_ZERO) && spec.precision.is_none() {
        let sign_length = usize::from(text.starts_with(['-', '+', ' ']));
        let prefix_length = sign_length
            + usize::from(
                text[sign_length..].starts_with("0x") || text[sign_length..].starts_with("0X"),
            ) * 2;
        text.insert_str(prefix_length, &"0".repeat(padding));
    } else {
        text.insert_str(0, &" ".repeat(padding));
    }
    text
}

fn argument<M: ServiceMemory>(
    context: &ServiceContext,
    memory: &M,
    position: usize,
) -> Result<u32, ServiceError> {
    if position < 4 {
        return Ok(context.register(4 + position).unwrap_or(0));
    }
    let stack = context.register(SP).unwrap_or(0);
    let offset = 16_u32
        .checked_add(u32::try_from((position - 4) * 4).unwrap_or(u32::MAX))
        .ok_or(ServiceError::InvalidArgument {
            operation: "varargs",
            detail: "stack offset overflow",
        })?;
    read_u32(memory, stack.wrapping_add(offset))
}
