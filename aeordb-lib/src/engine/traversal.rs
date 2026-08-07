#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum TraversalIntegrity {
  #[default]
  Complete,
  DiagnosticallyPartial,
  Corrupt,
}

impl TraversalIntegrity {
  pub const fn combine(self, other: Self) -> Self {
    if self as u8 >= other as u8 {
      self
    } else {
      other
    }
  }

  pub const fn is_complete(self) -> bool {
    matches!(self, Self::Complete)
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VisitorCompletion {
  #[default]
  Exhausted,
  StoppedByVisitor,
}

impl VisitorCompletion {
  pub const fn is_exhausted(self) -> bool {
    matches!(self, Self::Exhausted)
  }
}
