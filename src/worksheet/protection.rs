//! Why each kind of protected cell refuses edits, completing a sentence that
//! starts with the cell's address.

pub(crate) const FORMULA: &str = "contains a formula";
pub(crate) const FORMULA_RANGE: &str = "belongs to a formula range";
pub(crate) const MERGED: &str = "is merged into another cell's range";
pub(crate) const TABLE_HEADER: &str = "is an Excel table header cell; \
    editing it would desynchronize the table's column names";
pub(crate) const VALUE_METADATA: &str = "has value metadata (a linked or rich data type)";
pub(crate) const CELL_METADATA: &str = "has cell metadata with unsupported edit semantics";
