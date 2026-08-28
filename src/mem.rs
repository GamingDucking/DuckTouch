1|/*
2| * This Source Code Form is subject to the terms of the Mozilla Public
3| * License, v. 2.0. If a copy of the MPL was not distributed with this
4| * file, You can obtain one at https://mozilla.org/MPL/2.0/.
5| */
6|//! Types related to the virtual memory of the emulated application, or the
7|//! "guest memory".
8|//!
9|//! The virtual address space is 32-bit, as is the pointer size.
10|//!
11|//! No attempt is made to do endianness conversion for reads and writes to
12|//! memory, because all supported emulated and host platforms are little-endian.
13|//!
14|//! Relevant Apple documentation:
15|//! * [Memory Usage Performance Guidelines](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/ManagingMemory/ManagingMemory.html)
16|
17|use std::num::NonZeroU32;
18|
19|use crate::libc::wchar::wchar_t;
20|
21|mod allocator;
22|mod host;
23|
24|/// Equivalent of `usize` for guest memory.
25|pub type GuestUSize = u32;
26|
27|/// Equivalent of `isize` for guest memory.
28|pub type GuestISize = i32;
29|
30|/// Nonzero version of [GuestUSize].
31|pub type NonZeroGuestUSize = NonZeroU32;
32|
33|/// [std::mem::size_of], but returning a [GuestUSize].
34|pub const fn guest_size_of<T: Sized>() -> GuestUSize {
35|    assert!(std::mem::size_of::<T>() <= u32::MAX as usize);
36|    std::mem::size_of::<T>() as u32
37|}
38|
39|/// Internal type for representing an untyped virtual address.
40|type VAddr = GuestUSize;
41|
42|/// Internal type for representing an untyped virtual address.
43|type NonZeroVAddr = NonZeroGuestUSize;
44|
45|/// Pointer type for guest memory, or the "guest pointer" type.
46|///
47|/// The `MUT` type parameter determines whether this is mutable or not.
48|/// Don't write it out explicitly, use [ConstPtr], [MutPtr], [ConstVoidPtr] or
49|/// [MutVoidPtr] instead instead.
50|///
51|/// The implemented methods try to mirror the Rust [pointer] type's methods,
52|/// where possible.
53|#[repr(transparent)]
54|pub struct Ptr<T, const MUT: bool>(VAddr, std::marker::PhantomData<T>);
55|
56|// #[derive(...)] doesn't work for this type because it expects T to have the
57|// trait we want implemented
58|impl<T, const MUT: bool> Clone for Ptr<T, MUT> {
59|    fn clone(&self) -> Self {
60|        *self
61|    }
62|}
63|impl<T, const MUT: bool> Copy for Ptr<T, MUT> {}
64|impl<T, const MUT: bool> PartialEq for Ptr<T, MUT> {
65|    fn eq(&self, other: &Self) -> bool {
66|        self.0 == other.0
67|    }
68|}
69|impl<T, const MUT: bool> Eq for Ptr<T, MUT> {}
70|impl<T, const MUT: bool> std::hash::Hash for Ptr<T, MUT> {
71|    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
72|        self.0.hash(state);
73|    }
74|}
75|
76|/// Constant guest pointer type (like Rust's `*const T`).
77|pub type ConstPtr<T> = Ptr<T, false>;
78|
79|/// Mutable guest pointer type (like Rust's `*mut T`).
80|pub type MutPtr<T> = Ptr<T, true>;
81|
82|#[allow(dead_code)]
83|/// Constant guest pointer-to-void type (like C's `const void *`)
84|pub type ConstVoidPtr = ConstPtr<std::ffi::c_void>;
85|
86|/// Mutable guest pointer-to-void type (like C's `void *`)
87|pub type MutVoidPtr = MutPtr<std::ffi::c_void>;
88|
89|impl<T, const MUT: bool> Ptr<T, MUT> {
90|    pub const fn null() -> Self {
91|        Ptr(0, std::marker::PhantomData)
92|    }
93|
94|    pub fn to_bits(self) -> VAddr {
95|        self.0
96|    }
97|    pub const fn from_bits(bits: VAddr) -> Self {
98|        Ptr(bits, std::marker::PhantomData)
99|    }
100|
101|    pub fn cast<U>(self) -> Ptr<U, MUT> {
102|        Ptr::<U, MUT>::from_bits(self.to_bits())
103|    }
104|
105|    pub fn cast_void(self) -> Ptr<std::ffi::c_void, MUT> {
106|        self.cast()
107|    }
108|
109|    pub fn is_null(self) -> bool {
110|        self.to_bits() == 0
111|    }
112|
113|    pub fn non_null(self) -> Option<NonNullPtr<T>> {
114|        NonNullPtr::try_from_bits(self.0)
115|    }
116|}
117|
118|impl<T> ConstPtr<T> {
119|    #[allow(dead_code)]
120|    pub fn cast_mut(self) -> MutPtr<T> {
121|        Ptr::from_bits(self.to_bits())
122|    }
123|}
124|impl<T> MutPtr<T> {
125|    pub fn cast_const(self) -> ConstPtr<T> {
126|        Ptr::from_bits(self.to_bits())
127|    }
128|}
129|
130|impl<T, const MUT: bool> Default for Ptr<T, MUT> {
131|    fn default() -> Self {
132|        Self::null()
133|    }
134|}
135|
136|impl<T, const MUT: bool> std::fmt::Debug for Ptr<T, MUT> {
137|    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
138|        if self.is_null() {
139|            write!(f, "(null)")
140|        } else {
141|            write!(f, "{:#x}", self.to_bits())
142|        }
143|    }
144|}
145|
146|// C-like pointer arithmetic
147|impl<T, const MUT: bool> std::ops::Add<GuestUSize> for Ptr<T, MUT> {
148|    type Output = Self;
149|
150|    fn add(self, other: GuestUSize) -> Self {
151|        let size: GuestUSize = guest_size_of::<T>();
152|        assert_ne!(size, 0);
153|        // Real 32-bit ARM (ARMv7-A) computes addresses modulo 2^32: pointer
154|        // arithmetic silently wraps around the 4 GiB address space and never
155|        // traps. A fault only occurs when the resulting address is actually
156|        // *accessed* and points at unmapped memory — and `Mem::bytes_at` /
157|        // `bytes_at_mut` already handle that case gracefully via the null/OOB
158|        // stub pages. Using `checked_*().unwrap()` here instead turned benign
159|        // (or already-corrupt, but guest-local) pointer math into a hard host
160|        // panic, e.g. when a buggy guest computes `base + (size_t)(-N)` while
161|        // building a `std::string`/shader buffer. Mirror the hardware: wrap.
162|        Self::from_bits(self.to_bits().wrapping_add(other.wrapping_mul(size)))
163|    }
164|}
165|impl<T, const MUT: bool> std::ops::AddAssign<GuestUSize> for Ptr<T, MUT> {
166|    fn add_assign(&mut self, rhs: GuestUSize) {
167|        *self = *self + rhs;
168|    }
169|}
170|impl<T, const MUT: bool> std::ops::Sub<GuestUSize> for Ptr<T, MUT> {
171|    type Output = Self;
172|
173|    fn sub(self, other: GuestUSize) -> Self {
174|        let size: GuestUSize = guest_size_of::<T>();
175|        assert_ne!(size, 0);
176|        // See the note on `Add` above: 32-bit ARM address arithmetic wraps
177|        // modulo 2^32 and never traps, so subtracting past zero must wrap
178|        // rather than panic the host.
179|        Self::from_bits(self.to_bits().wrapping_sub(other.wrapping_mul(size)))
180|    }
181|}
182|impl<T, const MUT: bool> std::ops::SubAssign<GuestUSize> for Ptr<T, MUT> {
183|    fn sub_assign(&mut self, rhs: GuestUSize) {
184|        *self = *self - rhs;
185|    }
186|}
187|
188|/// Non-null pointer type for guest memory, similar to [std::ptr::NonNull].
189|/// You should use this wrapped in [Option] when storing types instead of
190|/// storing null pointers.
191|///
192|/// You can convert to this type using [Ptr::non_null] (where null pointers
193|/// will become [None] and other pointers will becone [Some], and convert back
194|/// using [Self::const_ptr] and [Self::mut_ptr].
195|#[repr(transparent)]
196|pub struct NonNullPtr<T>(NonZeroVAddr, std::marker::PhantomData<T>);
197|
198|#[allow(unused)]
199|pub type NonNullVoidPtr = NonNullPtr<std::ffi::c_void>;
200|
201|// #[derive(...)] doesn't work for this type because it expects T to have the
202|// trait we want implemented
203|impl<T> Clone for NonNullPtr<T> {
204|    fn clone(&self) -> Self {
205|        *self
206|    }
207|}
208|impl<T> Copy for NonNullPtr<T> {}
209|impl<T> PartialEq for NonNullPtr<T> {
210|    fn eq(&self, other: &Self) -> bool {
211|        self.0 == other.0
212|    }
213|}
214|impl<T> Eq for NonNullPtr<T> {}
215|impl<T> std::hash::Hash for NonNullPtr<T> {
216|    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
217|        self.0.hash(state);
218|    }
219|}
220|
221|#[allow(unused)]
222|impl<T> NonNullPtr<T> {
223|    pub fn to_bits(self) -> VAddr {
224|        self.0.into()
225|    }
226|    pub fn try_from_bits(bits: VAddr) -> Option<Self> {
227|        if bits == 0 {
228|            None
229|        } else {
230|            Some(Self(bits.try_into().unwrap(), std::marker::PhantomData))
231|        }
232|    }
233|
234|    pub fn from_bits(bits: VAddr) -> Self {
235|        Self::try_from_bits(bits).expect("Tried to create a NonNullPtr with a null value!")
236|    }
237|
238|    pub fn cast<U>(self) -> NonNullPtr<U> {
239|        NonNullPtr::<U>::try_from_bits(self.to_bits()).unwrap()
240|    }
241|
242|    pub fn cast_void(self) -> NonNullPtr<std::ffi::c_void> {
243|        self.cast()
244|    }
245|
246|    pub fn mut_ptr(self) -> MutPtr<T> {
247|        MutPtr::from_bits(self.0.into())
248|    }
249|
250|    pub fn const_ptr(self) -> MutPtr<T> {
251|        MutPtr::from_bits(self.0.into())
252|    }
253|}
254|
255|impl<T> std::fmt::Debug for NonNullPtr<T> {
256|    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
257|        write!(f, "{:#x}", self.to_bits())
258|    }
259|}
260|
261|/// Marker trait for types that can be safely read from guest memory.
262|///
263|/// See also [SafeWrite] and [crate::abi].
264|///
265|/// # Safety
266|/// Reading from guest memory is essentially doing a [std::mem::transmute],
267|/// which is notoriously unsafe in Rust.
268|/// Only types for which all possible bit
269|/// patterns are legal (e.g. integers) should have this trait.
270|pub unsafe trait SafeRead: Sized {}
271|// bool is one byte in size and has 0 as false, 1 as true in both Rust and ObjC
272|unsafe impl SafeRead for bool {}
273|unsafe impl SafeRead for i8 {}
274|unsafe impl SafeRead for u8 {}
275|unsafe impl SafeRead for i16 {}
276|unsafe impl SafeRead for u16 {}
277|unsafe impl SafeRead for i32 {}
278|unsafe impl SafeRead for u32 {}
279|unsafe impl SafeRead for i64 {}
280|unsafe impl SafeRead for u64 {}
281|unsafe impl SafeRead for f32 {}
282|unsafe impl SafeRead for f64 {}
283|unsafe impl<T, const MUT: bool> SafeRead for Ptr<T, MUT> {}
284|
285|/// Marker trait for types that can be written to guest memory.
286|///
287|/// Unlike for [SafeRead], there is no (Rust) safety consideration here;
288|/// it's
289|/// just a way to catch accidental use of types unintended for guest use.
290|/// This was added after discovering that `()` is "[Sized]" and therefore a
291|/// single stray semicolon can wreak havoc...
292|///
293|/// Especially for structs, be careful that the type matches the expected ABI.
294|/// At minimum you should have `#[repr(C, packed)]` and appropriate padding
295|/// members.
296|///
297|/// See also [SafeRead] and [crate::abi].
298|pub trait SafeWrite: Sized {}
299|impl<T: SafeRead> SafeWrite for T {}
300|
301|type Bytes = [u8; 1 << 32];
302|pub const PAGE_SIZE: GuestUSize = 4096;
303|pub const PAGE_SIZE_ALIGN_MASK: GuestUSize = 0xfff;
304|
305|/// The type that owns the guest memory and provides accessors for it.
306|pub struct Mem {
307|    /// This array is 4GiB in size so that it can cover the entire 32-bit
308|    /// virtual address space, but it should not use that much physical memory,
309|    /// assuming that the host OS backs it with lazily-allocated pages and we
310|    /// are careful to avoid accessing most of it.
311|    ///
312|    /// iPhone OS devices only had 128MiB or 256MiB of RAM total, with no swap
313|    /// space, so less than 6.25% of this array should be used, assuming no
314|    /// fragmentation.
315|    ///
316|    /// This is a raw pointer because inevitably we will have to hand out
317|    /// pointers to memory sometimes, and being able to hold a `&mut` on this
318|    /// array simultaneously seems like an undefined behavior trap.
319|    /// This also
320|    /// means that the underlying memory should never be moved, and therefore
321|    /// the array can't be growable.
322|    ///
323|    /// One advantage of `[u8; 1 << 32]` over `[u8]` is that it might help rustc
324|    /// optimize away bounds checks for `memory.bytes[ptr_32bit as usize]`.
325|    ///
326|    /// Note that unless direct memory access is disabled, the CPU emulation
327|    /// (dynarmic) accesses memory via this pointer directly except when a page
328|    /// fault occurs.
329|    bytes: *mut Bytes,
330|
331|    /// The size of the __PAGE_ZERO segment, where pointer accesses are trapped
332|    /// to prevent null pointer derefrences.
333|    ///
334|    /// We don't have full memory protection, but we can check accesses in that
335|    /// range.
336|    null_segment_size: VAddr,
337|
338|    allocator: allocator::Allocator,
339|
340|    /// The flag to control if memory is zeroed out on free (`true`, default)
341|    /// or on alloc (`false`).
342|    ///
343|    /// Right now only one game, Spore Origin, is setting this value to `false`
344|    /// via a game-specific hack.
345|    /// See [crate::Environment] for more info.
346|    pub(super) zero_memory_on_free: bool,
347|
348|    /// HACK: stub page for null-page READ accesses.
349|    /// Filled with zeros so that reading *(void**)NULL returns NULL.
350|    /// This page is NEVER written to by guest code — writes go to
351|    /// `null_write_sink` instead.
352|    null_stub_page: *mut u8,
353|
354|    /// HACK: separate write-sink page for null-page WRITE accesses.
355|    /// Writes to the null page go here and are silently discarded.
356|    /// This prevents write operations from corrupting the zero-filled
357|    /// read stub page.
358|    null_write_sink: *mut u8,
359|}
360|
361|impl Drop for Mem {
362|    fn drop(&mut self) {
363|        unsafe {
364|            crate::mem::host::free_guest_memory(self.bytes.cast(), std::mem::size_of::<Bytes>())
365|                .unwrap();
366|            // Free the read stub page
367|            if !self.null_stub_page.is_null() {
368|                crate::mem::host::free_memory(self.null_stub_page.cast(), PAGE_SIZE as usize)
369|                    .unwrap();
370|            }
371|            // Free the write sink page
372|            if !self.null_write_sink.is_null() {
373|                crate::mem::host::free_memory(self.null_write_sink.cast(), PAGE_SIZE as usize)
374|                    .unwrap();
375|            }
376|        }
377|    }
378|}
379|
380|impl Mem {
381|    /// [According to Apple](https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/CreatingThreads/CreatingThreads.html)
382|    /// among others, the iPhone OS main thread stack size is 1MiB.
383|    pub const MAIN_THREAD_STACK_SIZE: GuestUSize = 1024 * 1024;
384|
385|    /// Address of the lowest byte (not the base) of the main thread's stack.
386|    ///
387|    /// We are arbitrarily putting the stack at the top of the virtual address
388|    /// space (see also: stack.rs), I have no idea if this matches iPhone OS.
389|    pub const MAIN_THREAD_STACK_LOW_END: VAddr = 0u32.wrapping_sub(Self::MAIN_THREAD_STACK_SIZE);
390|
391|    /// iPhone OS secondary thread stack size.
392|    pub const SECONDARY_THREAD_DEFAULT_STACK_SIZE: GuestUSize = 512 * 1024;
393|
394|    /// Create a fresh instance of guest memory.
395|    pub fn new() -> Mem {
396|        let size = std::mem::size_of::<Bytes>();
397|        let ptr = unsafe { crate::mem::host::allocate_guest_memory(size).unwrap() };
398|
399|        assert_eq!(
400|            ptr as usize & PAGE_SIZE_ALIGN_MASK as usize,
401|            0,
402|            "Failed to align host memory with guest memory"
403|        );
404|        let bytes = ptr as *mut Bytes;
405|
406|        // Allocate read stub page for null-page reads (4KB, zero-filled).
407|        // Data reads of a NULL pointer (e.g. `*(void**)0`) return NULL.
408|        let null_stub_page = unsafe {
409|            let page = crate::mem::host::allocate_memory(PAGE_SIZE as usize).unwrap();
410|            let stub_slice = std::slice::from_raw_parts_mut(page as *mut u8, PAGE_SIZE as usize);
411|            stub_slice.fill(0);
412|            page as *mut u8
413|        };
414|
415|        // Allocate a separate write-sink page for null-page writes (4KB).
416|        // Writes to the null page are absorbed here so that they don't
417|        // corrupt the read stub page's zeros.
418|        let null_write_sink = unsafe {
419|            let page = crate::mem::host::allocate_memory(PAGE_SIZE as usize).unwrap();
420|            let sink_slice = std::slice::from_raw_parts_mut(page as *mut u8, PAGE_SIZE as usize);
421|            sink_slice.fill(0);
422|            page as *mut u8
423|        };
424|
425|        let allocator = allocator::Allocator::new();
426|        Mem {
427|            bytes,
428|            null_segment_size: 0,
429|            allocator,
430|            zero_memory_on_free: true,
431|            null_stub_page,
432|            null_write_sink,
433|        }
434|    }
435|
436|    /// Sets up the null segment of the given size.
437|    /// There's no reason to call
438|    /// this outside of binary loading, and it won't be respected even if you
439|    /// do.
440|    /// The size must not have been set already, and must be page aligned.
441|    pub fn set_null_segment_size(&mut self, new_null_segment_size: VAddr) {
442|        // TODO?: Maybe this should be replaced with a per-page rwx/callback
443|        //        setting?
444|        //        Currently we don't properly follow segment
445|        //        protections, which means that applications can write into
446|        //        segments they shouldn't be able to.
447|        //        Adding that would fix
448|        //        this, along with removing this special case.
449|        assert!(self.null_segment_size == 0);
450|        assert!(new_null_segment_size.is_multiple_of(0x1000));
451|        self.allocator
452|            .reserve(allocator::Chunk::new(0, new_null_segment_size));
453|        self.null_segment_size = new_null_segment_size;
454|    }
455|
456|    pub fn null_segment_size(&self) -> VAddr {
457|        self.null_segment_size
458|    }
459|
460|    /// Get a pointer to the full 4GiB of memory.
461|    /// This is only for use when
462|    /// setting up the CPU, never call this otherwise.
463|    ///
464|    /// Safety: You must ensure that this pointer does not outlive the instance
465|    /// of [Mem].
466|    /// You must not use it while a `&mut` is held on some region of
467|    /// guest memory.
468|    pub unsafe fn direct_memory_access_ptr(&mut self) -> *mut std::ffi::c_void {
469|        self.bytes.cast()
470|    }
471|
472|    fn bytes(&self) -> &Bytes {
473|        unsafe { &*self.bytes }
474|    }
475|    fn bytes_mut(&mut self) -> &mut Bytes {
476|        unsafe { &mut *self.bytes }
477|    }
478|
479|    // Soft handler for null-page accesses. No panic; returns a stub page.
480|    // Rate-limited: only the first N unique (addr, is_write) pairs are logged,
481|    // further occurrences are silently counted. This prevents the log from
482|    // being flooded when the game repeatedly probes null-page addresses.
483|    #[cold]
    #[cold]
    fn null_check_fail(at: VAddr, size: GuestUSize, is_write: bool, caller: &str) {
        use std::collections::HashSet;
        use std::sync::Mutex;
        static SEEN: Mutex<Option<HashSet<(VAddr, bool)>>> = Mutex::new(None);
        const MAX_UNIQUE_LOGS: usize = 64;

        // Breadcrumb dump: what were the last ObjC messages dispatched?
        let breadcrumbs = crate::objc::messages::OBJ_C_BREADCRUMBS.with(|b| {
            b.borrow().clone()
        });
489|
490|        let mut guard = SEEN.lock().unwrap();
491|        let set = guard.get_or_insert_with(HashSet::new);
492|        let key = (at, is_write);
493|        if set.contains(&key) {
494|            return;
495|        }
496|        if set.len() >= MAX_UNIQUE_LOGS {
497|            if set.len() == MAX_UNIQUE_LOGS {
498|                // Insert a sentinel to emit the notice only once.
499|                set.insert((0xFFFF_FFFE, false));
500|                log!(
501|                    "touchHLE::mem: further NULL-PAGE warnings silenced after {} unique sites",
502|                    MAX_UNIQUE_LOGS
503|                );
504|            }
505|            return;
506|        }
507|        set.insert(key);
508|        let op_type = if is_write { "WRITE" } else { "READ" };
509|        // Provide helpful context: small offsets are typically field accesses
510|        // on a nil Objective-C object pointer (nil + ivar offset). This is
511|        // defined behavior in ObjC (returns 0/nil) and is NOT a crash — just
512|        // a sign that the app is accessing a nil object's fields.
513|        let context = if !is_write && at < 0x1000 {
514|            " (likely nil ObjC object field access — returning zero)"
515|        } else if is_write && at < 0x1000 {
516|            " (likely nil ObjC object field write — discarding)"
517|        } else {
518|            " — returning stub page"
519|        };
520|        log!(
521|            "touchHLE::mem: NULL-PAGE {} at 0x{:08x} (size: 0x{:x}) from {}{} \
522|             (unique sites logged: {}/{})",
523|            op_type,
524|            at,
525|            size,
526|            caller,
527|            context,
528|            set.len(),
529|            MAX_UNIQUE_LOGS
530|        );

        if !breadcrumbs.is_empty() {
            log!("  -> Breadcrumbs (last 10 ObjC calls):");
            for (i, (recv, sel)) in breadcrumbs.iter().enumerate() {
                log!("     {}. receiver=0x{:08x} selector={}", i + 1, recv, sel);
            }
        }
    }
532|
533|    /// Special version of [Self::bytes_at] that returns [None] rather than
534|    /// panicking on failure.
535|    /// Only for use by [crate::gdb::GdbServer].
536|    pub fn get_bytes_fallible(&self, addr: ConstVoidPtr, count: GuestUSize) -> Option<&[u8]> {
537|        if addr.to_bits() < self.null_segment_size {
538|            // Для GDB возвращаем stub-страницу
539|            let offset = (addr.to_bits() % PAGE_SIZE) as usize;
540|            let count_usize = count as usize;
541|            let stub_slice = unsafe {
542|                std::slice::from_raw_parts(
543|                    self.null_stub_page.add(offset),
544|                    PAGE_SIZE as usize - offset,
545|                )
546|            };
547|            return Some(&stub_slice[..count_usize.min(stub_slice.len())]);
548|        }
549|        self.bytes()
550|            .get(addr.to_bits() as usize..)?
551|            .get(..count as usize)
552|    }
553|    /// Special version of [Self::bytes_at_mut] that returns [None] rather than
554|    /// panicking on failure.
555|    /// Only for use by [crate::gdb::GdbServer].
556|    pub fn get_bytes_fallible_mut(
557|        &mut self,
558|        addr: ConstVoidPtr,
559|        count: GuestUSize,
560|    ) -> Option<&mut [u8]> {
561|        if addr.to_bits() < self.null_segment_size {
562|            return None;
563|            // GDB не должен писать в null-page
564|        }
565|        self.bytes_mut()
566|            .get_mut(addr.to_bits() as usize..)?
567|            .get_mut(..count as usize)
568|    }
569|
570|    /// Get a slice for reading `count` bytes.
571|    /// This is the basic primitive for
572|    /// safe read-only memory access.
573|    ///
574|    /// This will panic when `ptr` is within the null page, even if `count` is
575|    /// 0. This may be inconvenient in some cases, but it makes the behavior
576|    /// when deriving a pointer from the slice consistent (though you should use
577|    /// [Self::ptr_at] for that).
578|    pub fn bytes_at<const MUT: bool>(&self, ptr: Ptr<u8, MUT>, count: GuestUSize) -> &[u8] {
579|        // ХАК: Вместо паники логируем и возвращаем данные из stub-страницы
580|        if ptr.to_bits() < self.null_segment_size {
581|            Self::null_check_fail(ptr.to_bits(), count, false, "bytes_at");
582|            // Возвращаем данные из stub-страницы вместо реальной памяти
583|            // Это предотвращает UndefinedInstruction когда игра использует
584|            // прочитанные значения как указатели на функции
585|            let offset = (ptr.to_bits() % PAGE_SIZE) as usize;
586|            let count_usize = count as usize;
587|            let available = PAGE_SIZE as usize - offset;
588|            let actual_count = count_usize.min(available);
589|            return unsafe {
590|                std::slice::from_raw_parts(self.null_stub_page.add(offset), actual_count)
591|            };
592|        }
593|        // Guard against out-of-bounds reads near the top of the 32-bit address
594|        // space. If `ptr + count` wraps around or exceeds the backing array,
595|        // return the stub page. This prevents panics when a game uses -1 or
596|        // another near-max address as a pointer (corrupted pointer arithmetic).
597|        let addr = ptr.to_bits() as usize;
598|        let end = addr.saturating_add(count as usize);
599|        if end > self.bytes().len() || end < addr {
600|            Self::null_check_fail(ptr.to_bits(), count, false, "bytes_at(OOB)");
601|            let offset = (ptr.to_bits() % PAGE_SIZE) as usize;
602|            let count_usize = count as usize;
603|            let available = PAGE_SIZE as usize - offset;
604|            let actual_count = count_usize.min(available);
605|            return unsafe {
606|                std::slice::from_raw_parts(self.null_stub_page.add(offset), actual_count)
607|            };
608|        }
609|        &self.bytes()[addr..][..count as usize]
610|    }
611|    /// Get a slice for reading `count` bytes without a null-page check.
612|    ///
613|    /// This **doesn't** panic at access within the null page.
614|    ///
615|    /// You shall have a good reason to use it instead of [Self::bytes_at]
616|    pub fn unchecked_bytes_at<const MUT: bool>(
617|        &self,
618|        ptr: Ptr<u8, MUT>,
619|        count: GuestUSize,
620|    ) -> &[u8] {
621|        let addr = ptr.to_bits() as usize;
622|        let end = addr.saturating_add(count as usize);
623|        if end > self.bytes().len() || end < addr {
624|            Self::null_check_fail(ptr.to_bits(), count, false, "unchecked_bytes_at(OOB)");
625|            let offset = (ptr.to_bits() % PAGE_SIZE) as usize;
626|            let count_usize = count as usize;
627|            let available = PAGE_SIZE as usize - offset;
628|            let actual_count = count_usize.min(available);
629|            return unsafe {
630|                std::slice::from_raw_parts(self.null_stub_page.add(offset), actual_count)
631|            };
632|        }
633|        &self.bytes()[addr..][..count as usize]
634|    }
635|    /// Get a slice for reading or writing `count` bytes.
636|    /// This is the basic
637|    /// primitive for safe read-write memory access.
638|    ///
639|    /// This will panic when `ptr` is within the null page, even if `count` is
640|    /// 0. This may be inconvenient in some cases, but it makes the behavior
641|    /// when deriving a pointer from the slice consistent (though you should use
642|    /// [Self::ptr_at_mut] for that).
643|    pub fn bytes_at_mut(&mut self, ptr: MutPtr<u8>, count: GuestUSize) -> &mut [u8] {
644|        // ХАК: Вместо паники логируем и возвращаем данные из stub-страницы
645|        if ptr.to_bits() < self.null_segment_size {
646|            Self::null_check_fail(ptr.to_bits(), count, true, "bytes_at_mut");
647|            // For writes to null-page, return the write-sink page so that
648|            // writes are silently absorbed without corrupting the read stub
649|            // page's zeros.
650|            let offset = (ptr.to_bits() % PAGE_SIZE) as usize;
651|            let count_usize = count as usize;
652|            let available = PAGE_SIZE as usize - offset;
653|            let actual_count = count_usize.min(available);
654|            return unsafe {
655|                std::slice::from_raw_parts_mut(self.null_write_sink.add(offset), actual_count)
656|            };
657|        }
658|        // Guard against out-of-bounds writes near the top of the 32-bit
659|        // address space (e.g. corrupted pointer = 0xFFFFFFFF).
660|        let addr = ptr.to_bits() as usize;
661|        let end = addr.saturating_add(count as usize);
662|        if end > self.bytes().len() || end < addr {
663|            Self::null_check_fail(ptr.to_bits(), count, true, "bytes_at_mut(OOB)");
664|            let offset = (ptr.to_bits() % PAGE_SIZE) as usize;
665|            let count_usize = count as usize;
666|            let available = PAGE_SIZE as usize - offset;
667|            let actual_count = count_usize.min(available);
668|            return unsafe {
669|                std::slice::from_raw_parts_mut(self.null_write_sink.add(offset), actual_count)
670|            };
671|        }
672|        &mut self.bytes_mut()[addr..][..count as usize]
673|    }
674|
675|    /// Get a pointer for reading an array of `count` elements of type `T`.
676|    /// Only use this for interfacing with unsafe C-like APIs.
677|    ///
678|    /// The `count` argument is purely for bounds-checking and does not affect
679|    /// the result.
680|    ///
681|    /// No guarantee is made about the alignment of the resulting pointer!
682|    /// Pointers that are well-aligned for the guest are not necessarily
683|    /// well-aligned for the host.
684|    /// Rust strictly requires pointers to be
685|    /// well-aligned when dereferencing them, or when constructing references or
686|    /// slices from them, so **be very careful**.
687|    pub fn ptr_at<T, const MUT: bool>(&self, ptr: Ptr<T, MUT>, count: GuestUSize) -> *const T
688|    where
689|        T: SafeRead,
690|    {
691|        let size = count.checked_mul(guest_size_of::<T>()).unwrap();
692|        self.bytes_at(ptr.cast(), size).as_ptr().cast()
693|    }
694|    /// A variation of [Self::ptr_at] without a null-page check.
695|    ///
696|    /// This **doesn't** panic at access within the null page.
697|    ///
698|    /// You shall have a good reason to use it instead of [Self::ptr_at]
699|    pub fn unchecked_ptr_at<T, const MUT: bool>(
700|        &self,
701|        ptr: Ptr<T, MUT>,
702|        count: GuestUSize,
703|    ) -> *const T
704|    where
705|        T: SafeRead,
706|    {
707|        let size = count.checked_mul(guest_size_of::<T>()).unwrap();
708|        self.unchecked_bytes_at(ptr.cast(), size).as_ptr().cast()
709|    }
710|    /// Get a pointer for reading or writing to an array of `count` elements of
711|    /// type `T`.
712|    /// Only use this for interfacing with unsafe C-like APIs.
713|    ///
714|    /// The `count` argument is purely for bounds-checking and does not affect
715|    /// the result.
716|    ///
717|    /// No guarantee is made about the alignment of the resulting pointer!
718|    /// Pointers that are well-aligned for the guest are not necessarily
719|    /// well-aligned for the host.
720|    /// Rust strictly requires pointers to be
721|    /// well-aligned when dereferencing them, or when constructing references or
722|    /// slices from them, so **be very careful**.
723|    pub fn ptr_at_mut<T>(&mut self, ptr: MutPtr<T>, count: GuestUSize) -> *mut T
724|    where
725|        T: SafeRead + SafeWrite,
726|    {
727|        let size = count.checked_mul(guest_size_of::<T>()).unwrap();
728|        self.bytes_at_mut(ptr.cast(), size).as_mut_ptr().cast()
729|    }
730|
731|    /// Transform a host pointer addressing a location in guest memory back into
732|    /// a guest pointer.
733|    /// This exists solely to deal with OpenGL `glGetPointerv`.
734|    /// You should never have another reason to use this.
735|    ///
736|    /// Panics if the host pointer is not addressing a location in guest memory.
737|    pub fn host_ptr_to_guest_ptr(&self, host_ptr: *const std::ffi::c_void) -> ConstVoidPtr {
738|        let host_ptr = host_ptr.cast::<u8>();
739|        let guest_mem_range = self.bytes().as_ptr_range();
740|        assert!(guest_mem_range.contains(&host_ptr));
741|        let guest_addr = host_ptr as usize - guest_mem_range.start as usize;
742|        Ptr::from_bits(u32::try_from(guest_addr).unwrap())
743|    }
744|
745|    /// Returns whether a host pointer addresses a location inside the guest's
746|    /// memory region. Used to sanity-check pointers that touchHLE hands to host
747|    /// APIs (e.g. client-side OpenGL vertex arrays): a pointer outside this
748|    /// range is wild and dereferencing it on the host would crash the emulator.
749|    pub fn is_host_ptr_in_guest_mem(&self, host_ptr: *const std::ffi::c_void) -> bool {
750|        let host_ptr = host_ptr.cast::<u8>();
751|        self.bytes().as_ptr_range().contains(&host_ptr)
752|    }
753|
754|    /// Read a value for memory.
755|    /// This is the preferred way to read memory in
756|    /// most cases.
757|    pub fn read<T, const MUT: bool>(&self, ptr: Ptr<T, MUT>) -> T
758|    where
759|        T: SafeRead,
760|    {
761|        // This is unsafe unless we are careful with which types SafeRead is
762|        // implemented for!
763|        // This would also be unsafe if the non-unaligned method was used.
764|        unsafe { self.ptr_at(ptr, 1).read_unaligned() }
765|    }
766|    /// Write a value to memory.
767|    /// This is the preferred way to write memory in
768|    /// most cases.
769|    pub fn write<T>(&mut self, ptr: MutPtr<T>, value: T)
770|    where
771|        T: SafeWrite,
772|    {
773|        let size = guest_size_of::<T>();
774|        assert!(size > 0);
775|        let slice = self.bytes_at_mut(ptr.cast(), size);
776|        let ptr: *mut T = slice.as_mut_ptr().cast();
777|        // It's unaligned because what is well-aligned for the guest is not
778|        // necessarily well-aligned for the host.
779|        // This would be unsafe if the non-unaligned method was used.
780|        unsafe { ptr.write_unaligned(value) }
781|    }
782|
783|    /// C-style `memmove`.
784|    ///
785|    /// Sanity-checks the arguments. If `src + size` or `dest + size` would
786|    /// run off the end of the 4 GiB guest address space, the operation is
787|    /// logged and skipped instead of panicking. This is a defensive measure
788|    /// for guest code that calls `memmove`/`memcpy` with corrupted arguments
789|    /// (for example, an uninitialised `std::string` whose internal length
790|    /// happens to be wildly out of range): a guest bug shouldn't take down
791|    /// the whole emulator.
792|    pub fn memmove(&mut self, dest: MutVoidPtr, src: ConstVoidPtr, size: GuestUSize) {
793|        let src_addr = src.to_bits() as usize;
794|        let dest_addr = dest.to_bits() as usize;
795|        let size_us = size as usize;
796|        let max = self.bytes_mut().len();
797|
798|        // Early reject: if size looks like a negative i32 cast to u32
799|        // (>= 0x8000_0000), it's almost certainly corrupted. Guest code on
800|        // 32-bit ARM that passes (size_t)(-1) or similar huge values is
801|        // buggy — skip the operation to keep the emulator alive.
802|        if size >= 0x8000_0000 {
803|            log!(
804|                "WARNING: memmove with likely-negative size ({:#x} = {}); \
805|                 src={:#x}, dest={:#x} — skipping",
806|                size,
807|                size as i32,
808|                src_addr,
809|                dest_addr,
810|            );
811|            return;
812|        }
813|
814|        // Also reject NULL source — real memmove(dest, NULL, n) is UB
815|        // but guest games (Geometry Dash) trigger it via corrupted strings.
816|        if src_addr == 0 && size > 0 {
817|            log!(
818|                "WARNING: memmove from NULL (dest={:#x}, size={:#x}) — skipping",
819|                dest_addr,
820|                size_us,
821|            );
822|            return;
823|        }
824|
825|        let src_end = match src_addr.checked_add(size_us) {
826|            Some(v) if v <= max => v,
827|            _ => {
828|                log!(
829|                    "WARNING: memmove with bogus args (src={:#x}, dest={:#x}, \
830|                     size={:#x}) — skipping to avoid host crash",
831|                    src_addr,
832|                    dest_addr,
833|                    size_us
834|                );
835|                return;
836|            }
837|        };
838|        let dest_end = match dest_addr.checked_add(size_us) {
839|            Some(v) if v <= max => v,
840|            _ => {
841|                log!(
842|                    "WARNING: memmove with bogus args (src={:#x}, dest={:#x}, \
843|                     size={:#x}) — skipping to avoid host crash",
844|                    src_addr,
845|                    dest_addr,
846|                    size_us
847|                );
848|                return;
849|            }
850|        };
851|        let _ = (src_end, dest_end);
852|
853|        self.bytes_mut()
854|            .copy_within(src_addr..src_addr + size_us, dest_addr)
855|    }
856|
857|    /// Allocate `size` bytes.
858|    pub fn alloc(&mut self, size: GuestUSize) -> MutVoidPtr {
859|        let ptr = Ptr::from_bits(self.allocator.alloc(size));
860|        if !self.zero_memory_on_free {
861|            self.bytes_at_mut(ptr.cast(), size).fill(0);
862|        }
863|
864|        log_dbg!("Allocated {:?} ({:#x} bytes)", ptr, size);
865|        ptr
866|    }
867|
868|    /// Allocate `size` bytes initialized to 0.
869|    pub fn calloc(&mut self, size: GuestUSize) -> MutVoidPtr {
870|        let ptr = self.alloc(size);
871|        self.bytes_at_mut(ptr.cast(), size).fill(0);
872|        ptr
873|    }
874|
875|    /// Implements Apple's documented `malloc_size(3)` contract: returns the
876|    /// size of the memory block that backs the allocation pointed to by
877|    /// `ptr`, or `0` if `ptr` is `NULL` or doesn't belong to any block
878|    /// allocated through malloc. This is deliberately a *silent* lookup —
879|    /// it's perfectly normal for apps to call `malloc_size` on arbitrary
880|    /// pointers (interior pointers, `__DATA` symbols, stack addresses,
881|    /// etc.) and treat a `0` result as "this isn't a heap allocation",
882|    /// so we must not flood the log when it happens. See
883|    /// <https://developer.apple.com/library/archive/documentation/Performance/Conceptual/ManagingMemory/Articles/MallocDebug.html>.
884|    pub fn malloc_size(&self, ptr: ConstVoidPtr) -> GuestUSize {
885|        if ptr.is_null() {
886|            return 0;
887|        }
888|        self.allocator
889|            .try_find_allocated_size(ptr.to_bits())
890|            .unwrap_or(0)
891|    }
892|
893|    /// Returns whether `addr` is the exact base of a live allocation. Used to
894|    /// defensively reject garbage pointers at the libc free() wrapper.
895|    pub fn is_known_allocation(&self, addr: VAddr) -> bool {
896|        self.allocator.is_known_allocation(addr)
897|    }
898|
899|    pub fn realloc(&mut self, old_ptr: MutVoidPtr, size: GuestUSize) -> MutVoidPtr {
900|        if old_ptr.is_null() {
901|            return self.alloc(size);
902|        }
903|
904|        // TODO: for a moment we always assume that we do not have enough size
905|        //       to realloc inplace
906|        let old_size = self.allocator.find_allocated_size(old_ptr.to_bits());
907|        if old_size >= size {
908|            return old_ptr;
909|        }
910|
911|        let new_ptr = self.alloc(size);
912|        self.memmove(new_ptr, old_ptr.cast_const(), old_size);
913|        self.free(old_ptr);
914|        new_ptr
915|    }
916|
917|    /// Free an allocation made with one of the `alloc` methods on this type.
918|    pub fn free(&mut self, ptr: MutVoidPtr) {
919|        if ptr.is_null() {
920|            return;
921|        }
922|        let addr = ptr.to_bits();
923|        // Silently ignore attempts to free the MACH_TASK_SELF constant
924|        // (0x7461736b = "task"). The Mono runtime stores this value and
925|        // attempts to free it during shutdown; it's not a real allocation.
926|        if addr == 0x7461736b {
927|            return;
928|        }
929|        // Reject obviously bogus pointers before passing to the allocator.
930|        if !self.allocator.is_known_allocation(addr) {
931|            log!("Can't free {:#x}, unknown allocation!", addr);
932|            return;
933|        }
934|        let size = self.allocator.free(addr);
935|        if self.zero_memory_on_free {
936|            self.bytes_at_mut(ptr.cast(), size).fill(0);
937|        }
938|
939|        log_dbg!("Freed {:?} ({:#x} bytes)", ptr, size);
940|    }
941|
942|    /// Allocate memory large enough for a value of type `T` and write the value
943|    /// to it.
944|    /// Equivalent to [Self::alloc] + [Self::write].
945|    pub fn alloc_and_write<T>(&mut self, value: T) -> MutPtr<T>
946|    where
947|        T: SafeWrite,
948|    {
949|        let ptr = self.alloc(guest_size_of::<T>()).cast();
950|        self.write(ptr, value);
951|        ptr
952|    }
953|
954|    /// Allocate and write a C string.
955|    /// This method will add a null terminator,
956|    /// so it is optimal if the input slice does not already contain one.
957|    pub fn alloc_and_write_cstr(&mut self, str_bytes: &[u8]) -> MutPtr<u8> {
958|        let len = str_bytes.len().try_into().unwrap();
959|        let ptr = self.alloc(len + 1).cast();
960|        self.bytes_at_mut(ptr, len).copy_from_slice(str_bytes);
961|        self.write(ptr + len, b'\0');
962|        ptr
963|    }
964|
965|    /// Get a C string (null-terminated) as a slice.
966|    /// The null terminator is not
967|    /// included in the slice.
968|    ///
969|    /// Safety: includes a maximum length guard (64KB) to prevent infinite loops
970|    /// if the guest provides a pointer to non-terminated data.
971|    pub fn cstr_at<const MUT: bool>(&self, ptr: Ptr<u8, MUT>) -> &[u8] {
972|        const MAX_CSTR_LEN: u32 = 65536; // 64KB safety limit
973|        self.cstr_at_with_max_len(ptr, MAX_CSTR_LEN)
974|    }
975|
976|    /// Like [Self::cstr_at], but with a caller-chosen maximum length instead of
977|    /// the default 64KB safety limit. Useful for data that can legitimately be
978|    /// larger than 64KB (e.g. GLSL shader source uploaded via `glShaderSource`
979|    /// without an explicit length), where the default cap would silently
980|    /// truncate the string and corrupt it.
981|    pub fn cstr_at_with_max_len<const MUT: bool>(
982|        &self,
983|        ptr: Ptr<u8, MUT>,
984|        max_len: u32,
985|    ) -> &[u8] {
986|        let mut len: u32 = 0;
987|        while self.read(ptr + len) != b'\0' {
988|            len += 1;
989|            if len >= max_len {
990|                log!(
991|                    "Warning: cstr_at({:?}): hit {}B safety limit without finding null terminator; truncating.",
992|                    ptr, max_len
993|                );
994|                break;
995|            }
996|        }
997|        self.bytes_at(ptr, len)
998|    }
999|
1000|    /// Get a C string (null-terminated) as a string slice, if it is valid
1001|    /// UTF-8, otherwise returning a byte slice.
1002|    /// The null terminator is not
1003|    /// included in the slice.
1004|    pub fn cstr_at_utf8<const MUT: bool>(&self, ptr: Ptr<u8, MUT>) -> Result<&str, &[u8]> {
1005|        let bytes = self.cstr_at(ptr);
1006|        std::str::from_utf8(bytes).map_err(|_| bytes)
1007|    }
1008|
1009|    pub fn wcstr_at<const MUT: bool>(&self, ptr: Ptr<wchar_t, MUT>) -> String {
1010|        const MAX_WCSTR_LEN: u32 = 16384; // 16K chars safety limit
1011|        let mut len: u32 = 0;
1012|        while self.read(ptr + len) != wchar_t::default() {
1013|            len += 1;
1014|            if len >= MAX_WCSTR_LEN {
1015|                log!(
1016|                    "Warning: wcstr_at({:?}): hit {} char safety limit without finding null terminator; truncating.",
1017|                    ptr, MAX_WCSTR_LEN
1018|                );
1019|                break;
1020|            }
1021|        }
1022|
1023|        // iOS/macOS uses 4-byte wchar_t (UTF-32LE). char::from_u32 returns
1024|        // None for surrogate values (U+D800..U+DFFF) and codepoints above
1025|        // U+10FFFF; in those cases we substitute U+FFFD REPLACEMENT CHARACTER
1026|        // instead of panicking so that bogus data from the guest does not
1027|        // crash the host.
1028|        let bytes = self.bytes_at(ptr.cast(), len * guest_size_of::<wchar_t>());
1029|        let iter = bytes.chunks_exact(4).map(|chunk| {
1030|            // chunks_exact(4) guarantees the length, so try_into never fails.
1031|            let code = u32::from_le_bytes(chunk.try_into().unwrap());
1032|            char::from_u32(code).unwrap_or('\u{FFFD}')
1033|        });
1034|        String::from_iter(iter)
1035|    }
1036|
1037|    /// Permanently mark a region of address space as being unusable to the
1038|    /// memory allocator.
1039|    ///
1040|    /// A zero-byte reservation is a documented no-op: it matches what xnu's
1041|    /// `mach_loader.c` does when handed a `LC_SEGMENT` whose `vmsize == 0`
1042|    /// (the kernel reserves no address space, the segment is silently
1043|    /// ignored). We mirror that here so the allocator's `Chunk` invariant —
1044|    /// every chunk must contain at least one byte — is preserved even when
1045|    /// callers (Mach-O loader, `dyld::do_initial_linking`, etc.) hand us a
1046|    /// degenerate request.
1047|    pub fn reserve(&mut self, base: VAddr, size: GuestUSize) {
1048|        if size == 0 {
1049|            log_dbg!(
1050|                "Mem::reserve({:#x}, 0) — no-op (matches xnu mach_loader.c)",
1051|                base
1052|            );
1053|            return;
1054|        }
1055|        self.allocator.reserve(allocator::Chunk::new(base, size));
1056|    }
1057|}
1058|
1059|#[cfg(test)]
1060|mod mem_tests {
1061|    use super::{Mem, MutPtr, Ptr};
1062|
1063|    #[test]
1064|    fn lazy_commit_far_addresses() {
1065|        let mut mem = Mem::new();
1066|
1067|        mem.set_null_segment_size(super::PAGE_SIZE);
1068|
1069|        let probes: [u32; 6] = [
1070|            0x0000_1000,
1071|            0x1000_0000,
1072|            0x4000_0000,
1073|            0x8000_0000,
1074|            0xC000_0000,
1075|            0xFFFE_F000,
1076|        ];
1077|        for &addr in &probes {
1078|            let p: MutPtr<u8> = Ptr::from_bits(addr);
1079|            mem.write(p, 0xAB);
1080|            assert_eq!(mem.read(p.cast_const()), 0xAB);
1081|        }
1082|    }
1083|
1084|    #[test]
1085|    fn ptr_arithmetic_wraps_modulo_2_32() {
1086|        // Real 32-bit ARM computes addresses modulo 2^32 and never traps on
1087|        // the arithmetic itself. These cases previously panicked the host via
1088|        // `checked_*().unwrap()`; they must now wrap like the hardware.
1089|        let near_top: Ptr<u8, true> = Ptr::from_bits(0xFFFF_FFFB);
1090|        assert_eq!((near_top + 0x10).to_bits(), 0x0000_000B);
1091|
1092|        let low: Ptr<u8, true> = Ptr::from_bits(0x0000_0004);
1093|        assert_eq!((low - 0x10).to_bits(), 0xFFFF_FFF4);
1094|
1095|        // Element-sized arithmetic (u32 = 4 bytes) must also wrap rather than
1096|        // overflow when the multiplied offset exceeds the address space.
1097|        let p: Ptr<u32, true> = Ptr::from_bits(0xFFFF_FFF0);
1098|        assert_eq!((p + 0x8).to_bits(), 0x0000_0010);
1099|    }
1100|}
1101|