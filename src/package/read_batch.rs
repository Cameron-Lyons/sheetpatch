use crate::{CellRef, Error, Result};
use std::io::{self, ErrorKind};

pub(super) fn parse_addresses<S: AsRef<str>>(
    addresses: impl IntoIterator<Item = S>,
) -> Result<Vec<CellRef>> {
    addresses
        .into_iter()
        .map(|address| address.as_ref().parse())
        .collect()
}

// The most cells one typed batch read returns: sixteen whole columns, and
// sixteen times the CLI's `get` limit. A worksheet part is read only up to
// 64 MiB, and each cell element needs an explicit reference of its own, so
// every cell of a worksheet as read fits. The addresses and results of a full
// read take a little over 1 GiB besides their text, where a range over the
// whole grid would need more than a terabyte.
const MAX_READ_CELLS: usize = 1 << 24;

// Collects typed addresses for a batch read, refusing a request over the limit
// before reading any cell. A range's exact size hint is checked before any
// address is stored, and other iterators are cut off at the limit.
pub(super) fn collect_cells(cells: impl IntoIterator<Item = CellRef>) -> Result<Vec<CellRef>> {
    let too_many = || {
        Error::Unsupported(format!(
            "too many cells: one read returns at most {MAX_READ_CELLS}"
        ))
    };
    let cells = cells.into_iter();
    let (expected, _) = cells.size_hint();
    if expected > MAX_READ_CELLS {
        return Err(too_many());
    }
    let mut collected = Vec::new();
    collected
        .try_reserve_exact(expected)
        .map_err(|_| io::Error::from(ErrorKind::OutOfMemory))?;
    for cell in cells {
        if collected.len() == MAX_READ_CELLS {
            return Err(too_many());
        }
        collected.push(cell);
    }
    Ok(collected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CellRange;

    #[test]
    fn batch_reads_collect_exactly_sixteen_whole_columns() {
        let too_many = |result: Result<Vec<CellRef>>| matches!(result, Err(Error::Unsupported(message)) if message.contains("too many cells"));
        let columns: CellRange = "A1:P1048576".parse().unwrap();
        assert_eq!(columns.cell_count(), MAX_READ_CELLS as u64);
        // With an exact size hint, and with none, whose cells are counted.
        assert_eq!(collect_cells(columns).unwrap().len(), MAX_READ_CELLS);
        let unsized_cells = columns.cells().filter(|_| true);
        assert_eq!(collect_cells(unsized_cells).unwrap().len(), MAX_READ_CELLS);
        let one_more = || columns.cells().chain([CellRef::try_new(1, 17).unwrap()]);
        assert!(too_many(collect_cells(one_more())));
        assert!(too_many(collect_cells(one_more().filter(|_| true))));
    }
}
