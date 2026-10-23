use crate::Error;

/// A worksheet discovered through the package's relationships.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Sheet {
    pub(super) name: String,
    pub(super) path: String,
    pub(super) visibility: SheetVisibility,
}

impl Sheet {
    /// The decoded worksheet name used by the workbook's read and edit methods.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The worksheet part's resolved path within the ZIP archive.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
    /// Whether Excel shows the worksheet's tab, from the sheet's `state` in
    /// the workbook. Hidden worksheets are read and edited like visible ones.
    #[must_use]
    pub fn visibility(&self) -> SheetVisibility {
        self.visibility
    }

    // Names this worksheet in an error raised while reading, preparing, or
    // patching it, retaining the error's variant. Every worksheet error passes
    // through here, which is what gives a protected-cell refusal its sheet.
    pub(super) fn context(&self, error: Error) -> Error {
        match error {
            Error::ProtectedCell { cell, reason, .. } => Error::ProtectedCell {
                sheet: self.name.clone(),
                cell,
                reason,
            },
            error => error.context(format_args!("worksheet {:?}", self.name)),
        }
    }
}

/// Whether Excel shows a worksheet's tab.
///
/// Additional states may be added in compatible releases. Callers should
/// include a fallback arm when matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SheetVisibility {
    /// The tab is shown. A sheet without a `state` attribute is visible.
    Visible,
    /// The tab is hidden, and Excel's Unhide command can show it again
    /// (`state="hidden"`).
    Hidden,
    /// The tab is hidden and not offered by Excel's Unhide command; only
    /// macros or property editors can show it again (`state="veryHidden"`).
    VeryHidden,
}

impl SheetVisibility {
    /// The state's SpreadsheetML spelling: `"visible"`, `"hidden"`, or
    /// `"veryHidden"`, as in the CLI's `list --json` output.
    ///
    /// ```
    /// use sheetpatch::SheetVisibility;
    ///
    /// assert_eq!(SheetVisibility::VeryHidden.as_str(), "veryHidden");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
            Self::VeryHidden => "veryHidden",
        }
    }
}
