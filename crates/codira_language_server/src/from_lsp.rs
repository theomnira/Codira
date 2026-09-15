//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! This modules contains several helper functions to convert from types defined
//! in the Language Server Protocol to our own datatypes.

use std::convert::TryFrom;

use codira_hir_input::{FileId, LineCol, LineIndex};
use codira_paths::AbsPathBuf;
use codira_syntax::{TextRange, TextSize};
use lsp_types::Url;

use crate::{state::LanguageServerSnapshot, FilePosition};

/// Converts the specified `uri` to an absolute path. Returns an error if the
/// url could not be converted to an absolute path.
pub(crate) fn abs_path(uri: &Url) -> anyhow::Result<AbsPathBuf> {
    uri.to_file_path()
        .ok()
        .and_then(|path| AbsPathBuf::try_from(path).ok())
        .ok_or_else(|| anyhow::anyhow!("invalid uri: {}", uri))
}

/// Returns the `codira_hir::FileId` associated with the given `Url`.
pub(crate) fn file_id(
    snapshot: &LanguageServerSnapshot,
    url: &lsp_types::Url,
) -> anyhow::Result<FileId> {
    abs_path(url).and_then(|path| {
        snapshot
            .vfs
            .read()
            .file_id(&path)
            .ok_or_else(|| anyhow::anyhow!("url does not refer to a file: {}", url))
            .map(|id| FileId(id.0))
    })
}

/// Converts the specified offset to our own `TextSize` structure
pub(crate) fn offset(line_index: &LineIndex, position: lsp_types::Position) -> TextSize {
    let line_col = LineCol {
        line: position.line,
        col_utf16: position.character,
    };
    line_index.offset(line_col)
}

/// Converts the given lsp range to a `TextRange`. This requires a `LineIndex`
/// to convert lines to offsets.
pub(crate) fn text_range(line_index: &LineIndex, range: lsp_types::Range) -> TextRange {
    let start = offset(line_index, range.start);
    let end = offset(line_index, range.end);
    TextRange::new(start, end)
}

/// Converts the specified lsp `text_document_position` to a `TextPosition`.
pub(crate) fn file_position(
    snapshot: &LanguageServerSnapshot,
    text_document_position: lsp_types::TextDocumentPositionParams,
) -> anyhow::Result<FilePosition> {
    let file_id = file_id(snapshot, &text_document_position.text_document.uri)?;
    let line_index = snapshot.analysis.file_line_index(file_id)?;
    let offset = offset(&line_index, text_document_position.position);
    Ok(FilePosition { file_id, offset })
}
