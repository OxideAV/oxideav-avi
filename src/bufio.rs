//! Position-tracking buffered I/O for the chunk walk and the chunk
//! writer.
//!
//! An AVI is a sequence of small records: an 8-byte chunk header, a
//! body, a pad byte; an index entry is 8 or 16 bytes. Read or written
//! straight through a `std::fs::File`, every one of them — and every
//! "where am I" query in between — is a system call: three to four per
//! packet. Both wrappers below keep the logical position themselves (a
//! position query costs nothing, a seek is deferred until the next
//! transfer actually needs the device somewhere else) and move small
//! records in blocks of up to 64 KiB.
//!
//! What goes through a block is copied twice, so only small transfers
//! do: a body of [`DIRECT`] bytes or more is read into (or written
//! from) the caller's own memory in one device call. The reader's
//! block grows from [`AHEAD_MIN`] while the reads stay sequential and
//! starts over after a jump or a direct transfer, so a header probe in
//! front of a large frame — the `ix##` scan at `open()` hops from
//! header to header — fetches a few kilobytes, not a whole block.

use std::io::{self, Read, Seek, SeekFrom, Write};

use oxideav_core::{ReadSeek, WriteSeek};

/// Largest block of either wrapper.
const BLOCK: usize = 64 * 1024;
/// Transfers of this size or more bypass the block.
const DIRECT: usize = 8 * 1024;
/// The reader's read-ahead after a jump; doubles per sequential refill.
const AHEAD_MIN: usize = 4 * 1024;
/// A forward gap up to this size is read through instead of sought
/// over: about a page, which a seek-and-read would touch anyway.
const HOP_MAX: u64 = 4 * 1024;

/// Buffered reader over a `ReadSeek` with a logical position.
pub(crate) struct BufferedInput {
    inner: Box<dyn ReadSeek>,
    /// Where the device actually is.
    device_pos: u64,
    /// Where the next `read` takes its bytes from.
    pos: u64,
    block: Box<[u8]>,
    /// File offset of `block[0]`; `block[..filled]` is valid.
    block_start: u64,
    filled: usize,
    /// How much the next refill asks the device for.
    ahead: usize,
}

impl BufferedInput {
    pub(crate) fn new(mut inner: Box<dyn ReadSeek>) -> io::Result<Self> {
        let at = inner.stream_position()?;
        Ok(Self {
            inner,
            device_pos: at,
            pos: at,
            block: vec![0u8; BLOCK].into_boxed_slice(),
            block_start: at,
            filled: 0,
            ahead: AHEAD_MIN,
        })
    }

    /// Move the device to the logical position if it is elsewhere.
    fn settle(&mut self) -> io::Result<()> {
        if self.device_pos != self.pos {
            self.device_pos = self.inner.seek(SeekFrom::Start(self.pos))?;
            if self.device_pos != self.pos {
                return Err(io::Error::other("seek landed off target"));
            }
        }
        Ok(())
    }

    /// One device read at the device position, retried on interrupt.
    fn device_read(inner: &mut dyn ReadSeek, into: &mut [u8]) -> io::Result<usize> {
        loop {
            match inner.read(into) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }
}

impl Read for BufferedInput {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        // Served from the block when the position lies inside it.
        if let Some(skip) = self.pos.checked_sub(self.block_start)
            && skip < self.filled as u64
        {
            let from = skip as usize;
            let n = out.len().min(self.filled - from);
            out[..n].copy_from_slice(&self.block[from..from + n]);
            self.pos += n as u64;
            return Ok(n);
        }
        if out.len() >= DIRECT {
            // Large transfer: straight into the caller's memory.
            self.settle()?;
            let n = Self::device_read(&mut *self.inner, out)?;
            self.device_pos += n as u64;
            self.pos += n as u64;
            self.ahead = AHEAD_MIN;
            return Ok(n);
        }
        // A short hop ahead of the device (a skipped body, a pad byte)
        // is read through rather than sought over: the next block starts
        // where the device already is, and the reads count as sequential.
        let hop = match self.pos.checked_sub(self.device_pos) {
            Some(h) if h <= HOP_MAX => {
                self.ahead = (self.ahead * 2).min(BLOCK);
                h as usize
            }
            _ => {
                self.settle()?;
                self.ahead = AHEAD_MIN;
                0
            }
        };
        let want = (hop + self.ahead).min(BLOCK);
        self.filled = 0;
        self.block_start = self.device_pos;
        let mut got = Self::device_read(&mut *self.inner, &mut self.block[..want])?;
        self.device_pos += got as u64;
        self.filled = got;
        if hop > 0 && (1..=hop).contains(&got) {
            // The device handed back less than the hop without being at
            // its end: go to the position proper.
            self.settle()?;
            self.block_start = self.pos;
            got = Self::device_read(&mut *self.inner, &mut self.block[..self.ahead])?;
            self.device_pos += got as u64;
            self.filled = got;
        }
        let from = (self.pos - self.block_start) as usize;
        let n = out.len().min(self.filled.saturating_sub(from));
        out[..n].copy_from_slice(&self.block[from..from + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for BufferedInput {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => {
                // Only the device knows its length.
                self.device_pos = self.inner.seek(SeekFrom::End(d))?;
                Some(self.device_pos)
            }
        };
        self.pos = target.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before start of file")
        })?;
        Ok(self.pos)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.pos)
    }
}

/// Buffered writer over a `WriteSeek` with a logical position.
///
/// Bytes are held until a block is full, a seek moves elsewhere, or
/// [`Write::flush`] is called; the muxer flushes at the end of
/// `write_header` and `write_trailer`, and a dropped writer flushes
/// what it still holds (errors at that point have no one to go to).
pub(crate) struct BufferedOutput {
    inner: Box<dyn WriteSeek>,
    held: Vec<u8>,
    /// Logical position: the device position plus `held.len()`.
    pos: u64,
}

impl BufferedOutput {
    pub(crate) fn new(mut inner: Box<dyn WriteSeek>) -> io::Result<Self> {
        let pos = inner.stream_position()?;
        Ok(Self {
            inner,
            held: Vec::with_capacity(BLOCK),
            pos,
        })
    }

    fn drain(&mut self) -> io::Result<()> {
        if !self.held.is_empty() {
            let r = self.inner.write_all(&self.held);
            // Held bytes are gone either way: a failed device write is
            // reported once and not replayed at a later position.
            self.held.clear();
            r?;
        }
        Ok(())
    }
}

impl BufferedOutput {
    /// Write the held bytes followed by `data` (which is not copied).
    fn write_gathered(&mut self, data: &[u8]) -> io::Result<()> {
        let mut head = 0; // bytes of `held` written
        let mut body = 0; // bytes of `data` written
        let result = loop {
            if head == self.held.len() {
                break self.inner.write_all(&data[body..]);
            }
            let parts = [
                io::IoSlice::new(&self.held[head..]),
                io::IoSlice::new(&data[body..]),
            ];
            match self.inner.write_vectored(&parts) {
                Ok(0) => break Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    let into_head = n.min(self.held.len() - head);
                    head += into_head;
                    body += n - into_head;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break Err(e),
            }
        };
        // As in `drain`: held bytes are not replayed after a failure.
        self.held.clear();
        result
    }
}

impl Write for BufferedOutput {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.held.len() + data.len() > BLOCK {
            self.drain()?;
        }
        if data.len() >= DIRECT {
            // The held bytes (this chunk's header, typically) and the
            // body leave in one gathered device write; the body is not
            // copied.
            self.write_gathered(data)?;
        } else {
            self.held.extend_from_slice(data);
        }
        self.pos += data.len() as u64;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain()?;
        self.inner.flush()
    }
}

impl Seek for BufferedOutput {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        match to {
            SeekFrom::Start(p) if p == self.pos => return Ok(p),
            SeekFrom::Current(0) => return Ok(self.pos),
            _ => {}
        }
        self.drain()?;
        self.pos = self.inner.seek(to)?;
        Ok(self.pos)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.pos)
    }
}

impl Drop for BufferedOutput {
    fn drop(&mut self) {
        let _ = self.drain();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    /// Counts device calls; shares its bytes with the test.
    #[derive(Clone, Default)]
    struct Device {
        bytes: Arc<Mutex<Cursor<Vec<u8>>>>,
        calls: Arc<Mutex<(usize, usize, usize)>>, // reads, writes, seeks
    }

    impl Read for Device {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.calls.lock().unwrap().0 += 1;
            self.bytes.lock().unwrap().read(b)
        }
    }
    impl Write for Device {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.calls.lock().unwrap().1 += 1;
            self.bytes.lock().unwrap().write(b)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Device {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.calls.lock().unwrap().2 += 1;
            self.bytes.lock().unwrap().seek(p)
        }
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn reads_match_the_device_at_every_position_and_size() {
        let data = pattern(3 * BLOCK + 1234);
        let dev = Device::default();
        *dev.bytes.lock().unwrap() = Cursor::new(data.clone());
        let mut r = BufferedInput::new(Box::new(dev.clone())).unwrap();
        // A scripted mix of small reads, block-sized reads, relative
        // and absolute seeks (some past the end, some backwards).
        let mut pos = 0u64;
        let mut state = 12345u32;
        for step in 0..4000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let len = match state >> 29 {
                0 => DIRECT + (state as usize % 90_000),
                1 => 0,
                _ => 1 + (state as usize >> 8) % 300,
            };
            match step % 7 {
                3 => {
                    pos = (state as u64 >> 3) % (data.len() as u64 + 500);
                    assert_eq!(r.seek(SeekFrom::Start(pos)).unwrap(), pos);
                }
                5 => {
                    let d = (state as i64 >> 20) % 4000 - 2000;
                    if let Some(p) = pos.checked_add_signed(d) {
                        pos = p;
                        assert_eq!(r.seek(SeekFrom::Current(d)).unwrap(), pos);
                    } else {
                        assert!(r.seek(SeekFrom::Current(d)).is_err());
                    }
                }
                _ => {}
            }
            assert_eq!(r.stream_position().unwrap(), pos);
            let mut got = vec![0u8; len];
            let n = r.read(&mut got).unwrap();
            let avail = data.len().saturating_sub(pos as usize);
            if len > 0 && avail > 0 {
                assert!(n > 0 && n <= len.min(avail));
            } else {
                assert_eq!(n, 0);
            }
            if n > 0 {
                assert_eq!(&got[..n], &data[pos as usize..pos as usize + n]);
            }
            pos += n as u64;
        }
        assert_eq!(r.seek(SeekFrom::End(-10)).unwrap(), data.len() as u64 - 10);
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, data[data.len() - 10..]);
    }

    #[test]
    fn a_sequential_walk_of_small_records_reads_one_block_at_a_time() {
        let data = pattern(10 * BLOCK);
        let dev = Device::default();
        *dev.bytes.lock().unwrap() = Cursor::new(data.clone());
        let mut r = BufferedInput::new(Box::new(dev.clone())).unwrap();
        let mut hdr = [0u8; 8];
        let mut at = 0usize;
        while at + 40 <= data.len() {
            // Header, position query, skip a 31-byte body + pad.
            r.read_exact(&mut hdr).unwrap();
            assert_eq!(hdr, data[at..at + 8]);
            assert_eq!(r.stream_position().unwrap(), at as u64 + 8);
            r.seek(SeekFrom::Current(32)).unwrap();
            at += 40;
        }
        let (reads, _, seeks) = *dev.calls.lock().unwrap();
        // The read-ahead doubles 8, 16, 32, 64 KiB, then stays there.
        assert!(reads <= 14, "{reads} device reads for ten blocks");
        assert_eq!(seeks, 1, "only the position query at construction");
    }

    #[test]
    fn header_probes_between_large_bodies_fetch_little() {
        // 200 records of an 8-byte header and a 100 000-byte body: the
        // scan reads the headers only, the walk reads the bodies whole.
        let data = pattern(200 * 100_008);
        let dev = Device::default();
        *dev.bytes.lock().unwrap() = Cursor::new(data.clone());
        let fetched = Arc::new(Mutex::new(0usize));
        struct Meter(Device, Arc<Mutex<usize>>);
        impl Read for Meter {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                let n = self.0.read(b)?;
                *self.1.lock().unwrap() += n;
                Ok(n)
            }
        }
        impl Seek for Meter {
            fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
                self.0.seek(p)
            }
        }
        let mut r = BufferedInput::new(Box::new(Meter(dev.clone(), fetched.clone()))).unwrap();
        let mut hdr = [0u8; 8];
        for k in 0..200usize {
            r.read_exact(&mut hdr).unwrap();
            assert_eq!(hdr, data[k * 100_008..k * 100_008 + 8]);
            r.seek(SeekFrom::Current(100_000)).unwrap();
        }
        assert!(*fetched.lock().unwrap() <= 201 * AHEAD_MIN);

        r.seek(SeekFrom::Start(0)).unwrap();
        *fetched.lock().unwrap() = 0;
        let before = dev.calls.lock().unwrap().0;
        let mut body = vec![0u8; 100_000];
        for k in 0..200usize {
            r.read_exact(&mut hdr).unwrap();
            r.read_exact(&mut body).unwrap();
            assert_eq!(body, data[k * 100_008 + 8..(k + 1) * 100_008]);
        }
        // Nothing is fetched twice, and a record costs two device reads.
        assert_eq!(*fetched.lock().unwrap(), data.len());
        assert!(dev.calls.lock().unwrap().0 - before <= 2 * 200 + 1);
    }

    #[test]
    fn writes_seeks_and_patches_land_as_on_the_bare_device() {
        let script = |w: &mut dyn WriteSeek| {
            let mut state = 99u32;
            for i in 0..3000u32 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let len = if state >> 28 == 0 {
                    DIRECT + (state as usize % 99_999)
                } else {
                    (state as usize >> 8) % 200
                };
                let body: Vec<u8> = (0..len).map(|k| (k as u32 ^ i) as u8).collect();
                w.write_all(&body).unwrap();
                if i % 97 == 0 {
                    // Back-patch a size field, then return.
                    let end = w.stream_position().unwrap();
                    w.seek(SeekFrom::Start(end / 3)).unwrap();
                    w.write_all(&i.to_le_bytes()).unwrap();
                    assert_eq!(w.seek(SeekFrom::Start(end)).unwrap(), end);
                }
            }
            w.flush().unwrap();
        };
        let mut bare = Cursor::new(Vec::new());
        script(&mut bare);
        let dev = Device::default();
        let mut w = BufferedOutput::new(Box::new(dev.clone())).unwrap();
        script(&mut w);
        assert_eq!(*dev.bytes.lock().unwrap().get_ref(), *bare.get_ref());
    }

    #[test]
    fn small_writes_reach_the_device_in_blocks_and_on_drop() {
        let dev = Device::default();
        let mut w = BufferedOutput::new(Box::new(dev.clone())).unwrap();
        for i in 0..100_000u32 {
            w.write_all(&i.to_le_bytes()).unwrap();
            assert_eq!(w.stream_position().unwrap(), 4 * (u64::from(i) + 1));
        }
        let writes = dev.calls.lock().unwrap().1;
        assert!(writes <= 400_000 / BLOCK + 1, "{writes} device writes");
        drop(w);
        let bytes = dev.bytes.lock().unwrap().get_ref().clone();
        assert_eq!(bytes.len(), 400_000);
        assert_eq!(bytes[399_996..], 99_999u32.to_le_bytes());
    }
}
