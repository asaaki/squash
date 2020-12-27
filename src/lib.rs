// TODO: Can we make this alloc-only?

use std::alloc::{self, Layout};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter, Result as FmtResult};
use std::marker::PhantomData;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::ptr::{self, NonNull};
use std::slice;
use std::str;

// We want to have the null pointer optimisation but we also don't want to allocate for empty
// slices. That means we need some pointer that denotes an empty slice that we recognize and won't
// ever be returned from the allocator, but is not Null. So we simply get this pointer.
//
// Note that this may lead to unaligned pointer. That is OK if the pointer is never dereferenced.
static ZERO_SENTINEL: u8 = 0;

#[derive(Copy, Clone, Debug)]
pub struct TooLong;

impl Display for TooLong {
    fn fmt(&self, fmt: &mut Formatter) -> FmtResult {
        write!(fmt, "Too long")
    }
}

impl Error for TooLong { }

pub unsafe trait Header {
    fn extra_needed(len: usize) -> Result<usize, TooLong>;
    unsafe fn encode_len(len: usize, extra: *mut u8) -> Self;
    unsafe fn decode_len(&self, extra: *const u8) -> usize;
    fn inc(&self) -> bool;
    fn dec(&self) -> bool;
}

pub struct BoxHeader(u8);

// TODO: Variable length encoding
unsafe impl Header for BoxHeader {
    #[inline]
    fn extra_needed(len: usize) -> Result<usize, TooLong> {
        if len > u8::MAX as usize {
            Err(TooLong)
        } else {
            Ok(0)
        }
    }
    #[inline]
    unsafe fn encode_len(len: usize, _: *mut u8) -> Self {
        assert!(len <= u8::MAX as usize);
        Self(len as u8)
    }
    #[inline]
    unsafe fn decode_len(&self, _: *const u8) -> usize {
        self.0 as usize
    }
    #[inline]
    fn inc(&self) -> bool {
        false
    }
    #[inline]
    fn dec(&self) -> bool {
        true
    }
}

pub struct OwnedSlice<T, H = BoxHeader>
where
    H: Header,
{
    header: NonNull<H>,
    _data: PhantomData<T>,
}

impl<T, H> OwnedSlice<T, H>
where
    H: Header,
{
    #[inline]
    fn len(&self) -> usize {
        if self.is_sentinel() {
            return 0;
        }

        unsafe {
            let header = &*self.header.as_ref();
            let len_data = self.header.as_ptr().cast::<u8>().add(Self::len_offset());
            header.decode_len(len_data)
        }
    }

    #[inline]
    fn len_offset() -> usize {
        Layout::new::<H>().extend(Layout::array::<u8>(0).unwrap()).unwrap().1
    }

    #[inline]
    fn layout_and_offsets(len: usize) -> Result<(Layout, usize, usize), TooLong> {
        let extra = H::extra_needed(len)?;
        let (l1, len_off) = Layout::new::<H>()
            .extend(Layout::array::<u8>(extra).expect("Insanely large stuff"))
            .expect("Insanely large stuff");
        let data_layout = Layout::array::<T>(len).expect("Insanely large stuff");
        let (layout, data_off) = l1.extend(data_layout).expect("Insanely large stuff");
        Ok((layout, len_off, data_off))
    }

    #[inline]
    fn layout(len: usize) -> Layout {
        Self::layout_and_offsets(len).unwrap().0
    }

    #[inline]
    fn data_offset(len: usize) -> usize {
        Self::layout_and_offsets(len).unwrap().2
    }

    #[inline]
    fn data(&self, len: usize) -> *mut T {
        let offset = Self::data_offset(len);
        unsafe {
            self.header.as_ptr().cast::<u8>().add(offset).cast::<T>()
        }
    }

    #[inline]
    fn is_sentinel(&self) -> bool {
        ptr::eq(self.header.as_ptr().cast::<u8>(), &ZERO_SENTINEL)
    }

    pub fn new(src: &[T]) -> Result<Self, TooLong>
    where
        T: Clone,
    {
        if src.is_empty() {
            // Use the sentinel thing
            return Ok(Self::default());
        }

        let len = src.len();
        let (layout, len_off, data_offset) = Self::layout_and_offsets(len)?;
        assert!(layout.size() > 0, "TODO: Handle 0 layout? Can it even happen?");
        let ptr = unsafe { alloc::alloc(layout) };
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }
        unsafe {
            let data_ptr = ptr.add(data_offset).cast::<T>();
            let len_ptr = ptr.add(len_off);
            let hdr = ptr.cast::<H>();

            // Initialize everything
            ptr::write(hdr, H::encode_len(len, len_ptr));
            for (idx, src) in src.iter().enumerate() {
                // FIXME: Handle panics and release the memory/call destructors. Currently it is
                // not UB, but we leak all the cloned things and the allocation. Not great.
                ptr::write(data_ptr.add(idx), src.clone());
            }

            Ok(Self {
                header: NonNull::new(hdr).unwrap(),
                _data: PhantomData,
            })
        }
    }

    // TODO: Some more constructors? Something without cloning?
}

impl<T, H> Drop for OwnedSlice<T, H>
where
    H: Header,
{
    fn drop(&mut self) {
        if self.is_sentinel() {
            return;
        }

        unsafe {
            if self.header.as_ref().dec() {
                let len = self.len();
                let layout = Self::layout(len);
                if mem::needs_drop::<T>() {
                    let data = self.data(len);

                    for i in 0..len {
                        ptr::drop_in_place(data.add(i));
                    }
                }

                alloc::dealloc(self.header.as_ptr().cast::<u8>(), layout);
            }
        }
    }
}

impl<T, H> Clone for OwnedSlice<T, H>
where
    H: Header,
    T: Clone,
{
    fn clone(&self) -> Self {
        if !self.is_sentinel() && unsafe { self.header.as_ref().inc() } {
            Self {
                header: self.header,
                _data: PhantomData,
            }
        } else {
            Self::new(self.deref()).expect("Already have layout for this size")
        }
    }
}

impl<T, H> Deref for OwnedSlice<T, H>
where
    H: Header,
{
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        if self.is_sentinel() {
            return &[];
        }

        let len = self.len();
        unsafe {
            slice::from_raw_parts(self.data(len), len)
        }
    }
}

impl<T> DerefMut for OwnedSlice<T, BoxHeader> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        if self.is_sentinel() {
            return &mut [];
        }

        let len = self.len();
        unsafe {
            slice::from_raw_parts_mut(self.data(len), len)
        }
    }
}

impl<T, H> Debug for OwnedSlice<T, H>
where
    H: Header,
    T: Debug,
{
    fn fmt(&self, fmt: &mut Formatter) -> FmtResult {
        self.deref().fmt(fmt)
    }
}

impl<T, H> Default for OwnedSlice<T, H>
where
    H: Header,
{
    fn default() -> Self {
        Self {
            header: NonNull::new((&ZERO_SENTINEL as *const u8 as *mut u8).cast()).unwrap(),
            _data: PhantomData,
        }
    }
}

// With some headers, we do Arc-like sharing of stuff. Therefore we need to be conservative about
// these and require both Send + Sync as the bounds, just like Arc.
unsafe impl<T, H> Send for OwnedSlice<T, H>
where
    H: Header + Send + Sync,
    T: Send + Sync,
{}

unsafe impl<T, H> Sync for OwnedSlice<T, H>
where
    H: Header + Send + Sync,
    T: Send + Sync,
{}

#[derive(Clone, Default)]
pub struct Str<H: Header = BoxHeader>(OwnedSlice<u8, H>);

impl<H> Str<H>
where
    H: Header,
{
    pub fn new(s: &str) -> Result<Self, TooLong> {
        OwnedSlice::new(s.as_bytes()).map(Self)
    }
}

impl<H> Debug for Str<H>
where
    H: Header,
{
    fn fmt(&self, fmt: &mut Formatter) -> FmtResult {
        write!(fmt, "{:?}", self.deref())
    }
}

impl<H> Display for Str<H>
where
    H: Header,
{
    fn fmt(&self, fmt: &mut Formatter) -> FmtResult {
        write!(fmt, "{}", self.deref())
    }
}

impl<H> Deref for Str<H>
where
    H: Header,
{
    type Target = str;

    fn deref(&self) -> &str {
        // It was created from str originally
        unsafe { str::from_utf8_unchecked(&self.0) }
    }
}

impl DerefMut for Str<BoxHeader> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // It was created from str originally
        unsafe { str::from_utf8_unchecked_mut(&mut self.0) }
    }
}

// TODO: CStr and other wrappers

#[cfg(test)]
mod tests {
    use super::*;

    /// Check we have the null-pointer optimisation.
    #[test]
    fn null_ptr_opt() {
        assert_eq!(
            mem::size_of::<OwnedSlice<String>>(),
            mem::size_of::<Option<OwnedSlice<String>>>(),
        );
    }

    /// Exercise the special handling of the sentinel.
    #[test]
    fn empty() {
        let mut s = OwnedSlice::<String>::new(&[]).unwrap();
        assert_eq!(s.deref(), &[] as &[String]);
        assert_eq!(s.deref_mut(), &mut [] as &mut [String]);
        let s2 = s.clone();
        assert_eq!(&s as &[_], &s2 as &[_]);
        assert_eq!("[]", format!("{:?}", s));

        let s3 = OwnedSlice::<String>::default();
        assert_eq!(&s as &[_], &s3 as &[_]);
    }

    /// Test with few strings.
    ///
    /// Use strings so miri can check we run destructors alright.
    #[test]
    fn full() {
        let mut s = OwnedSlice::<String>::new(&[
            "Hello".to_owned(),
            "World".to_owned(),
        ]).unwrap();
        assert_eq!(2, s.len());
        assert_eq!(s[1], "World");
        s[0] = "Round".to_owned();
        assert_eq!(s[0], "Round");
        let s2 = s.clone();
        assert_eq!(s.deref(), s2.deref());
        assert_eq!(2, s2.len());
        assert_eq!("[\"Round\", \"World\"]", format!("{:?}", s2));
    }

    #[test]
    fn too_long() {
        let long = vec![0u8; 300];
        OwnedSlice::<_>::new(&long).unwrap_err();
    }

    #[test]
    fn strings() {
        let s: Str = Str::new("Hello").unwrap();
        assert_eq!("Hello", s.deref());
        assert_eq!("Hello", s.to_string());
        assert_eq!("\"Hello\"", format!("{:?}", s));
    }
}
