//! Port of `tdutils/td/utils/PathView.{h,cpp}` — the read-only path
//! splitter behind `clean_filename`, `getFileMimeType` and the Files-slice
//! name handling. The C++ slices the raw bytes between the last slash and
//! the last dot (a leading dot right after the slash does not start an
//! extension, so `.bashrc` has none); here the views are `&[u8]` subslices
//! of the owned [`PathView::path`].
//!
//! `parent_dir_noslash`, `relative` and `dir_and_file` (the PathView.cpp
//! halves) have no consumer yet and land with theirs (the Files slice).

/// C++ `PathView(Slice path)` (PathView.cpp:12-24) plus the header
/// accessors over the two cached split points.
pub struct PathView {
    path: Vec<u8>,
    last_slash: isize,
    last_dot: isize,
}

fn is_slash(c: u8) -> bool {
    c == b'/' || c == b'\\'
}

impl PathView {
    pub fn new(path: &str) -> PathView {
        let path = path.as_bytes().to_vec();
        let mut last_slash = path.len() as isize - 1;
        while last_slash >= 0 && !is_slash(path[last_slash as usize]) {
            last_slash -= 1;
        }
        let mut last_dot = path.len() as isize;
        let mut i = last_dot - 1;
        while i > last_slash + 1 {
            if path[i as usize] == b'.' {
                last_dot = i;
                break;
            }
            i -= 1;
        }
        PathView {
            path,
            last_slash,
            last_dot,
        }
    }

    /// `PathView::empty`.
    pub fn is_empty(&self) -> bool {
        self.path.is_empty()
    }

    /// `PathView::is_dir`.
    pub fn is_dir(&self) -> bool {
        self.path.last().copied().map(is_slash).unwrap_or(false)
    }

    /// `PathView::extension` — empty when the name has no extension.
    pub fn extension(&self) -> &str {
        if self.last_dot == self.path.len() as isize {
            return "";
        }
        std::str::from_utf8(&self.path[self.last_dot as usize + 1..]).unwrap_or("")
    }

    /// `PathView::without_extension`.
    pub fn without_extension(&self) -> &str {
        std::str::from_utf8(&self.path[..self.last_dot.max(0) as usize]).unwrap_or("")
    }

    /// `PathView::file_stem`.
    pub fn file_stem(&self) -> &str {
        let start = (self.last_slash + 1).max(0) as usize;
        let end = (self.last_dot - self.last_slash - 1).max(0) as usize;
        std::str::from_utf8(&self.path[start..start + end]).unwrap_or("")
    }

    /// `PathView::file_name`.
    pub fn file_name(&self) -> &str {
        std::str::from_utf8(&self.path[(self.last_slash + 1).max(0) as usize..]).unwrap_or("")
    }

    /// `PathView::path`.
    pub fn path(&self) -> &str {
        std::str::from_utf8(&self.path).unwrap_or("")
    }

    /// `PathView::is_absolute` — a leading slash or a `C:\`-style drive
    /// prefix.
    pub fn is_absolute(&self) -> bool {
        !self.is_empty()
            && (is_slash(self.path[0])
                || (self.path.len() >= 3 && self.path[1] == b':' && is_slash(self.path[2])))
    }

    /// `PathView::is_relative`.
    pub fn is_relative(&self) -> bool {
        !self.is_absolute()
    }
}

#[cfg(test)]
mod tests {
    use super::PathView;

    #[test]
    fn splits_names_and_extensions() {
        let view = PathView::new("/a/b/c/file.txt.zip");
        assert_eq!(view.file_name(), "file.txt.zip");
        assert_eq!(view.file_stem(), "file.txt");
        assert_eq!(view.extension(), "zip");
        assert_eq!(view.without_extension(), "/a/b/c/file.txt");
        assert!(!view.is_dir());
        assert!(view.is_absolute());
    }

    #[test]
    fn leading_dot_is_not_an_extension() {
        // The ctor skips the dot at last_slash + 1, like the C++.
        let view = PathView::new(".bashrc");
        assert_eq!(view.file_stem(), ".bashrc");
        assert_eq!(view.extension(), "");
        let view = PathView::new("/home/user/.bashrc");
        assert_eq!(view.file_stem(), ".bashrc");
        assert_eq!(view.extension(), "");
    }

    #[test]
    fn directories_and_relative_paths() {
        let view = PathView::new("relative/dir/");
        assert!(view.is_dir());
        assert!(view.is_relative());
        assert_eq!(view.file_name(), "");
        assert_eq!(view.extension(), "");
        let view = PathView::new("C:\\temp\\x.tar.gz");
        assert!(view.is_absolute());
        assert_eq!(view.file_stem(), "x.tar");
        assert_eq!(view.extension(), "gz");
    }

    #[test]
    fn empty_path() {
        let view = PathView::new("");
        assert!(view.is_empty());
        assert_eq!(view.file_name(), "");
        assert!(!view.is_absolute());
    }
}
