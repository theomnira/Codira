//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Heap snapshots: write every object of a [`MarkSweep`] heap, with its type,
//!   root count and contents, to a byte stream, and rebuild an equivalent heap
//!   from it in another runtime (or another process) that loaded the same
//!   assembly.
//!
//! Format (all integers little-endian):
//!
//! ```text
//! magic "CODIRA-HEAP\0" | version u32 | pointer size u32
//! type count u32, then per type:   tag u8 (0 concrete: guid [u8; 16],
//!                                          1 array: element type follows)
//! object count u64, then per object: type index u32 | root count u32 |
//!   struct: byte count u64, payload bytes
//!   array:  element count u64, element bytes
//! ```
//!
//! Every GC reference inside a payload is stored as the referenced object's
//! index plus one (`0` is null), so the snapshot does not depend on
//! addresses. Types are stored by GUID and resolved against the restoring
//! runtime's type table; a struct whose layout differs from the one in the
//! snapshot is refused rather than misread. Raw pointers cannot be carried
//! across processes, so a heap holding a type with pointer fields is refused
//! too.
//!
//! A snapshot must be taken, like a collection, while no other thread runs
//! Codira code.

use std::{
    io::{self, Read, Write},
    ptr::NonNull,
};

use rustc_hash::FxHashMap;

use crate::{
    abi,
    gc::{
        array::ArrayHeader,
        heap::{handle_of, header_of, ObjectHeader},
        mark_sweep::array_data_offset,
        safepoint, Event, GcPtr, MarkSweep, Observer,
    },
    r#type::{Type, TypeData, TypeKind},
};

const MAGIC: &[u8; 12] = b"CODIRA-HEAP\0";
const VERSION: u32 = 1;

/// An error while writing or reading a heap snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// Reading or writing the stream failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The stream is not a heap snapshot this version can read.
    #[error("not a readable heap snapshot: {0}")]
    Format(String),
    /// A type in the snapshot does not exist in the restoring runtime.
    #[error("the snapshot uses a type the runtime does not have (GUID {0})")]
    UnknownType(abi::Guid),
    /// A type's layout changed since the snapshot was taken.
    #[error("the layout of `{0}` differs from the one in the snapshot")]
    LayoutMismatch(String),
    /// A type holds raw pointers, which mean nothing in another process.
    #[error("`{0}` contains raw pointers, which a snapshot cannot carry")]
    RawPointers(String),
}

/// What a snapshot contained after it was restored.
#[derive(Debug, Default)]
pub struct RestoredHeap {
    /// Every restored object, in snapshot order.
    pub objects: Vec<GcPtr>,
    /// The restored objects that were rooted, with their root counts. They
    /// keep those counts: unroot each of them as often as its count says
    /// once the host no longer needs it.
    pub roots: Vec<(GcPtr, u32)>,
}

/// A type as stored in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TypeKey {
    Concrete(abi::Guid),
    Array(Box<TypeKey>),
}

impl TypeKey {
    fn of(ty: &Type) -> Result<TypeKey, SnapshotError> {
        match ty.kind() {
            TypeKind::Primitive(guid) => Ok(TypeKey::Concrete(*guid)),
            TypeKind::Struct(s) => Ok(TypeKey::Concrete(*s.guid())),
            TypeKind::Array(a) => Ok(TypeKey::Array(Box::new(TypeKey::of(&a.element_type())?))),
            TypeKind::Pointer(_) => Err(SnapshotError::RawPointers(ty.name().to_owned())),
        }
    }

    fn write(&self, out: &mut impl Write) -> io::Result<()> {
        match self {
            TypeKey::Concrete(guid) => {
                out.write_all(&[0])?;
                out.write_all(&guid.0)
            }
            TypeKey::Array(element) => {
                out.write_all(&[1])?;
                element.write(out)
            }
        }
    }

    fn read(input: &mut impl Read, depth: u32) -> Result<TypeKey, SnapshotError> {
        if depth > 64 {
            return Err(SnapshotError::Format(
                "array types nest too deeply".to_owned(),
            ));
        }
        match read_u8(input)? {
            0 => {
                let mut guid = [0u8; 16];
                input.read_exact(&mut guid)?;
                Ok(TypeKey::Concrete(abi::Guid(guid)))
            }
            1 => Ok(TypeKey::Array(Box::new(TypeKey::read(input, depth + 1)?))),
            tag => Err(SnapshotError::Format(format!("unknown type tag {tag}"))),
        }
    }

    fn resolve(
        &self,
        resolve: &impl Fn(&abi::Guid) -> Option<Type>,
    ) -> Result<Type, SnapshotError> {
        match self {
            TypeKey::Concrete(guid) => resolve(guid).ok_or(SnapshotError::UnknownType(*guid)),
            TypeKey::Array(element) => Ok(element.resolve(resolve)?.array_type()),
        }
    }
}

fn read_u8(input: &mut impl Read) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    input.read_exact(&mut buf)?;
    Ok(buf[0])
}

fn read_u32(input: &mut impl Read) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    input.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(input: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    input.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

/// Returns true if a value of type `ty`, stored inline (as a field or an
/// array element), is or contains a raw pointer.
fn inline_has_raw_pointers(ty: &TypeData) -> bool {
    if ty.is_pointer() {
        return true;
    }
    // A reference is stored as a handle; the object it refers to is checked
    // on its own.
    !ty.is_reference() && payload_has_raw_pointers(ty)
}

/// Returns true if the payload of an object of type `ty` contains a raw
/// pointer.
fn payload_has_raw_pointers(ty: &TypeData) -> bool {
    match ty.array_element() {
        Some(element) => inline_has_raw_pointers(element),
        None => ty
            .struct_fields()
            .into_iter()
            .any(|(field, _)| inline_has_raw_pointers(field)),
    }
}

/// An object read from a snapshot, not yet allocated.
struct Pending {
    ty: usize,
    roots: u32,
    length: usize,
    bytes: Vec<u8>,
}

/// The layout of the bytes a snapshot stores for an object.
struct ObjectLayout<'t> {
    element: Option<&'t TypeData>,
    /// For arrays: the data offset in the payload and the element stride.
    data_offset: usize,
    stride: usize,
}

impl<'t> ObjectLayout<'t> {
    fn of(ty: &'t TypeData) -> Self {
        match ty.array_element() {
            Some(element) => ObjectLayout {
                element: Some(element),
                data_offset: array_data_offset(element),
                stride: element.reference_layout().pad_to_align().size(),
            },
            None => ObjectLayout {
                element: None,
                data_offset: 0,
                stride: 0,
            },
        }
    }

    /// Offsets of the GC references within the stored bytes of an object of
    /// `ty` with `length` elements (arrays) or its payload (structs).
    fn reference_slots(&self, ty: &TypeData, length: usize) -> Vec<usize> {
        match self.element {
            Some(element) if element.is_reference() => {
                (0..length).map(|i| i * self.stride).collect()
            }
            Some(element) => {
                let offsets = element.ref_offsets();
                (0..length)
                    .flat_map(|i| offsets.iter().map(move |&o| i * self.stride + o as usize))
                    .collect()
            }
            None => ty.ref_offsets().iter().map(|&o| o as usize).collect(),
        }
    }
}

impl<O> MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    /// Writes every object of the heap to `out`. Returns the number of
    /// objects written.
    ///
    /// The world is stopped while the heap is read, so the snapshot is
    /// consistent even while other threads run Codira code.
    pub fn write_snapshot(&self, out: &mut impl Write) -> Result<usize, SnapshotError> {
        safepoint::stop_the_world(|_| self.write_snapshot_stopped(out))
    }

    fn write_snapshot_stopped(&self, out: &mut impl Write) -> Result<usize, SnapshotError> {
        let inner = self.heap.inner.read();
        let objects = inner.objects();
        let index_of: FxHashMap<usize, u64> = objects
            .iter()
            .enumerate()
            .map(|(index, o)| (o.as_ptr() as usize, index as u64 + 1))
            .collect();

        // Collect the distinct types, checking each can be carried.
        let mut types: Vec<TypeKey> = Vec::new();
        let mut type_index: FxHashMap<usize, u32> = FxHashMap::default();
        let mut object_types = Vec::with_capacity(objects.len());
        for object in &objects {
            let header = unsafe { object.as_ref() };
            let address = header.ty as usize;
            let index = if let Some(&index) = type_index.get(&address) {
                index
            } else {
                let ty = unsafe { header.type_view() };
                if payload_has_raw_pointers(unsafe { header.type_data() }) {
                    return Err(SnapshotError::RawPointers(ty.name().to_owned()));
                }
                types.push(TypeKey::of(&ty)?);
                let index = u32::try_from(types.len() - 1).expect("fewer than 2^32 types");
                type_index.insert(address, index);
                index
            };
            object_types.push(index);
        }

        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&(std::mem::size_of::<usize>() as u32).to_le_bytes())?;
        out.write_all(&(types.len() as u32).to_le_bytes())?;
        for key in &types {
            key.write(out)?;
        }

        out.write_all(&(objects.len() as u64).to_le_bytes())?;
        let mut bytes = Vec::new();
        for (object, ty_index) in objects.iter().zip(object_types) {
            let header = unsafe { object.as_ref() };
            let ty = unsafe { header.type_data() };
            let layout = ObjectLayout::of(ty);
            out.write_all(&ty_index.to_le_bytes())?;
            out.write_all(
                &header
                    .roots
                    .load(std::sync::atomic::Ordering::Acquire)
                    .to_le_bytes(),
            )?;

            let length = if layout.element.is_some() {
                let length = unsafe { (*header.data.cast::<ArrayHeader>()).length };
                bytes.clear();
                bytes.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(
                        header.data.add(layout.data_offset),
                        length * layout.stride,
                    )
                });
                out.write_all(&(length as u64).to_le_bytes())?;
                length
            } else {
                let size = ty.layout().size();
                bytes.clear();
                bytes.extend_from_slice(unsafe { std::slice::from_raw_parts(header.data, size) });
                out.write_all(&(size as u64).to_le_bytes())?;
                0
            };

            // Replace every reference by an object index
            for slot in layout.reference_slots(ty, length) {
                let raw = usize::from_ne_bytes(
                    bytes[slot..slot + std::mem::size_of::<usize>()]
                        .try_into()
                        .expect("a reference slot is pointer-sized"),
                );
                let id = if raw == 0 {
                    0
                } else {
                    *index_of.get(&raw).ok_or_else(|| {
                        SnapshotError::Format(format!(
                            "an object of `{}` refers to {raw:#x}, which is not in the heap",
                            unsafe { header.type_view() }.name()
                        ))
                    })?
                };
                bytes[slot..slot + 8].copy_from_slice(&id.to_le_bytes());
            }
            out.write_all(&bytes)?;
        }
        out.flush()?;
        Ok(objects.len())
    }

    /// Rebuilds the objects of a snapshot written by
    /// [`MarkSweep::write_snapshot`] in this heap. `resolve` maps a type
    /// GUID to the runtime's type (usually through its type table). Objects
    /// already in the heap are left alone.
    ///
    /// The world is stopped while the objects are rebuilt: no collection can
    /// run before they are rooted, and allocating them never collects (an
    /// allocation that would exceed the heap limit fails the restore).
    pub fn read_snapshot(
        &self,
        input: &mut impl Read,
        resolve: impl Fn(&abi::Guid) -> Option<Type>,
    ) -> Result<RestoredHeap, SnapshotError> {
        safepoint::stop_the_world(|_| self.read_snapshot_stopped(input, resolve))
    }

    fn read_snapshot_stopped(
        &self,
        input: &mut impl Read,
        resolve: impl Fn(&abi::Guid) -> Option<Type>,
    ) -> Result<RestoredHeap, SnapshotError> {
        let mut magic = [0u8; 12];
        input.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(SnapshotError::Format("bad magic".to_owned()));
        }
        let version = read_u32(input)?;
        if version != VERSION {
            return Err(SnapshotError::Format(format!(
                "version {version}, expected {VERSION}"
            )));
        }
        let pointer_size = read_u32(input)?;
        if pointer_size as usize != std::mem::size_of::<usize>() || pointer_size != 8 {
            return Err(SnapshotError::Format(format!(
                "taken with {pointer_size}-byte pointers"
            )));
        }

        let type_count = read_u32(input)?;
        let mut types = Vec::with_capacity(type_count.min(1 << 16) as usize);
        for _ in 0..type_count {
            types.push(TypeKey::read(input, 0)?.resolve(&resolve)?);
        }

        // Read every object first, so references can point forward.
        let object_count = read_u64(input)?;
        let mut pending = Vec::new();
        for _ in 0..object_count {
            let ty = read_u32(input)? as usize;
            let roots = read_u32(input)?;
            let Some(ty_ref) = types.get(ty) else {
                return Err(SnapshotError::Format(format!(
                    "type index {ty} out of range"
                )));
            };
            let data = unsafe { ty_ref.data_ptr().as_ref() };
            let layout = ObjectLayout::of(data);
            let count = usize::try_from(read_u64(input)?)
                .map_err(|_overflow| SnapshotError::Format("object too large".to_owned()))?;
            let (length, size) = if layout.element.is_some() {
                let size = count
                    .checked_mul(layout.stride)
                    .ok_or_else(|| SnapshotError::Format("array too large".to_owned()))?;
                (count, size)
            } else {
                if count != data.layout().size() {
                    return Err(SnapshotError::LayoutMismatch(ty_ref.name().to_owned()));
                }
                (0, count)
            };
            let mut bytes = vec![0u8; size];
            input.read_exact(&mut bytes)?;
            pending.push(Pending {
                ty,
                roots,
                length,
                bytes,
            });
        }

        // Allocate, then fill in.
        let mut restored = RestoredHeap::default();
        for object in &pending {
            let ty = &types[object.ty];
            let handle = if ty.is_array() {
                self.try_alloc_array(ty, object.length)
                    .map(|array| crate::gc::Array::as_raw(&array))
            } else {
                self.try_alloc(ty)
            }
            .map_err(|err| SnapshotError::Format(format!("allocation failed: {err}")))?;
            restored.objects.push(handle);
        }

        for (object, &handle) in pending.iter_mut().zip(&restored.objects) {
            let ty = &types[object.ty];
            let data = unsafe { ty.data_ptr().as_ref() };
            let layout = ObjectLayout::of(data);
            for slot in layout.reference_slots(data, object.length) {
                let id = u64::from_le_bytes(
                    object.bytes[slot..slot + 8]
                        .try_into()
                        .expect("a reference slot is 8 bytes"),
                );
                let target = if id == 0 {
                    0usize
                } else {
                    let index = usize::try_from(id - 1).unwrap_or(usize::MAX);
                    let Some(target) = restored.objects.get(index) else {
                        return Err(SnapshotError::Format(format!(
                            "reference to object {id} of {object_count}"
                        )));
                    };
                    target.as_ptr() as usize
                };
                object.bytes[slot..slot + 8].copy_from_slice(&target.to_ne_bytes());
            }

            let header: NonNull<ObjectHeader> = header_of(handle);
            unsafe {
                let payload = header.as_ref().data;
                std::ptr::copy_nonoverlapping(
                    object.bytes.as_ptr(),
                    payload.add(layout.data_offset),
                    object.bytes.len(),
                );
                if object.roots > 0 {
                    header
                        .as_ref()
                        .roots
                        .fetch_add(object.roots, std::sync::atomic::Ordering::AcqRel);
                    restored.roots.push((handle_of(header), object.roots));
                }
            }
        }

        Ok(restored)
    }
}
