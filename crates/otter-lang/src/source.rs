//! Source text and positions. The frontend owns its sources: nothing is
//! registered globally, and every span refers to one [`Source`].

/// A byte range in a source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
  pub start: usize,
  pub end: usize,
}

impl Span {
  pub fn new(start: usize, end: usize) -> Span {
    Span { start, end }
  }

  /// The smallest span covering both.
  pub fn to(self, other: Span) -> Span {
    Span::new(self.start.min(other.start), self.end.max(other.end))
  }
}

/// The program's source text: the file it was loaded from, and every file
/// it includes (`@include`), in one text with one offset space, so a span
/// alone says which file it is in.
pub struct Source {
  /// The first file's name.
  pub name: String,
  /// Every file's text, in the order they were added, each starting on a
  /// line of its own.
  pub text: String,
  line_starts: Vec<usize>,
  files: Vec<File>,
}

/// One file's part of the text.
struct File {
  name: String,
  start: usize,
  /// The index in `line_starts` of its first line.
  first_line: usize,
  /// Each non-ASCII character's byte offset in the file, with the bytes
  /// beyond one per character up to and including it, so a column is found
  /// without counting the line's characters. Empty for ASCII text.
  wide: Vec<(usize, usize)>,
}

impl Source {
  pub fn new(name: impl Into<String>, text: impl Into<String>) -> Source {
    let name = name.into();
    let mut source = Source {
      name: name.clone(),
      text: String::new(),
      line_starts: vec![0],
      files: Vec::new(),
    };
    source.add(name, &text.into());
    source
  }

  /// Appends another file's text, starting on a line of its own; returns
  /// its range in [`Source::text`].
  pub fn add(&mut self, name: impl Into<String>, text: &str) -> Span {
    if !self.text.is_empty() && !self.text.ends_with('\n') {
      self.text.push('\n');
      self.line_starts.push(self.text.len());
    }
    let start = self.text.len();
    let mut wide = Vec::new();
    if !text.is_ascii() {
      let mut extra = 0;
      for (i, c) in text.char_indices().filter(|(_, c)| !c.is_ascii()) {
        extra += c.len_utf8() - 1;
        wide.push((i, extra));
      }
    }
    self.files.push(File {
      name: name.into(),
      start,
      first_line: self.line_starts.len() - 1,
      wide,
    });
    self.text.push_str(text);
    self
      .line_starts
      .extend(text.match_indices('\n').map(|(i, _)| start + i + 1));
    Span::new(start, self.text.len())
  }

  fn file(&self, offset: usize) -> &File {
    let i = self.files.partition_point(|f| f.start <= offset);
    &self.files[i.saturating_sub(1)]
  }

  /// The name of the file holding `offset`.
  pub fn file_name(&self, offset: usize) -> &str {
    &self.file(offset).name
  }

  /// 1-based line and column (in characters) of a byte offset, within the
  /// file holding it.
  pub fn line_col(&self, offset: usize) -> (u32, u32) {
    let offset = offset.min(self.text.len());
    let line = match self.line_starts.binary_search(&offset) {
      Ok(i) => i,
      Err(i) => i - 1,
    };
    let start = self.line_starts[line];
    // A line is within one file.
    let file = self.file(offset);
    // Bytes beyond one per character before `offset` in the file.
    let extra = |offset: usize| match file
      .wide
      .partition_point(|&(at, _)| at < offset - file.start)
    {
      0 => 0,
      i => file.wide[i - 1].1,
    };
    let column = offset - start - (extra(offset) - extra(start)) + 1;
    ((line - file.first_line) as u32 + 1, column as u32)
  }

  /// `line:column`, for messages that point at another position.
  pub fn position(&self, offset: usize) -> String {
    let (line, column) = self.line_col(offset);
    format!("{line}:{column}")
  }

  /// Line `line` (1-based) of the file named `file`, without its newline.
  pub fn line(&self, file: &str, line: u32) -> Option<&str> {
    let index = self.files.iter().position(|f| f.name == file)?;
    let first = self.files[index].first_line + line.checked_sub(1)? as usize;
    let end = self
      .files
      .get(index + 1)
      .map_or(self.text.len(), |next| next.start);
    let start = *self.line_starts.get(first)?;
    if start > end || (start == end && start != self.files[index].start) {
      return None;
    }
    let stop = self.text[start..end].find('\n').map_or(end, |i| start + i);
    Some(&self.text[start..stop])
  }

  pub fn slice(&self, span: Span) -> &str {
    &self.text[span.start..span.end]
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn line_and_column_are_one_based_and_count_characters() {
    let s = Source::new("t", "ab\n\u{e9}cd\n");
    assert_eq!(s.line_col(0), (1, 1));
    assert_eq!(s.line_col(3), (2, 1));
    // é is two bytes, one column.
    assert_eq!(s.line_col(5), (2, 2));
    assert_eq!(s.line_col(s.text.len()), (3, 1));
    // Wider characters, on lines after others and in an added file.
    let mut s = Source::new("t", "\u{e9}\u{1f9a6}a\u{4e2d}b\n\u{e9}x");
    let lib = s.add("lib", "\u{4e2d}y");
    for (offset, c) in s.text.char_indices() {
      let line_start = s.text[..offset].rfind('\n').map_or(0, |i| i + 1);
      let column = s.text[line_start..offset].chars().count() as u32 + 1;
      assert_eq!(s.line_col(offset).1, column, "{c:?} at {offset}");
    }
    assert_eq!(s.line_col(lib.start + 3), (1, 2));
  }

  #[test]
  fn an_added_file_has_its_own_lines_and_name() {
    let mut s = Source::new("main", "a\nb");
    let lib = s.add("lib", "cd\nef\n");
    assert_eq!(s.slice(lib), "cd\nef\n");
    assert_eq!((s.file_name(0), s.line_col(2)), ("main", (2, 1)));
    assert_eq!(
      (s.file_name(lib.start), s.line_col(lib.start)),
      ("lib", (1, 1))
    );
    assert_eq!(s.line_col(lib.start + 4), (2, 2));
    // An empty first file.
    let mut s = Source::new("main", "");
    let lib = s.add("lib", "x");
    assert_eq!(
      (lib.start, s.file_name(0), s.line_col(0)),
      (0, "lib", (1, 1))
    );
  }
}
