//! Fixed-capacity collections for a heapless build.
//!
//! Everything here stores its elements inline in a const-sized array, so the
//! whole program runs without an allocator. ArrayVec and ArrayString stand in
//! for Vec and String, and every write that would exceed the capacity is
//! reported instead of growing.
//!
//! Storing elements inline with no allocator means MaybeUninit, and one
//! invariant carries every unsafe block below. ArrayVec holds
//! [MaybeUninit<T>; N] of which only the first len slots are initialized: a slot
//! below len was written before len reached it, and a slot at or above len is
//! dead and never read without being written first. len drops before a vacated
//! top slot can be observed, and a bulk drop sets len to its final value before
//! dropping anything, so a panicking Drop cannot revisit a slot already dropped.
//! ArrayString keeps the parallel invariant that buf[..len] is always valid
//! UTF-8, since every writer appends or moves whole code points and checks char
//! boundaries. The SAFETY comments below name this rather than restate it.

use core::fmt;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::ptr;

/// A vector with a compile-time capacity and no heap backing.
///
/// Pushes past the capacity are reported instead of growing. Elements are kept
/// in an uninitialized array and only the first len slots are live, so T may
/// own resources (an OwnedFd, say) and is dropped correctly.
pub struct ArrayVec<T, const N: usize> {
    buf: [MaybeUninit<T>; N],
    len: usize,
}

impl<T, const N: usize> ArrayVec<T, N> {
    pub const fn new() -> Self {
        Self {
            buf: [const { MaybeUninit::uninit() }; N],
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    /// Kept alongside len, which clippy requires it to be, though the slice's
    /// own is_empty is what most callers reach through Deref.
    #[allow(dead_code)]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn is_full(&self) -> bool {
        self.len == N
    }

    pub const fn remaining(&self) -> usize {
        N - self.len
    }

    /// Append a value. Returns it back as Err when already at capacity.
    pub fn push(&mut self, value: T) -> Result<(), T> {
        if self.len == N {
            return Err(value);
        }
        // SAFETY: len < N, so buf[len] is an in-bounds, currently-dead slot.
        unsafe { self.buf.get_unchecked_mut(self.len).write(value) };
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: slot len was live before the decrement and is not read again
        // until something writes it via push.
        Some(unsafe { self.buf.get_unchecked(self.len).assume_init_read() })
    }

    /// Insert value at index, shifting later elements right. Returns it back as
    /// Err when full or index is past the end.
    pub fn insert(&mut self, index: usize, value: T) -> Result<(), T> {
        if self.len == N || index > self.len {
            return Err(value);
        }
        let p = self.buf.as_mut_ptr();
        // SAFETY: index <= len < N, so [index, len) is a live run with one free
        // slot above it; the copy shifts that run up by one, then index is
        // overwritten with value. Bytes are MaybeUninit, so no element is
        // dropped or read uninitialized.
        unsafe {
            ptr::copy(p.add(index), p.add(index + 1), self.len - index);
            (*p.add(index)).write(value);
        }
        self.len += 1;
        Ok(())
    }

    /// Remove the element at index and return it, replacing it with the last
    /// element (O(1), does not preserve order). None when out of range.
    pub fn swap_remove(&mut self, index: usize) -> Option<T> {
        if index >= self.len {
            return None;
        }
        let last = self.len - 1;
        self.as_mut_slice().swap(index, last);
        self.pop()
    }

    /// Remove and return the element at index, shifting later elements left.
    /// None when index is out of range.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.len {
            return None;
        }
        let p = self.buf.as_mut_ptr() as *mut T;
        // SAFETY: index < len, so p.add(index) is a live element; it is read out
        // first, then the tail (index+1, len) shifts down over its slot. len
        // shrinks by one so the vacated top slot is never read again.
        let value = unsafe {
            let value = ptr::read(p.add(index));
            ptr::copy(p.add(index + 1), p.add(index), self.len - index - 1);
            value
        };
        self.len -= 1;
        Some(value)
    }

    /// Keep only the elements for which keep returns true, in order, dropping
    /// the rest. The closure must not panic; this compaction is not unwind-safe.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        let p = self.buf.as_mut_ptr() as *mut T;
        let mut write = 0;
        for read in 0..self.len {
            // SAFETY: read < len, so this element is live.
            if unsafe { keep(&*p.add(read)) } {
                if write != read {
                    // SAFETY: write < read, both in bounds; the source is moved
                    // bitwise into the earlier slot and never dropped, so there
                    // is no double free.
                    unsafe { ptr::copy_nonoverlapping(p.add(read), p.add(write), 1) };
                }
                write += 1;
            } else {
                // SAFETY: read is live and is not copied anywhere, so dropping
                // it here is the only drop it gets.
                unsafe { ptr::drop_in_place(p.add(read)) };
            }
        }
        self.len = write;
    }

    /// Drop every live element and reset the length to zero.
    pub fn clear(&mut self) {
        let live: *mut [T] =
            ptr::slice_from_raw_parts_mut(self.buf.as_mut_ptr() as *mut T, self.len);
        self.len = 0;
        // SAFETY: the slice covers exactly the previously-live prefix, and len
        // is zeroed first so a panicking Drop cannot revisit a dropped element.
        unsafe { ptr::drop_in_place(live) };
    }

    /// Shorten to at most len elements, dropping the rest.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        let tail: *mut [T] = ptr::slice_from_raw_parts_mut(
            // SAFETY: len < self.len <= N, so this points at a live slot.
            unsafe { self.buf.as_mut_ptr().add(len) } as *mut T,
            self.len - len,
        );
        self.len = len;
        // SAFETY: tail spans the dropped suffix, all previously live.
        unsafe { ptr::drop_in_place(tail) };
    }

    pub fn as_slice(&self) -> &[T] {
        // SAFETY: the first len slots are initialized.
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr() as *const T, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: the first len slots are initialized.
        unsafe { core::slice::from_raw_parts_mut(self.buf.as_mut_ptr() as *mut T, self.len) }
    }
}

impl<T: Copy, const N: usize> ArrayVec<T, N> {
    /// Copy a slice onto the end. Returns Err with the count that fit when the
    /// whole slice does not.
    pub fn extend_from_slice(&mut self, src: &[T]) -> Result<(), usize> {
        let room = N - self.len;
        let take = src.len().min(room);
        // SAFETY: dst covers take dead slots starting at len; src and dst do
        // not overlap (dst is inside self, src is a borrowed slice).
        unsafe {
            ptr::copy_nonoverlapping(
                src.as_ptr(),
                self.buf.as_mut_ptr().add(self.len) as *mut T,
                take,
            );
        }
        self.len += take;
        if take == src.len() {
            Ok(())
        } else {
            Err(take)
        }
    }
}

impl<T, const N: usize> Drop for ArrayVec<T, N> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<T, const N: usize> Default for ArrayVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Deref for ArrayVec<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: PartialEq, const N: usize, const M: usize> PartialEq<ArrayVec<T, M>> for ArrayVec<T, N> {
    fn eq(&self, other: &ArrayVec<T, M>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq, const N: usize> Eq for ArrayVec<T, N> {}

impl<T: fmt::Debug, const N: usize> fmt::Debug for ArrayVec<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), f)
    }
}

impl<T, const N: usize> DerefMut for ArrayVec<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

/// A UTF-8 string with a compile-time byte capacity and no heap backing.
///
/// Only whole str and char pushes are accepted, so the bytes stay valid UTF-8
/// and as_str needs no revalidation. Writes that would overflow are reported.
#[derive(Clone, Copy)]
pub struct ArrayString<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> ArrayString<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn as_str(&self) -> &str {
        // SAFETY: only push_str and push(char) write the buffer, both of which
        // append valid UTF-8, so buf[..len] is always a valid str.
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Append a string slice. Returns Err and writes nothing when it would not
    /// fit, so the value stays whole rather than truncated mid-character.
    pub fn push_str(&mut self, s: &str) -> Result<(), ()> {
        let bytes = s.as_bytes();
        if self.len + bytes.len() > N {
            return Err(());
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }

    pub fn push(&mut self, c: char) -> Result<(), ()> {
        let mut tmp = [0u8; 4];
        self.push_str(c.encode_utf8(&mut tmp))
    }

    /// Insert a string slice at byte index idx (a char boundary), shifting the
    /// rest right. Err and no change if idx is invalid or it would overflow.
    pub fn insert_str(&mut self, idx: usize, s: &str) -> Result<(), ()> {
        if idx > self.len || !self.as_str().is_char_boundary(idx) {
            return Err(());
        }
        let add = s.len();
        if self.len + add > N {
            return Err(());
        }
        self.buf.copy_within(idx..self.len, idx + add);
        self.buf[idx..idx + add].copy_from_slice(s.as_bytes());
        self.len += add;
        Ok(())
    }

    /// Insert a char at byte index idx (a char boundary).
    pub fn insert(&mut self, idx: usize, ch: char) -> Result<(), ()> {
        let mut tmp = [0u8; 4];
        self.insert_str(idx, ch.encode_utf8(&mut tmp))
    }

    /// Remove and return the char starting at byte index idx, shifting the rest
    /// left. None if idx is not a char boundary within the string.
    pub fn remove(&mut self, idx: usize) -> Option<char> {
        if idx >= self.len {
            return None;
        }
        let ch = self.as_str().get(idx..)?.chars().next()?;
        let n = ch.len_utf8();
        self.buf.copy_within(idx + n..self.len, idx);
        self.len -= n;
        Some(ch)
    }

    /// Delete the byte range [start, end), shifting the rest left. Err and no
    /// change unless both bounds are char boundaries within the string: a
    /// partial delete would leave a lone continuation byte, and as_str would
    /// then hand out bytes that are not UTF-8.
    pub fn delete_range(&mut self, start: usize, end: usize) -> Result<(), ()> {
        if start > end || end > self.len {
            return Err(());
        }
        let s = self.as_str();
        if !s.is_char_boundary(start) || !s.is_char_boundary(end) {
            return Err(());
        }
        self.buf.copy_within(end..self.len, start);
        self.len -= end - start;
        Ok(())
    }
}

impl<const N: usize> Default for ArrayString<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Deref for ArrayString<N> {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl<const N: usize, const M: usize> PartialEq<ArrayString<M>> for ArrayString<N> {
    fn eq(&self, other: &ArrayString<M>) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<const N: usize> PartialEq<str> for ArrayString<N> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<const N: usize> PartialEq<&str> for ArrayString<N> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl<const N: usize> Eq for ArrayString<N> {}

impl<const N: usize> fmt::Write for ArrayString<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_str(s).map_err(|_| fmt::Error)
    }
}

impl<const N: usize> fmt::Debug for ArrayString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> fmt::Display for ArrayString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write;

    #[test]
    fn array_vec_push_pop_and_capacity() {
        let mut v: ArrayVec<u32, 3> = ArrayVec::new();
        assert!(v.is_empty());
        assert_eq!(v.push(1), Ok(()));
        assert_eq!(v.push(2), Ok(()));
        assert_eq!(v.push(3), Ok(()));
        assert!(v.is_full());
        assert_eq!(v.push(4), Err(4));
        assert_eq!(v.as_slice(), &[1, 2, 3]);
        assert_eq!(v.pop(), Some(3));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn array_vec_extend_reports_overflow() {
        let mut v: ArrayVec<u8, 4> = ArrayVec::new();
        assert_eq!(v.extend_from_slice(&[1, 2]), Ok(()));
        assert_eq!(v.extend_from_slice(&[3, 4, 5]), Err(2));
        assert_eq!(v.as_slice(), &[1, 2, 3, 4]);
    }

    #[test]
    fn array_vec_insert_shifts_right() {
        let mut v: ArrayVec<u8, 4> = ArrayVec::new();
        let _ = v.extend_from_slice(&[1, 2, 3]);
        assert_eq!(v.insert(1, 9), Ok(()));
        assert_eq!(v.as_slice(), &[1, 9, 2, 3]);
        // Full now: a further insert hands the value back.
        assert_eq!(v.insert(0, 7), Err(7));
    }

    #[test]
    fn array_vec_remove_shifts_left() {
        let mut v: ArrayVec<u8, 4> = ArrayVec::new();
        let _ = v.extend_from_slice(&[1, 2, 3]);
        assert_eq!(v.remove(1), Some(2));
        assert_eq!(v.as_slice(), &[1, 3]);
        assert_eq!(v.remove(5), None);
    }

    #[test]
    fn array_vec_retain_keeps_order_and_drops_rest() {
        let mut v: ArrayVec<u8, 6> = ArrayVec::new();
        let _ = v.extend_from_slice(&[1, 2, 3, 4, 5]);
        v.retain(|&x| x % 2 == 1);
        assert_eq!(v.as_slice(), &[1, 3, 5]);
    }

    #[test]
    fn array_vec_retain_drops_removed_elements() {
        use std::rc::Rc;
        let tag = Rc::new(());
        let mut v: ArrayVec<Rc<()>, 4> = ArrayVec::new();
        let _ = v.push(tag.clone());
        let _ = v.push(tag.clone());
        let _ = v.push(tag.clone());
        assert_eq!(Rc::strong_count(&tag), 4);
        let mut seen = 0;
        v.retain(|_| {
            seen += 1;
            seen == 2 // keep only the middle one
        });
        assert_eq!(v.len(), 1);
        assert_eq!(Rc::strong_count(&tag), 2);
    }

    #[test]
    fn array_vec_drops_live_elements() {
        use std::rc::Rc;
        let counter = Rc::new(());
        let mut v: ArrayVec<Rc<()>, 4> = ArrayVec::new();
        let _ = v.push(counter.clone());
        let _ = v.push(counter.clone());
        assert_eq!(Rc::strong_count(&counter), 3);
        v.truncate(1);
        assert_eq!(Rc::strong_count(&counter), 2);
        drop(v);
        assert_eq!(Rc::strong_count(&counter), 1);
    }

    #[test]
    fn array_string_push_and_overflow() {
        let mut s: ArrayString<8> = ArrayString::new();
        assert_eq!(s.push_str("abc"), Ok(()));
        assert_eq!(s.push('d'), Ok(()));
        assert_eq!(s.as_str(), "abcd");
        // Does not fit: nothing is written.
        assert_eq!(s.push_str("xxxxx"), Err(()));
        assert_eq!(s.as_str(), "abcd");
    }

    #[test]
    fn array_string_insert_and_remove() {
        let mut s: ArrayString<16> = ArrayString::new();
        let _ = s.push_str("ac");
        assert_eq!(s.insert(1, 'b'), Ok(()));
        assert_eq!(s.as_str(), "abc");
        assert_eq!(s.insert_str(3, "de"), Ok(()));
        assert_eq!(s.as_str(), "abcde");
        assert_eq!(s.remove(0), Some('a'));
        assert_eq!(s.as_str(), "bcde");
        assert_eq!(s.delete_range(1, 3), Ok(()));
        assert_eq!(s.as_str(), "be");
    }

    #[test]
    fn array_string_delete_range_rejects_split_char() {
        let mut s: ArrayString<16> = ArrayString::new();
        let _ = s.push_str("aéb");
        // 'é' occupies bytes 1..3, so 2 is inside it. Both bounds are checked.
        assert_eq!(s.delete_range(1, 2), Err(()));
        assert_eq!(s.delete_range(2, 3), Err(()));
        assert_eq!(s.as_str(), "aéb");
        assert_eq!(s.delete_range(1, 3), Ok(()));
        assert_eq!(s.as_str(), "ab");
    }

    #[test]
    fn array_string_delete_range_rejects_bad_bounds() {
        let mut s: ArrayString<16> = ArrayString::new();
        let _ = s.push_str("abc");
        assert_eq!(s.delete_range(2, 1), Err(()));
        assert_eq!(s.delete_range(0, 4), Err(()));
        assert_eq!(s.as_str(), "abc");
    }

    #[test]
    fn array_string_insert_multibyte() {
        let mut s: ArrayString<16> = ArrayString::new();
        let _ = s.push_str("ab");
        assert_eq!(s.insert(1, 'é'), Ok(()));
        assert_eq!(s.as_str(), "aéb");
        // Removing at the multibyte boundary returns the whole char.
        assert_eq!(s.remove(1), Some('é'));
        assert_eq!(s.as_str(), "ab");
    }

    #[test]
    fn array_vec_swap_remove() {
        let mut v: ArrayVec<u8, 4> = ArrayVec::new();
        let _ = v.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(v.swap_remove(1), Some(2));
        // Element 1 is now the former last element.
        assert_eq!(v.as_slice(), &[1, 4, 3]);
    }

    #[test]
    fn array_string_write_macro() {
        let mut s: ArrayString<16> = ArrayString::new();
        let _ = write!(s, "{}-{}", 1, 2);
        assert_eq!(s.as_str(), "1-2");
    }
}
