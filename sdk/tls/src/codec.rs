// SPDX-License-Identifier: GPL-2.0-only
//! Reading and writing TLS's wire encoding (RFC 8446 3): big-endian integers and length-prefixed vectors.
//!
//! A [`Reader`] never panics on a short or malformed input: every read is checked and a failure is
//! [`TlsError::DecodeError`], because every byte it reads came from the network. A [`Writer`] writes into
//! the caller's slice and fails with [`TlsError::BufferTooSmall`] rather than truncating.

use crate::TlsError;

/// A cursor over bytes received from the peer.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos == self.buf.len()
    }

    /// Refuse anything left over: a structure followed by bytes it does not account for is malformed.
    pub fn finish(&self) -> Result<(), TlsError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(TlsError::DecodeError)
        }
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], TlsError> {
        let end = self.pos.checked_add(n).ok_or(TlsError::DecodeError)?;
        let s = self.buf.get(self.pos..end).ok_or(TlsError::DecodeError)?;
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, TlsError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, TlsError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn u24(&mut self) -> Result<usize, TlsError> {
        let b = self.take(3)?;
        Ok((b[0] as usize) << 16 | (b[1] as usize) << 8 | b[2] as usize)
    }

    /// A vector with a one-byte length.
    pub fn vec8(&mut self) -> Result<&'a [u8], TlsError> {
        let n = self.u8()? as usize;
        self.take(n)
    }

    /// A vector with a two-byte length.
    pub fn vec16(&mut self) -> Result<&'a [u8], TlsError> {
        let n = self.u16()? as usize;
        self.take(n)
    }

    /// A vector with a three-byte length.
    pub fn vec24(&mut self) -> Result<&'a [u8], TlsError> {
        let n = self.u24()?;
        self.take(n)
    }
}

/// A cursor writing into the caller's buffer.
pub(crate) struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn len(&self) -> usize {
        self.pos
    }

    pub fn put(&mut self, bytes: &[u8]) -> Result<(), TlsError> {
        let end = self.pos + bytes.len();
        self.buf.get_mut(self.pos..end).ok_or(TlsError::BufferTooSmall)?.copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    pub fn u8(&mut self, v: u8) -> Result<(), TlsError> {
        self.put(&[v])
    }

    pub fn u16(&mut self, v: u16) -> Result<(), TlsError> {
        self.put(&v.to_be_bytes())
    }

    /// Reserve a length field of `width` bytes, to be filled by [`Writer::end`] once the vector is written.
    pub fn begin(&mut self, width: usize) -> Result<Mark, TlsError> {
        let at = self.pos;
        self.put(&[0u8; 3][..width])?;
        Ok(Mark { at, width })
    }

    /// Fill in a length reserved by [`Writer::begin`] with the number of bytes written since.
    pub fn end(&mut self, mark: Mark) {
        let len = self.pos - mark.at - mark.width;
        let bytes = (len as u32).to_be_bytes();
        debug_assert!(len < 1 << (8 * mark.width), "a vector outgrew its length field");
        self.buf[mark.at..mark.at + mark.width].copy_from_slice(&bytes[4 - mark.width..]);
    }
}

/// A reserved length field.
pub(crate) struct Mark {
    at: usize,
    width: usize,
}
