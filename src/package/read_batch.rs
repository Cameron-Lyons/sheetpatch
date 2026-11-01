use crate::{CellRef, Error, Result};
use std::io::{self, ErrorKind};

pub(super) fn parse_addresses<S: AsRef<str>>(
    addresses: impl IntoIterator<Item = S>,
) -> Result<Vec<CellRef>> {
    collect_results(
        addresses
            .into_iter()
            .map(|address| address.as_ref().parse()),
        MAX_READ_CELLS,
    )
}

// The most cells one batch read returns: sixteen whole columns, and
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
    collect_results(cells.into_iter().map(Ok), MAX_READ_CELLS)
}

// Both APIs have the same bound. Every capacity increase is fallible, including
// iterators without a useful lower size hint. `limit` also lets tests exercise
// the streaming boundary without allocating millions of addresses.
fn collect_results(
    cells: impl Iterator<Item = Result<CellRef>>,
    limit: usize,
) -> Result<Vec<CellRef>> {
    let too_many =
        || Error::Unsupported(format!("too many cells: one read returns at most {limit}"));
    let (expected, _) = cells.size_hint();
    if expected > limit {
        return Err(too_many());
    }
    let mut collected = Vec::new();
    collected
        .try_reserve_exact(expected)
        .map_err(|_| io::Error::from(ErrorKind::OutOfMemory))?;
    for cell in cells {
        if collected.len() == limit {
            return Err(too_many());
        }
        let cell = cell?;
        if collected.len() == collected.capacity() {
            collected
                .try_reserve(1)
                .map_err(|_| io::Error::from(ErrorKind::OutOfMemory))?;
        }
        collected.push(cell);
    }
    Ok(collected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CellRange;
    use std::iter;

    #[test]
    fn string_addresses_share_the_typed_limit_before_consuming_input() {
        let oversized = iter::repeat_n("A1", MAX_READ_CELLS + 1)
            .inspect(|_| panic!("an oversized exact iterator must be rejected before consumption"));
        assert!(matches!(
            parse_addresses(oversized),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn streaming_batches_preserve_order_errors_and_limit_consumption() {
        let addresses = ["B2", "A1", "B2"];
        let cells = || {
            addresses
                .iter()
                .map(|address| address.parse())
                .filter(|_| true)
        };
        assert_eq!(
            collect_results(cells(), 3).unwrap(),
            addresses.map(|address| address.parse::<CellRef>().unwrap())
        );
        assert!(matches!(
            collect_results(cells(), 2),
            Err(Error::Unsupported(_))
        ));
        let invalid = ["A1", "invalid", "B2"].into_iter().map(str::parse);
        assert!(matches!(
            collect_results(invalid, 3),
            Err(Error::InvalidCellReference(_))
        ));
        let mut consumed = 0;
        let endless = iter::repeat_with(|| {
            consumed += 1;
            "A1".parse()
        })
        .filter(|_| true);
        assert!(matches!(
            collect_results(endless, 3),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(consumed, 4);
    }

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
