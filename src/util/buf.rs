// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::{Result, Write, Read};
use std::fmt::{self, Debug, Formatter};
use alloc::raw_vec::RawVec;
use std::{cmp, ptr, slice, mem};

use bytes::{ByteBuf, MutByteBuf, alloc};
pub use mio::{TryRead, TryWrite};

use util::escape;

// `create_mem_buf` creates the buffer with fixed capacity s.
pub fn create_mem_buf(s: usize) -> MutByteBuf {
    unsafe {
        ByteBuf::from_mem_ref(alloc::heap(s.next_power_of_two()), s as u32, 0, s as u32).flip()
    }
}

/// `PipeBuffer` is useful when you want to move data from `Write` to a `Read` or vice versa.
pub struct PipeBuffer {
    // the index of the first byte of written data.
    start: usize,
    // the index of buf that new data should be written in.
    end: usize,
    buf: RawVec<u8>,
}

impl PipeBuffer {
    pub fn new(capacity: usize) -> PipeBuffer {
        PipeBuffer {
            start: 0,
            end: 0,
            // one extra byte to indicate if buf is full or empty.
            buf: RawVec::with_capacity(capacity + 1),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        if self.end >= self.start {
            self.end - self.start
        } else {
            self.buf.cap() - self.start + self.end
        }
    }

    #[inline]
    unsafe fn buf_as_slice(&self) -> &[u8] {
        slice::from_raw_parts(self.buf.ptr(), self.buf.cap())
    }

    #[inline]
    unsafe fn buf_as_slice_mut(&self) -> &mut [u8] {
        slice::from_raw_parts_mut(self.buf.ptr(), self.buf.cap())
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.buf.cap() - 1
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the written buf.
    fn slice(&self) -> (&[u8], &[u8]) {
        unsafe {
            let buf = self.buf_as_slice();
            if self.end >= self.start {
                (&buf[self.start..self.end], &[])
            } else {
                (&buf[self.start..], &buf[..self.end])
            }
        }
    }

    /// Get the not written buf.
    fn slice_append(&mut self) -> (&mut [u8], &mut [u8]) {
        if self.is_full() {
            return (&mut [], &mut []);
        }
        unsafe {
            let start = self.start;
            let end = self.end;
            let cap = self.capacity();
            let buf = self.buf_as_slice_mut();

            if start == 0 {
                (&mut buf[end..cap], &mut [])
            } else if start <= end {
                let (right, left) = buf.split_at_mut(end);
                let (right, _) = right.split_at_mut(start - 1);
                (left, right)
            } else {
                (&mut buf[end..start - 1], &mut [])
            }
        }
    }

    /// Ensure the capacity of inner buf not less than `capacity`.
    ///
    /// If capacity is larger than inner buf, a larger buffer will be reallocated.
    /// Allocated buffer's capacity doesn't have to be equal to specified value.
    pub fn ensure(&mut self, capacity: usize) {
        if capacity <= self.capacity() {
            return;
        }

        let cap = self.buf.cap();
        self.buf.reserve(cap, capacity + 1 - cap);
        let new_cap = self.buf.cap();

        unsafe {
            if self.start <= self.end {
                // written data are linear, no need to move.
                return;
            } else if new_cap - cap > self.end && self.end <= cap - self.start {
                // left part can be fit in new buf and is shorter.
                let left = self.buf.ptr();
                let new_pos = self.buf.ptr().offset(cap as isize);
                ptr::copy_nonoverlapping(left, new_pos, self.end);
                self.end += cap;
            } else {
                let right = self.buf.ptr().offset(self.start as isize);
                self.start = new_cap - (cap - self.start);
                let new_pos = self.buf.ptr().offset(self.start as isize);
                if self.start >= cap {
                    ptr::copy_nonoverlapping(right, new_pos, new_cap - self.start);
                } else {
                    ptr::copy(right, new_pos, new_cap - self.start);
                }
            }
        }
    }

    /// shrink the inner buf to specified capacity.
    pub fn shrink_to(&mut self, len: usize) {
        assert!(len < self.capacity());

        let new_cap = len + 1;
        let to_keep = cmp::min(len, self.len());
        unsafe {
            if self.start <= self.end {
                let dest = self.buf.ptr();
                if self.start >= len {
                    self.start = 0;
                    self.end = to_keep;
                    let source = self.buf.ptr().offset(self.start as isize);
                    ptr::copy_nonoverlapping(source, dest, to_keep);
                } else {
                    let right_len = new_cap - self.start;
                    if right_len >= to_keep {
                        self.end = self.start + to_keep;
                    } else {
                        self.end = to_keep - right_len;
                        let source = self.buf.ptr().offset(new_cap as isize);
                        ptr::copy_nonoverlapping(source, dest, self.end);
                    }
                }
            } else {
                let source = self.buf.ptr().offset(self.start as isize);
                let right_len = self.buf.cap() - self.start;
                if right_len >= to_keep {
                    self.start = 0;
                    self.end = to_keep;
                    ptr::copy(source, self.buf.ptr(), to_keep);
                } else {
                    self.start = new_cap - right_len;
                    if self.end >= self.start {
                        self.end = self.start - 1;
                    }
                    ptr::copy(source,
                              self.buf.ptr().offset(self.start as isize),
                              right_len);
                }
            }
        }
        self.buf.shrink_to_fit(new_cap);
    }

    /// Read data from `r` and fill inner buf.
    ///
    /// Please note that the buffer size will not change automatically,
    /// you have to call capacity-related method to adjust it.
    pub fn read_from<R: Read>(&mut self, r: &mut R) -> Result<usize> {
        let mut end = self.end;
        let mut readed;
        {
            let (left, right) = self.slice_append();
            match try!(r.try_read(left)) {
                None => return Ok(0),
                Some(l) => readed = l,
            }
            end += readed;
            if readed == left.len() && !right.is_empty() {
                // Can't return error because r has been read into left.
                if let Ok(Some(l)) = r.try_read(right) {
                    end = l;
                    readed += l;
                }
            }
        }
        self.end = end;
        Ok(readed)
    }

    /// Write the inner buffer to `w`.
    pub fn write_to<W: Write>(&mut self, w: &mut W) -> Result<usize> {
        let mut start = self.start;
        let mut written;
        {
            let (left, right) = self.slice();
            match try!(w.try_write(left)) {
                None => return Ok(0),
                Some(l) => written = l,
            }
            start += written;
            if written == left.len() && !right.is_empty() {
                // Can't return error because left has written into w.
                if let Ok(Some(l)) = w.try_write(right) {
                    start = l;
                    written += l;
                }
            }
        }
        self.start = start;
        Ok(written)
    }
}

impl Read for PipeBuffer {
    fn read(&mut self, mut buf: &mut [u8]) -> Result<usize> {
        self.write_to(&mut buf)
    }
}

impl Write for PipeBuffer {
    fn write(&mut self, mut buf: &[u8]) -> Result<usize> {
        let min_cap = self.len() + buf.len();
        self.ensure(min_cap);
        self.read_from(&mut buf)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl PartialEq for PipeBuffer {
    fn eq(&self, right: &PipeBuffer) -> bool {
        if self.len() != right.len() {
            return false;
        }

        let (mut l1, mut r1) = self.slice();
        let (mut l2, mut r2) = right.slice();
        if l1.len() > l2.len() {
            mem::swap(&mut l1, &mut l2);
            mem::swap(&mut r1, &mut r2);
        }
        l1 == &l2[..l1.len()] && &r1[..l2.len() - l1.len()] == &l2[l1.len()..] &&
        &r1[l2.len() - l1.len()..] == r2
    }
}

impl<'a> PartialEq<&'a [u8]> for PipeBuffer {
    fn eq(&self, right: &&'a [u8]) -> bool {
        if self.len() != right.len() {
            return false;
        }

        let (l, r) = self.slice();
        l == &right[..l.len()] && r == &right[l.len()..]
    }
}

impl Debug for PipeBuffer {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f,
               "PipeBuffer [start: {}, end: {}, buf: {}]",
               self.start,
               self.end,
               escape(unsafe { self.buf_as_slice() }))
    }
}

#[cfg(test)]
mod tests {
    use std::io::*;
    use super::*;

    #[test]
    fn test_read_from() {
        let mut s = PipeBuffer::new(25);

        let cap = s.capacity();
        let padding = vec![0; cap];
        for len in 0..cap {
            let expected = vec![len as u8; len];

            for pos in 0..cap + 1 {
                for l in 0..len {
                    s.start = pos;
                    s.end = pos;

                    let mut input = &expected[0..l];
                    assert_eq!(l, s.read_from(&mut input).unwrap());
                    assert_eq!(s, &expected[0..l]);
                    input = &expected[l..];
                    assert_eq!(len - l, s.read_from(&mut input).unwrap());
                    assert_eq!(s, expected.as_slice());

                    input = padding.as_slice();
                    assert_eq!(cap - len, s.read_from(&mut input).unwrap());
                    let mut exp = expected.clone();
                    exp.extend_from_slice(&padding[..cap - len]);
                    assert_eq!(s, exp.as_slice());
                }
            }
        }
    }

    #[test]
    fn test_write_to() {
        let mut s = PipeBuffer::new(25);

        let cap = s.capacity();
        for len in 0..cap {
            let expected = vec![len as u8; len];

            for pos in 0..cap + 1 {
                for l in 0..len {
                    s.start = pos;
                    s.end = pos;

                    let mut input = expected.as_slice();
                    assert_eq!(len, s.read_from(&mut input).unwrap());

                    let mut w = vec![0; l];
                    {
                        let mut buf = w.as_mut_slice();
                        assert_eq!(l, s.write_to(&mut buf).unwrap());
                    }
                    assert_eq!(w, &expected[..l]);
                    assert_eq!(s, &expected[l..]);

                    let mut w = vec![0; cap];
                    assert_eq!(len - l, s.read(&mut w).unwrap());
                    assert_eq!(&w[..len - l], &expected[l..]);
                }
            }
        }
    }

    #[test]
    fn test_shrink_to() {
        let cap = 25;
        for l in 0..cap {
            let expect = vec![l as u8; l];

            for pos in 0..cap + 1 {
                for shrink in 0..cap {
                    let mut s = PipeBuffer::new(cap);
                    s.start = pos;
                    s.end = pos;

                    let mut input = expect.as_slice();
                    assert_eq!(l, s.read_from(&mut input).unwrap());
                    s.shrink_to(shrink);

                    assert_eq!(shrink, s.capacity());
                    if shrink > l {
                        assert_eq!(s, expect.as_slice());
                    } else {
                        assert_eq!(s, &expect[..shrink]);
                    }
                }
            }
        }
    }

    #[test]
    fn test_ensure() {
        let cap = 25;
        for l in 0..cap {
            let expect = vec![l as u8; l];

            for pos in 0..cap + 1 {
                for init in 0..cap {
                    let mut s = PipeBuffer::new(cap);
                    s.start = pos;
                    s.end = pos;

                    let example = vec![init as u8; init];
                    let mut input = example.as_slice();
                    assert_eq!(init, s.read_from(&mut input).unwrap());
                    assert_eq!(s, example.as_slice());
                    s.ensure(init + l);
                    assert_eq!(s, example.as_slice());
                    input = expect.as_slice();

                    assert_eq!(l, s.read_from(&mut input).unwrap());
                    let mut exp = example.clone();
                    exp.extend_from_slice(&expect);
                    assert_eq!(s, exp.as_slice());
                }
            }
        }
    }
}
