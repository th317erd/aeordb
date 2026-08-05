use std::error::Error;
use std::fmt::{self, Display, Formatter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedInputClass {
  UnknownMagicOrVersion,
  UnknownRequiredCapability,
  UnknownTypeKindOrEnum,
  TruncationOrTrailingBytes,
  LengthCountOrArithmeticOverflow,
  AllocationAmplification,
  NoncanonicalBooleanOrOptionalPresence,
  NonzeroReservedOrPadding,
  ChecksumOrIntegrityMismatch,
  IdentityKeyOrGenerationMismatch,
  NoncanonicalOrderOrDuplicate,
  InvalidUtf8PathGlobOrNativePath,
  InvalidGraphEdgeOrCycle,
  AmbiguousEqualSequenceSelector,
  UnsupportedPlatformDurability,
  CrossRecordClosureMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError {
  class: MalformedInputClass,
  code: &'static str,
  context: String,
}

impl FormatError {
  pub fn new(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> Self {
    Self { class, code, context: context.into() }
  }

  pub fn class(&self) -> MalformedInputClass {
    self.class
  }

  pub fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl Display for FormatError {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    if self.context.is_empty() {
      formatter.write_str(self.code)
    } else {
      write!(formatter, "{}: {}", self.code, self.context)
    }
  }
}

impl Error for FormatError {}

pub type FormatResult<T> = Result<T, FormatError>;

#[derive(Debug)]
pub struct BoundedReader<'a> {
  bytes: &'a [u8],
  offset: usize,
  allocation_budget: usize,
  allocated_bytes: usize,
}

impl<'a> BoundedReader<'a> {
  pub fn new(bytes: &'a [u8], allocation_budget: usize) -> FormatResult<Self> {
    if bytes.len() > allocation_budget {
      return Err(FormatError::new(
        MalformedInputClass::AllocationAmplification,
        "input_exceeds_hard_cap",
        format!("{} bytes exceeds {allocation_budget}-byte cap", bytes.len()),
      ));
    }
    Ok(Self { bytes, offset: 0, allocation_budget, allocated_bytes: 0 })
  }

  pub fn position(&self) -> usize {
    self.offset
  }

  pub fn remaining(&self) -> usize {
    self.bytes.len() - self.offset
  }

  pub fn allocated_bytes(&self) -> usize {
    self.allocated_bytes
  }

  pub fn read_exact(&mut self, length: usize) -> FormatResult<&'a [u8]> {
    let end = self.offset.checked_add(length).ok_or_else(|| {
      FormatError::new(
        MalformedInputClass::LengthCountOrArithmeticOverflow,
        "reader_offset_overflow",
        format!("offset {} plus length {length}", self.offset),
      )
    })?;
    if end > self.bytes.len() {
      return Err(FormatError::new(
        MalformedInputClass::TruncationOrTrailingBytes,
        "truncated_input",
        format!("need {length} bytes at {}, only {} remain", self.offset, self.remaining()),
      ));
    }
    let value = &self.bytes[self.offset..end];
    self.offset = end;
    Ok(value)
  }

  pub fn read_u8(&mut self) -> FormatResult<u8> {
    Ok(self.read_exact(1)?[0])
  }

  pub fn read_u16(&mut self) -> FormatResult<u16> {
    Ok(u16::from_le_bytes(self.read_exact(2)?.try_into().expect("exact slice length")))
  }

  pub fn read_u32(&mut self) -> FormatResult<u32> {
    Ok(u32::from_le_bytes(self.read_exact(4)?.try_into().expect("exact slice length")))
  }

  pub fn read_u64(&mut self) -> FormatResult<u64> {
    Ok(u64::from_le_bytes(self.read_exact(8)?.try_into().expect("exact slice length")))
  }

  pub fn read_i64(&mut self) -> FormatResult<i64> {
    Ok(i64::from_le_bytes(self.read_exact(8)?.try_into().expect("exact slice length")))
  }

  pub fn read_u32_length_prefixed(&mut self, field_cap: usize) -> FormatResult<Vec<u8>> {
    let declared = usize::try_from(self.read_u32()?).map_err(|_| {
      FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, "length_conversion_overflow", "u32 length does not fit usize")
    })?;
    if declared > field_cap {
      return Err(FormatError::new(
        MalformedInputClass::AllocationAmplification,
        "declared_length_exceeds_cap",
        format!("declared {declared} bytes exceeds {field_cap}-byte field cap"),
      ));
    }
    let new_total = self.allocated_bytes.checked_add(declared).ok_or_else(|| {
      FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, "allocation_counter_overflow", "allocation counter overflow")
    })?;
    if new_total > self.allocation_budget {
      return Err(FormatError::new(
        MalformedInputClass::AllocationAmplification,
        "allocation_budget_exceeded",
        format!("allocation total {new_total} exceeds {}-byte budget", self.allocation_budget),
      ));
    }
    let value = self.read_exact(declared)?.to_vec();
    self.allocated_bytes = new_total;
    Ok(value)
  }

  pub fn checked_array_bytes(count: usize, element_width: usize, cap: usize) -> FormatResult<usize> {
    let bytes = count.checked_mul(element_width).ok_or_else(|| {
      FormatError::new(
        MalformedInputClass::LengthCountOrArithmeticOverflow,
        "array_length_overflow",
        format!("count {count} times width {element_width}"),
      )
    })?;
    if bytes > cap {
      return Err(FormatError::new(
        MalformedInputClass::AllocationAmplification,
        "array_exceeds_cap",
        format!("array requires {bytes} bytes, cap is {cap}"),
      ));
    }
    Ok(bytes)
  }

  pub fn finish(&self) -> FormatResult<()> {
    if self.offset != self.bytes.len() {
      return Err(FormatError::new(
        MalformedInputClass::TruncationOrTrailingBytes,
        "trailing_bytes",
        format!("{} bytes remain", self.bytes.len() - self.offset),
      ));
    }
    Ok(())
  }
}
