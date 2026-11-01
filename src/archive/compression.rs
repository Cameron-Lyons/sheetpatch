//! Deterministic segmented Deflate compression and bounded stream inspection.
use super::records::{Sizes, zip32};
use super::{MAX_PART_SIZE, over_read_limit, unsupported};
use crate::Result;
use flate2::write::DeflateEncoder;
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use std::io::{self, Write};
use std::num::{NonZero, NonZeroUsize};
use std::panic;
use std::thread::{self, Scope, ScopedJoinHandle};

// Level 4 is the fastest level that keeps lazy matching. For worksheet XML it
// produces archives about as small as the default level 6, several times faster.
pub(super) const DEFLATE_LEVEL: u32 = 4;
/// Replacements longer than two segments are deflated in segments of this
/// length, several at once. Each segment starts without the previous one's
/// history, which costs about 0.1% in size.
pub(super) const DEFLATE_SEGMENT: usize = 1 << 20;
pub(super) const MAX_DEFLATE_THREADS: usize = 8;

fn deflate_segment_len() -> usize {
    #[cfg(test)]
    if let Some(len) = super::tests::SEGMENT_OVERRIDE.get() {
        return len;
    }
    DEFLATE_SEGMENT
}

/// Deflate `data` as one raw stream, returned in pieces to write in order.
/// Data longer than two segments is split across at most `max_threads`, or
/// `MAX_DEFLATE_THREADS` by default, including the caller. The bytes do not
/// depend on how many threads run.
pub(super) fn deflate(data: &[u8], max_threads: Option<NonZeroUsize>) -> Result<Vec<Vec<u8>>> {
    let segment = deflate_segment_len();
    if data.len() <= 2 * segment {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(DEFLATE_LEVEL));
        encoder.write_all(data)?;
        return Ok(vec![encoder.finish()?]);
    }
    let threads = thread::available_parallelism()
        .map_or(1, NonZero::get)
        .min(max_threads.map_or(MAX_DEFLATE_THREADS, NonZeroUsize::get))
        .min(data.len().div_ceil(segment));
    deflate_segments(data, segment, threads)
}

/// Deflate nonempty `data` in segments of `segment` bytes, compressed one by
/// one in contiguous groups on `threads` threads, the calling thread among
/// them. A group whose thread cannot be started is compressed by the calling
/// thread instead.
pub(super) fn deflate_segments(
    data: &[u8],
    segment: usize,
    threads: usize,
) -> Result<Vec<Vec<u8>>> {
    let segments: Vec<_> = data.chunks(segment).collect();
    let last = segments.len() - 1;
    let compress = |first: usize, group: &[&[u8]]| -> Result<Vec<Vec<u8>>> {
        group
            .iter()
            .enumerate()
            .map(|(index, segment)| deflate_segment(segment, first + index == last))
            .collect()
    };
    let per_thread = segments.len().div_ceil(threads);
    let mut groups = segments
        .chunks(per_thread)
        .enumerate()
        .map(|(index, group)| (index * per_thread, group));
    let (first, group) = groups.next().expect("data is nonempty");
    thread::scope(|scope| {
        // Start every other group's thread before compressing this group.
        let mut others = Vec::new();
        for (first, group) in groups {
            let thread = spawn_scoped(scope, move || compress(first, group));
            others.push((first, group, thread));
        }
        let mut output = compress(first, group)?;
        for (first, group, thread) in others {
            output.extend(match thread {
                Ok(thread) => thread
                    .join()
                    .unwrap_or_else(|payload| panic::resume_unwind(payload))?,
                Err(_) => compress(first, group)?,
            });
        }
        Ok(output)
    })
}

/// Start a thread in `scope`, which fails like any thread start, for example
/// under a limit on the user's processes.
fn spawn_scoped<'scope, T: Send + 'scope>(
    scope: &'scope Scope<'scope, '_>,
    run: impl FnOnce() -> T + Send + 'scope,
) -> io::Result<ScopedJoinHandle<'scope, T>> {
    #[cfg(test)]
    super::tests::SPAWNS.set(super::tests::SPAWNS.get() + 1);
    #[cfg(test)]
    if super::tests::SPAWN_FAILS.get() {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    thread::Builder::new().spawn_scoped(scope, run)
}

/// Deflate one segment of a stream. Every segment but the last ends with a
/// sync flush, which byte-aligns it without ending the stream, so the next
/// segment's blocks can follow it.
fn deflate_segment(data: &[u8], last: bool) -> Result<Vec<u8>> {
    let mut compressor = Compress::new(
        Compression::new(DEFLATE_LEVEL),
        /* zlib_header: */ false,
    );
    let flush = if last {
        FlushCompress::Finish
    } else {
        FlushCompress::Sync
    };
    let mut output = Vec::with_capacity(data.len() / 2 + 64);
    loop {
        let consumed = compressor.total_in() as usize;
        let status = compressor
            .compress_vec(&data[consumed..], &mut output, flush)
            .map_err(io::Error::other)?;
        // A flush is complete once a call consumed all input and left room
        // in the output; until then the compressor may hold pending bytes.
        // Stopping early would silently truncate the stream, which saves
        // never read back.
        let done = if last {
            status == Status::StreamEnd
        } else {
            compressor.total_in() as usize == data.len() && output.len() < output.capacity()
        };
        if done {
            return Ok(output);
        }
        output.reserve(output.capacity());
    }
}

pub(super) fn inspect_deflate(compressed: &[u8], name: &str) -> Result<Sizes> {
    let mut decoder = Decompress::new(/* zlib_header: */ false);
    let mut buffer = [0; 8192];
    let mut crc = crc32fast::Hasher::new();
    loop {
        let previous_in = decoder.total_in();
        let previous_out = decoder.total_out();
        let status = decoder
            .decompress(
                &compressed[previous_in as usize..],
                &mut buffer,
                FlushDecompress::None,
            )
            .map_err(|error| {
                unsupported(format!(
                    "cannot compact invalid obsolete deflate stream in {name}: {error}"
                ))
            })?;
        if decoder.total_out() > MAX_PART_SIZE as u64 {
            return Err(over_read_limit(format_args!("obsolete ZIP member {name}")));
        }
        let produced = (decoder.total_out() - previous_out) as usize;
        crc.update(&buffer[..produced]);
        if status == Status::StreamEnd {
            let compressed_size = zip32(decoder.total_in() as usize, "compressed ZIP member size")?;
            return Ok(Sizes {
                crc: crc.finalize(),
                compressed_size,
                size: decoder.total_out() as u32,
            });
        }
        if decoder.total_in() == previous_in && decoder.total_out() == previous_out {
            return Err(unsupported(format!(
                "cannot compact incomplete obsolete deflate stream in {name}"
            )));
        }
    }
}
