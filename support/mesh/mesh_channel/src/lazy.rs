// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods for lazily looking up serialize and deserialize functions for a
//! type.
//!
//! This mechanism is used to allow a type's serialize and deserialize functions
//! to only be instantiated by the compiler if it is actually possible to call
//! them at runtime. If it's known to be impossible for a given type to be
//! serialized/deserialized, then there is no reason to include the generated
//! code in the binary. This has two advanges:
//!
//! * Reduces code side by not including code for types that will never be sent
//!   across a process boundary.
//! * Makes it possible to support mesh channels over types that cannot be
//!   serialized at all.
//!
//! Ordinarily the linker would be able to eliminate dead code, but because of
//! the way mesh ports work, the linker cannot statically determine that these
//! serialize functions are dead.
//!
//! So instead, at the point where it becomes possible for a mesh port to need
//! to serialize or deserialize its contents (i.e. when serializing the port, or
//! when changing the port's type so that the port's peer might need to
//! serialize+deserialize to convert the message types), we set the associated
//! serialize/deserialize functions.
//!
//! For now, we store those function pointers in a global map, which we try to
//! avoid accessing more than necessary to avoid expensive RwLock operations.
//! This would become simpler if Rust supported generic/associated statics, so
//! that the serialize/deserialize functions could just be some static data
//! associated with each type T. Alas.
//!
//! Another alternative might be to use weak linkage somehow, but this is not
//! exposed in stable Rust.

// UNSAFETY: Transmutes between function types to erase generics.
#![expect(unsafe_code)]

use mesh_node::message::MeshPayload;
use mesh_node::message::SerializeMessage;
use mesh_node::resource::Resource;
use mesh_node::resource::SerializedMessage;
use mesh_protobuf::encoding::SerializedMessageEncoder;
use mesh_protobuf::protobuf::MessageSizer;
use mesh_protobuf::protobuf::MessageWriter;
use mesh_protobuf::MessageEncode;
use parking_lot::RwLock;
use std::any::TypeId;
use std::collections::HashMap;
use std::marker::PhantomData;

/// A serializer type for `T`, to be used with [`LazyMessage`].
#[repr(transparent)]
pub struct SerializeFn<T>(static_ref::StaticRef<dyn DynMessageEncode<T>>);

trait DynMessageEncode<T>: Send + Sync {
    fn compute_message_size(&self, msg: &mut T, sizer: MessageSizer<'_>);
    fn write_message(&self, msg: T, writer: MessageWriter<'_, '_, Resource>);
}

struct DynEncoder<E>(PhantomData<E>);

impl<T, E> DynMessageEncode<T> for DynEncoder<E>
where
    E: MessageEncode<T, Resource> + Send + Sync,
{
    fn compute_message_size(&self, msg: &mut T, sizer: MessageSizer<'_>) {
        E::compute_message_size(msg, sizer)
    }

    fn write_message(&self, msg: T, writer: MessageWriter<'_, '_, Resource>) {
        E::write_message(msg, writer)
    }
}

impl<T> Copy for SerializeFn<T> {}

impl<T> Clone for SerializeFn<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> std::fmt::Debug for SerializeFn<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SerializeFn").finish()
    }
}

/// A deserializer type for `T`, to be used with [`LazyMessage`].
#[repr(transparent)]
pub struct DeserializeFn<T>(fn(SerializedMessage) -> Result<T, mesh_protobuf::Error>);

impl<T> Copy for DeserializeFn<T> {}

impl<T> Clone for DeserializeFn<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> std::fmt::Debug for DeserializeFn<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DeserializeFn").finish()
    }
}

static SERIALIZE: RwLock<Option<HashMap<TypeId, ([usize; 2], usize)>>> = RwLock::new(None);

static SERIALIZED_MESSAGE_SERIALIZE: SerializeFn<SerializedMessage> =
    SerializeFn(static_ref::StaticRef::new(&DynEncoder::<
        SerializedMessageEncoder,
    >(PhantomData)));
pub fn ensure_serializable<T: MeshPayload>() -> (SerializeFn<T>, DeserializeFn<T>) {
    let id = TypeId::of::<T>();
    if id == TypeId::of::<SerializedMessage>() {
        (
            // SAFETY: SerializeFn<T> is the same as SerializeFn<SerializedMessage>
            unsafe {
                std::mem::transmute::<SerializeFn<SerializedMessage>, SerializeFn<T>>(
                    SERIALIZED_MESSAGE_SERIALIZE,
                )
            },
            // SAFETY: DeserializeFn<T> is the same as DeserializeFn<SerializedMessage>
            unsafe {
                std::mem::transmute::<DeserializeFn<SerializedMessage>, DeserializeFn<T>>(
                    DeserializeFn(Result::<SerializedMessage, mesh_protobuf::Error>::Ok),
                )
            },
        )
    } else if let Some((serialize, deserialize)) = entries(id) {
        (
            // SAFETY: a function pointer of the appropriate type was put into the map.
            unsafe { std::mem::transmute::<[usize; 2], SerializeFn<T>>(serialize) },
            // SAFETY: a function pointer of the appropriate type was put into the map.
            unsafe { std::mem::transmute::<usize, DeserializeFn<T>>(deserialize) },
        )
    } else {
        let serialize = SerializeFn(static_ref::StaticRef::new(&DynEncoder::<
            <T as MeshPayload>::Encoding,
        >(PhantomData)));
        let deserialize = DeserializeFn(SerializedMessage::into_message);
        set_entries(
            id,
            // SAFETY: converting a fat pointer to usize*2 has no safety requirements.
            unsafe { std::mem::transmute::<SerializeFn<T>, [usize; 2]>(serialize) },
            deserialize.0 as usize,
        );
        (serialize, deserialize)
    }
}

fn set_entries(id: TypeId, serialize: [usize; 2], deserialize: usize) {
    SERIALIZE
        .write()
        .get_or_insert_with(HashMap::new)
        .insert(id, (serialize, deserialize));
}

fn entries(id: TypeId) -> Option<([usize; 2], usize)> {
    Some(*SERIALIZE.read().as_ref()?.get(&id)?)
}

fn serialize_entry(id: TypeId) -> Option<[usize; 2]> {
    let (serialize, _) = entries(id)?;
    Some(serialize)
}

fn deserialize_entry(id: TypeId) -> Option<usize> {
    let (_, deserialize) = entries(id)?;
    Some(deserialize)
}

/// Gets the serializer for `T`, if [`ensure_serializable::<T>`] has been
/// called.
pub fn serializer<T: 'static>() -> Option<SerializeFn<T>> {
    let id = TypeId::of::<T>();
    if id == TypeId::of::<SerializedMessage>() {
        // SAFETY: SerializeFn<T> is the same as SerializeFn<SerializedMessage>.
        Some(unsafe {
            std::mem::transmute::<SerializeFn<SerializedMessage>, SerializeFn<T>>(
                SERIALIZED_MESSAGE_SERIALIZE,
            )
        })
    } else {
        let f = serialize_entry(id)?;
        // SAFETY: a function pointer of the appropriate type was put into the map.
        Some(unsafe { std::mem::transmute::<[usize; 2], SerializeFn<T>>(f) })
    }
}

/// Gets the deserializer for `T`, if [`ensure_serializable::<T>`] has been
/// called.
pub fn deserializer<T: 'static>() -> Option<DeserializeFn<T>> {
    let id = TypeId::of::<T>();
    let f = if id == TypeId::of::<SerializedMessage>() {
        DeserializeFn(Result::<SerializedMessage, mesh_protobuf::Error>::Ok).0 as usize
    } else {
        deserialize_entry(id)?
    };
    // SAFETY: a function pointer of the appropriate type was put into the map.
    Some(unsafe { std::mem::transmute::<usize, DeserializeFn<T>>(f) })
}

// A message that might be able to be serialized.
pub struct LazyMessage<T> {
    msg: T,
    serialize: Option<SerializeFn<T>>,
}

impl<T: 'static + Send> LazyMessage<T> {
    // Creates a new message wrapping `data`, which will be lazily serialized
    // when needed.
    //
    // If a serialize function is not provided, then one will be looked up at
    // serialize time. In this case, the caller is responsible for ensuring that
    // [`crate::lazy::ensure_serializable`] has been called before calling
    // [`Self::serialize`] on this result.
    pub fn new(data: T, serialize: Option<SerializeFn<T>>) -> Self {
        LazyMessage {
            msg: data,
            serialize,
        }
    }
}

impl<T: 'static + Send> SerializeMessage for LazyMessage<T> {
    type Concrete = T;

    fn compute_message_size(&mut self, sizer: MessageSizer<'_>) {
        let serialize = self
            .serialize
            .get_or_insert_with(|| serializer::<T>().expect("missing serialize for T"));

        serialize.0.compute_message_size(&mut self.msg, sizer);
    }

    fn write_message(self, writer: MessageWriter<'_, '_, Resource>) {
        self.serialize.unwrap().0.write_message(self.msg, writer);
    }

    fn extract(self) -> Self::Concrete {
        self.msg
    }
}

/// Parses the method, using the provided deserialize function to
/// deserialize it if necessary.
///
/// If no deserialize function is provided, then one will be looked up when
/// necessary. The caller is responsible for ensuring
/// [`crate::lazy::ensure_serializable`] has been called if the message
/// might be in serialized state.
pub fn lazy_parse<T: 'static + Send>(
    serialized: SerializedMessage,
    cache: &mut Option<DeserializeFn<T>>,
) -> Result<T, mesh_protobuf::Error> {
    let deserialize =
        *cache.get_or_insert_with(|| deserializer::<T>().expect("missing deserialize"));
    (deserialize.0)(serialized)
}

mod static_ref {
    use std::ops::Deref;

    /// This is equivalent to a `&'static T`, except that it does not require a
    /// lifetime bound on `T` just to declare it.
    #[repr(transparent)]
    pub struct StaticRef<T: ?Sized>(*const T);

    impl<T: ?Sized> StaticRef<T> {
        pub const fn new(x: &'static T) -> Self {
            Self(x)
        }
    }

    impl<T: ?Sized> From<&'static T> for StaticRef<T> {
        fn from(x: &'static T) -> Self {
            Self(x)
        }
    }

    impl<T: ?Sized> Deref for StaticRef<T> {
        type Target = T;

        fn deref(&self) -> &T {
            // SAFETY: the inner T is known to be a valid object with 'static
            // lifetime.
            unsafe { &*self.0 }
        }
    }

    impl<T: ?Sized> Copy for StaticRef<T> {}

    impl<T: ?Sized> Clone for StaticRef<T> {
        fn clone(&self) -> Self {
            *self
        }
    }

    /// SAFETY: &'static T is Send if T is Sync.
    unsafe impl<T: ?Sized + Sync> Send for StaticRef<T> {}

    /// SAFETY: &'static T is Sync if T is Sync.
    unsafe impl<T: ?Sized + Sync> Sync for StaticRef<T> {}
}
