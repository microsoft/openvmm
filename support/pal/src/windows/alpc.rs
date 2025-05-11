// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]

use super::IoCompletionPort;
use super::ObjectAttributes;
use super::chk_status;
use headervec::HeaderVec;
use ntlpcapi::*;
use std::cmp::min;
use std::ffi::c_void;
use std::io;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::ops::DerefMut;
use std::os::windows::prelude::*;
use std::ptr::NonNull;
use std::ptr::null_mut;
use std::time::Duration;
use winapi::shared::ntstatus::STATUS_TIMEOUT;

mod ntlpcapi {
    #![allow(non_snake_case, dead_code, clippy::upper_case_acronyms)]

    pub use ntapi::ntlpcapi::*;
    use winapi::shared::ntdef::HANDLE;
    use winapi::shared::ntdef::NTSTATUS;

    // These constants are not defined in ntapi.
    pub const LPC_CONNECTION_REPLY: u32 = 11;
    pub const LPC_CANCELED: u32 = 12;

    pub const LPC_CONTINUATION_REQUIRED: u32 = 0x2000;

    pub const ALPC_PORFLG_ACCEPT_REQUESTS: u32 = 0x00020000;
    pub const ALPC_PORFLG_ACCEPT_DUP_HANDLES: u32 = 0x00080000;
    pub const ALPC_PORFLG_ACCEPT_INDIRECT_HANDLES: u32 = 0x02000000;

    pub const ALPC_HANDLEFLG_INDIRECT: u32 = 0x00040000;

    pub const ALPC_VIEWFLG_UNMAP_EXISTING: u32 = 0x00010000;
    pub const ALPC_VIEWFLG_AUTO_RELEASE: u32 = 0x00020000;
    pub const ALPC_VIEWFLG_SECURED_ACCESS: u32 = 0x00040000;

    pub const OB_FILE_OBJECT_TYPE: u32 = 0x00000001;
    pub const OB_THREAD_OBJECT_TYPE: u32 = 0x00000004;
    pub const OB_SEMAPHORE_OBJECT_TYPE: u32 = 0x00000008;
    pub const OB_EVENT_OBJECT_TYPE: u32 = 0x00000010;
    pub const OB_PROCESS_OBJECT_TYPE: u32 = 0x00000020;
    pub const OB_MUTANT_OBJECT_TYPE: u32 = 0x00000040;
    pub const OB_SECTION_OBJECT_TYPE: u32 = 0x00000080;
    pub const OB_REG_KEY_OBJECT_TYPE: u32 = 0x00000100;
    pub const OB_TOKEN_OBJECT_TYPE: u32 = 0x00000200;
    pub const OB_COMPOSITION_OBJECT_TYPE: u32 = 0x00000400;
    pub const OB_JOB_OBJECT_TYPE: u32 = 0x00000800;
    pub const OB_ALL_OBJECT_TYPE_CODES: u32 = 0x00000ffd;

    // This is defined incorrectly in ntapi 0.3.6.
    unsafe extern "C" {
        pub fn AlpcInitializeMessageAttribute(
            AttributeFlags: u32,
            Buffer: PALPC_MESSAGE_ATTRIBUTES,
            BufferSize: usize,
            RequiredBufferSize: *mut usize,
        ) -> NTSTATUS;
    }

    // This is defined incorrectly in ntapi 0.3.6.
    #[repr(C)]
    pub struct ALPC_HANDLE_ATTR {
        pub Flags: u32,
        pub u2: ALPC_HANDLE_ATTR_u2,
        pub u3: ALPC_HANDLE_ATTR_u3,
        pub u4: ALPC_HANDLE_ATTR_u4,
    }

    impl Default for ALPC_HANDLE_ATTR {
        fn default() -> Self {
            // SAFETY: ALPC_HANDLE_ATTR has no safety invariants
            unsafe { std::mem::zeroed() }
        }
    }

    #[repr(C)]
    pub union ALPC_HANDLE_ATTR_u2 {
        pub Handle: HANDLE,
        pub HandleAttrArray: *mut ALPC_HANDLE_ATTR32,
    }

    #[repr(C)]
    pub union ALPC_HANDLE_ATTR_u3 {
        pub ObjectType: u32,
        pub HandleCount: u32,
    }

    #[repr(C)]
    pub union ALPC_HANDLE_ATTR_u4 {
        pub DesiredAccess: u32,
        pub GrantedAccess: u32,
    }

    #[repr(C)]
    pub struct ALPC_HANDLE_ATTR32 {
        pub Flags: u32,
        pub Handle: u32,
        pub ObjectType: u32,
        pub Access: u32,
    }
}

#[derive(Debug, Clone)]
pub struct PortConfig {
    waitable: bool,
    max_message_len: usize,
}

impl PortConfig {
    pub fn new() -> Self {
        PortConfig {
            waitable: false,
            max_message_len: 512,
        }
    }

    pub fn waitable(mut self, waitable: bool) -> Self {
        self.waitable = waitable;
        self
    }

    pub fn max_message_len(mut self, n: usize) -> Self {
        self.max_message_len = n;
        self
    }

    fn port_attributes(&self) -> ALPC_PORT_ATTRIBUTES {
        let mut attributes = ALPC_PORT_ATTRIBUTES {
            Flags: ALPC_PORFLG_ACCEPT_DUP_HANDLES
                | ALPC_PORFLG_ACCEPT_INDIRECT_HANDLES
                | ALPC_PORFLG_ACCEPT_REQUESTS,
            DupObjectTypes: OB_ALL_OBJECT_TYPE_CODES,
            MaxMessageLength: self.max_message_len + size_of::<PORT_MESSAGE>(),
            MaxPoolUsage: usize::MAX,
            MaxSectionSize: usize::MAX,
            MaxTotalSectionSize: usize::MAX,
            MaxViewSize: usize::MAX,
            ..Default::default()
        };
        if self.waitable {
            attributes.Flags |= ALPC_PORFLG_WAITABLE_PORT;
        }
        attributes
    }

    pub fn create(self, obj_attr: &ObjectAttributes<'_>) -> io::Result<Port> {
        let mut port = null_mut();
        let mut port_attr = self.port_attributes();

        // SAFETY: calling API and getting the handle result according to the NT API
        let port = unsafe {
            chk_status(NtAlpcCreatePort(
                &mut port,
                obj_attr.as_ptr(),
                &mut port_attr,
            ))?;
            OwnedHandle::from_raw_handle(port)
        };
        Ok(Port(port))
    }

    pub fn connect(self, obj_attr: &ObjectAttributes<'_>, data: &[u8]) -> io::Result<Port> {
        let mut port = null_mut();
        let mut port_attr = self.port_attributes();

        let len: i16 = data.len().try_into().expect("message too large");
        let mut message = HeaderVec::<PORT_MESSAGE, u8, 32>::new(Default::default());
        message.head.u1.s.DataLength = len;
        message.head.u1.s.TotalLength = len
            .checked_add(size_of::<PORT_MESSAGE>() as i16)
            .expect("message too large");
        message.extend_tail_from_slice(data);

        // SAFETY: calling API and getting the handle result according to the NT API
        let port = unsafe {
            chk_status(NtAlpcConnectPortEx(
                &mut port,
                obj_attr.as_ptr(),
                null_mut(),
                &mut port_attr,
                0, // flags
                null_mut(),
                if data.is_empty() {
                    null_mut()
                } else {
                    message.as_mut_ptr()
                },
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            ))?;
            OwnedHandle::from_raw_handle(port)
        };
        Ok(Port(port))
    }
}

#[derive(Debug)]
pub struct Port(OwnedHandle);

#[derive(Debug)]
pub struct Message {
    pub ty: MessageType,
    pub len: usize,
    pub pid: u32,
    pub port_context: usize,
    pub handles: Vec<OwnedHandle>,
}

#[derive(Debug)]
struct MessageKey {
    message_id: u32,
    callback_id: u32,
}

#[derive(Debug)]
pub enum MessageType {
    Datagram,
    ConnectionRequest,
    PortClosed,
    ConnectionReply,
    Canceled,
    Request,
    Reply,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct PortMessageHeader(PORT_MESSAGE);

// SAFETY: PORT_MESSAGE does not have any thread-related ownership semantics but
// contains an internal pointer value so is not automatically Send/Sync.
unsafe impl Send for PortMessageHeader {}
// SAFETY: as above
unsafe impl Sync for PortMessageHeader {}

// NT requires pointer alignment for these.
#[repr(C, align(8))]
#[derive(Copy, Clone)]
struct Attributes<const FLAGS: u32, const N: usize> {
    header: ALPC_MESSAGE_ATTRIBUTES,
    data: MaybeUninit<[u8; N]>,
}

impl<const FLAGS: u32, const N: usize> Default for Attributes<FLAGS, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const FLAGS: u32, const N: usize> Attributes<FLAGS, N> {
    fn new() -> Self {
        let mut this = Self {
            header: Default::default(),
            data: MaybeUninit::uninit(),
        };
        // SAFETY: AlpcInitializeMessageAttribute initializes the buffer header.
        // If the capacity of the array is too small, then this will panic.
        unsafe {
            let mut attributes_size = 0;
            chk_status(AlpcInitializeMessageAttribute(
                FLAGS,
                &mut this.header,
                size_of_val(&this),
                &mut attributes_size,
            ))
            .unwrap();
        }
        this
    }

    fn clear(&mut self) {
        self.header.ValidAttributes = 0;
    }

    // SAFETY: caller must ensure the attribute type `n` matches type `T`.
    unsafe fn attribute<T>(&self, n: u32) -> Option<&T> {
        if (self.header.ValidAttributes & n) != 0 {
            // SAFETY: AlpcGetMessageAttribute returns an internal reference into
            // the attribute object. It relies on the data being properly
            // initialized.
            unsafe {
                Some(
                    AlpcGetMessageAttribute(std::ptr::from_ref(&self.header).cast_mut(), n)
                        .cast::<T>()
                        .as_ref()
                        .unwrap(),
                )
            }
        } else {
            None
        }
    }

    /// # Safety
    ///
    /// Caller must ensure the attribute type `n` matches type `T`.
    unsafe fn set_attribute<T>(&mut self, n: u32, val: T) {
        // SAFETY: guaranteed by caller.
        let ptr = unsafe { AlpcGetMessageAttribute(&mut self.header, n).cast::<T>() };
        let ptr = NonNull::new(ptr).unwrap().as_ptr();

        // SAFETY: guaranteed by caller.
        unsafe { ptr.write(val) };
        self.header.ValidAttributes |= n;
    }

    fn context(&self) -> Option<&ALPC_CONTEXT_ATTR> {
        // SAFETY: ALPC_MESSAGE_CONTEXT_ATTRIBUTE matches ALPC_CONTEXT_ATTR
        unsafe { self.attribute(ALPC_MESSAGE_CONTEXT_ATTRIBUTE) }
    }

    fn handle(&self) -> Option<&ALPC_HANDLE_ATTR> {
        // SAFETY: ALPC_MESSAGE_HANDLE_ATTRIBUTE matches ALPC_HANDLE_ATTR
        unsafe { self.attribute(ALPC_MESSAGE_HANDLE_ATTRIBUTE) }
    }

    fn set_handle(&mut self, val: ALPC_HANDLE_ATTR) {
        // SAFETY: ALPC_MESSAGE_HANDLE_ATTRIBUTE matches ALPC_HANDLE_ATTR
        unsafe { self.set_attribute(ALPC_MESSAGE_HANDLE_ATTRIBUTE, val) }
    }

    fn view(&self) -> Option<&ALPC_DATA_VIEW_ATTR> {
        // SAFETY: ALPC_MESSAGE_VIEW_ATTRIBUTE matches ALPC_DATA_VIEW_ATTR
        unsafe { self.attribute(ALPC_MESSAGE_VIEW_ATTRIBUTE) }
    }

    fn set_view(&mut self, val: ALPC_DATA_VIEW_ATTR) {
        // SAFETY: ALPC_MESSAGE_VIEW_ATTRIBUTE matches ALPC_DATA_VIEW_ATTR
        unsafe { self.set_attribute(ALPC_MESSAGE_VIEW_ATTRIBUTE, val) }
    }

    fn as_mut_ptr(&mut self) -> *mut ALPC_MESSAGE_ATTRIBUTES {
        &mut self.header
    }
}

const RECV_ATTR_SIZE: usize = (size_of::<ALPC_CONTEXT_ATTR>()
    + size_of::<ALPC_HANDLE_ATTR>()
    + size_of::<ALPC_DATA_VIEW_ATTR>())
.next_power_of_two();

/// A buffer for receiving messages into.
pub struct RecvMessageBuffer {
    buf: HeaderVec<PortMessageHeader, u8, 0>,
    attributes: Attributes<
        {
            ALPC_MESSAGE_CONTEXT_ATTRIBUTE
                | ALPC_MESSAGE_HANDLE_ATTRIBUTE
                | ALPC_MESSAGE_VIEW_ATTRIBUTE
        },
        RECV_ATTR_SIZE,
    >,
}

impl RecvMessageBuffer {
    pub fn new(len: usize) -> Self {
        Self {
            buf: HeaderVec::with_capacity(Default::default(), std::cmp::max(len, 32)),
            attributes: Attributes::new(),
        }
    }

    fn internal_type(&self) -> u32 {
        // SAFETY: all PORT_MESSAGE union fields are initialized
        unsafe { self.buf.head.0.u2.s.Type as u32 & 0xff }
    }
}

/// A received message.
pub struct RecvMessage<'a> {
    port: &'a Port,
    message: &'a mut RecvMessageBuffer,
    drop_view: bool,
}

impl RecvMessage<'_> {
    pub fn message_type(&self) -> MessageType {
        match self.message.internal_type() {
            0 => panic!("uninitialized message"),
            LPC_DATAGRAM => MessageType::Datagram,
            LPC_CONNECTION_REQUEST => MessageType::ConnectionRequest,
            LPC_PORT_CLOSED => MessageType::PortClosed,
            LPC_CONNECTION_REPLY => MessageType::ConnectionReply,
            LPC_CANCELED => MessageType::Canceled,
            LPC_REQUEST => MessageType::Request,
            LPC_REPLY => MessageType::Reply,
            n => panic!("invalid message type {}", n),
        }
    }

    /// If true, this message needs a reply to release kernel resources.
    pub fn needs_reply(&self) -> bool {
        // SAFETY: all PORT_MESSAGE union fields are initialized
        unsafe { self.message.buf.head.0.u2.s.Type as u32 & LPC_CONTINUATION_REQUIRED != 0 }
    }

    fn reply_key(&self) -> Option<MessageKey> {
        self.needs_reply().then_some(MessageKey {
            message_id: self.message.buf.head.0.MessageId,
            callback_id: {
                // SAFETY: all PORT_MESSAGE union fields are initialized
                unsafe { self.message.buf.head.0.u4.CallbackId }
            },
        })
    }

    pub fn data(&self) -> &[u8] {
        &self.message.buf.tail
    }

    pub fn context(&self) -> usize {
        self.message.attributes.context().unwrap().PortContext as usize
    }

    pub fn pid(&self) -> u32 {
        // SAFETY: all PORT_MESSAGE union fields are initialized
        unsafe { self.message.buf.head.0.u3.ClientId.UniqueProcess as usize as u32 }
    }

    pub fn handles(&self, port: &Port, handles: &mut Vec<OwnedHandle>) -> io::Result<()> {
        if let Some(handle_attr) = self.message.attributes.handle() {
            // SAFETY: all PORT_MESSAGE union fields are initialized
            let handle_count = unsafe { handle_attr.u3.HandleCount };

            assert!(handle_count > 0);
            handles.reserve(handle_count as usize);
            for i in 0..handle_count {
                let mut info = ALPC_MESSAGE_HANDLE_INFORMATION {
                    Index: i,
                    ..Default::default()
                };
                // SAFETY: NtAlpcQueryInformationMessage fills in the buffer
                // with the a valid handle for the specified index.
                let handle = unsafe {
                    chk_status(NtAlpcQueryInformationMessage(
                        port.0.as_raw_handle(),
                        self.message.buf.as_ptr() as *mut _,
                        AlpcMessageHandleInformation,
                        std::ptr::from_mut(&mut info).cast(),
                        size_of_val(&info) as u32,
                        null_mut(),
                    ))?;
                    OwnedHandle::from_raw_handle(info.Handle as usize as RawHandle)
                };
                handles.push(handle);
            }
        }

        Ok(())
    }

    /// Returns access to the shared memory view.
    pub fn view(&mut self) -> Option<PortSectionView<'_>> {
        let attr = self.message.attributes.view()?;
        let view = PortSectionView {
            port: self.port,
            attr: *attr,
            unmap: false,
        };
        Some(view)
    }

    /// Returns access to the secure shared memory view.
    ///
    /// A secure view is one that is guaranteed by the kernel to be immutable.
    /// This allows it to be accessed as `&[u8]`.
    pub fn secure_view(&mut self) -> Option<ReadablePortSectionView<'_>> {
        let view = self.view()?;
        if view.attr.Flags & ALPC_VIEWFLG_SECURED_ACCESS == 0 {
            return None;
        }
        Some(ReadablePortSectionView(view))
    }
}

impl Drop for RecvMessage<'_> {
    fn drop(&mut self) {
        let drop_view = self.drop_view;
        if let Some(mut view) = self.view() {
            view.unmap = drop_view;
        }
    }
}

pub struct SendMessage {
    buf: HeaderVec<PortMessageHeader, u8, 0>,
}

impl SendMessage {
    pub fn new() -> Self {
        Self {
            buf: HeaderVec::new(Default::default()),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: HeaderVec::with_capacity(Default::default(), cap),
        }
    }

    pub fn extend(&mut self, data: &[u8]) {
        self.buf.extend_tail_from_slice(data);
    }

    /// Returns the current message length.
    pub fn len(&self) -> usize {
        self.buf.tail.len()
    }

    /// Returns the remaining spare capacity of the message as a slice of
    /// `MaybeUninit<U::Element>`.
    ///
    /// The returned slice can be used to fill the message with data before
    /// marking the data as initialized using [`Self::set_len].
    pub fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        self.buf.spare_tail_capacity_mut()
    }

    /// Updates the initialized byte length of the message.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `len` message bytes have been initialized.
    pub unsafe fn set_len(&mut self, len: usize) {
        // SAFETY: guaranteed by caller.
        unsafe {
            self.buf.set_tail_len(len);
        }
    }
}

impl From<&'_ [u8]> for SendMessage {
    fn from(data: &'_ [u8]) -> Self {
        let mut msg = Self::with_capacity(data.len());
        msg.extend(data);
        msg
    }
}

#[must_use]
pub struct SendOperation<'a> {
    port: &'a Port,
    message: &'a mut SendMessage,
    handles: Vec<ALPC_HANDLE_ATTR32>,
    view: Option<PortSectionView<'a>>,
    secure_view: bool,
}

impl<'a> SendOperation<'a> {
    pub fn add_handle(&mut self, handle: BorrowedHandle<'a>) -> &mut Self {
        self.handles.push(ALPC_HANDLE_ATTR32 {
            Flags: ALPC_HANDLEFLG_DUPLICATE_SAME_ACCESS,
            Handle: handle.as_raw_handle() as usize as u32,
            ObjectType: 0,
            Access: 0,
        });
        self
    }

    pub fn add_handles<I, T>(&mut self, iter: I) -> &mut Self
    where
        I: Iterator<Item = &'a T>,
        T: 'a + AsHandle,
    {
        for h in iter {
            self.add_handle(h.as_handle());
        }
        self
    }

    /// Sets the view to send with the message.
    ///
    /// If true, try to secure the view so that the receiver is guaranteed
    /// immutable access. This only works if the underlying section was created
    /// with [`PortSection::new_secure`].
    pub fn set_view(&mut self, view: PortSectionView<'a>, secure: bool) -> &mut Self {
        assert_eq!(std::ptr::from_ref(self.port), std::ptr::from_ref(view.port));
        self.view = Some(view);
        self.secure_view = secure;
        self
    }

    /// Sends a message that does not require a response.
    pub fn send(self) -> io::Result<()> {
        self.do_send(true, None)
    }

    /// Sends a request message that expects a response.
    pub fn request(self) -> io::Result<()> {
        self.do_send(false, None)
    }

    /// Sends a reply to a message.
    pub fn reply(self, request: RecvMessage<'_>) -> io::Result<()> {
        self.do_send(true, Some(request))
    }

    fn do_send(mut self, release: bool, request: Option<RecvMessage<'_>>) -> io::Result<()> {
        let len: i16 = self
            .message
            .buf
            .tail
            .len()
            .try_into()
            .expect("message too large");
        self.message.buf.head = Default::default();
        self.message.buf.head.0.u1.s.DataLength = len;
        self.message.buf.head.0.u1.s.TotalLength = len
            .checked_add(size_of::<PORT_MESSAGE>() as i16)
            .expect("message too large");

        const ATTR_SIZE: usize = size_of::<ALPC_HANDLE_ATTR>() + size_of::<ALPC_DATA_VIEW_ATTR>();
        let mut attributes: Option<
            Attributes<{ ALPC_MESSAGE_HANDLE_ATTRIBUTE | ALPC_MESSAGE_VIEW_ATTRIBUTE }, ATTR_SIZE>,
        > = None;
        if !self.handles.is_empty() {
            let mut handle_attr;
            if self.handles.len() == 1 {
                handle_attr = ALPC_HANDLE_ATTR {
                    Flags: ALPC_HANDLEFLG_DUPLICATE_SAME_ACCESS,
                    ..Default::default()
                };
                handle_attr.u2.Handle = self.handles[0].Handle as usize as *mut _;
            } else {
                handle_attr = ALPC_HANDLE_ATTR {
                    Flags: ALPC_HANDLEFLG_INDIRECT,
                    ..Default::default()
                };
                handle_attr.u2.HandleAttrArray = self.handles.as_mut_ptr();
                handle_attr.u3.HandleCount = self.handles.len().try_into().unwrap();
            }

            attributes
                .get_or_insert_with(Default::default)
                .set_handle(handle_attr);
        }

        let mut unmap_old_view = false;
        if let Some(mut request) = request {
            let key = request.reply_key().unwrap();
            self.message.buf.head.0.MessageId = key.message_id;
            self.message.buf.head.0.u4.CallbackId = key.callback_id;
            unmap_old_view = request.drop_view;
            request.drop_view = false;
        }

        if let Some(mut view) = self.view {
            let mut attr = view.attr;
            attr.Flags |= ALPC_VIEWFLG_AUTO_RELEASE;
            if self.secure_view {
                attr.Flags |= ALPC_VIEWFLG_SECURED_ACCESS;
            }
            if unmap_old_view {
                attr.Flags |= ALPC_VIEWFLG_UNMAP_EXISTING;
            }
            attributes
                .get_or_insert_with(Default::default)
                .set_view(attr);
            view.unmap = false;
        } else if unmap_old_view {
            attributes
                .get_or_insert_with(Default::default)
                .set_view(ALPC_DATA_VIEW_ATTR {
                    Flags: ALPC_VIEWFLG_UNMAP_EXISTING,
                    SectionHandle: null_mut(),
                    ViewBase: null_mut(),
                    ViewSize: 0,
                });
        }

        // SAFETY: calling NT API according to contract
        unsafe {
            chk_status(NtAlpcSendWaitReceivePort(
                self.port.0.as_raw_handle(),
                if release {
                    ALPC_MSGFLG_RELEASE_MESSAGE
                } else {
                    0
                },
                &mut self.message.buf.head.0,
                attributes
                    .as_mut()
                    .map(Attributes::as_mut_ptr)
                    .unwrap_or(null_mut()),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            ))?;
            Ok(())
        }
    }
}

impl Port {
    fn accept_reject(
        &self,
        config: Option<PortConfig>,
        request: RecvMessage<'_>,
        port_context: usize,
        message: &mut SendMessage,
    ) -> io::Result<Option<Port>> {
        let len: i16 = message
            .buf
            .tail
            .len()
            .try_into()
            .expect("message too large");
        message.buf.head = Default::default();
        message.buf.head.0.u1.s.DataLength = len;
        message.buf.head.0.u1.s.TotalLength = len
            .checked_add(size_of::<PORT_MESSAGE>() as i16)
            .expect("message too large");
        let key = request.reply_key().unwrap();
        message.buf.head.0.MessageId = key.message_id;
        message.buf.head.0.u4.CallbackId = key.callback_id;

        let accept = config.is_some();
        let mut port_attr = config.map(|c| c.port_attributes());

        // SAFETY: calling NT API according to contract
        unsafe {
            let mut port = null_mut();
            chk_status(NtAlpcAcceptConnectPort(
                &mut port,
                self.0.as_raw_handle(),
                0,
                null_mut(),
                port_attr
                    .as_mut()
                    .map(std::ptr::from_mut)
                    .unwrap_or(null_mut()),
                port_context as *mut c_void,
                &mut message.buf.head.0,
                null_mut(),
                accept.into(),
            ))?;
            let port = if accept {
                Some(Port(OwnedHandle::from_raw_handle(port)))
            } else {
                None
            };
            Ok(port)
        }
    }

    pub fn accept(
        &self,
        config: PortConfig,
        request: RecvMessage<'_>,
        port_context: usize,
        message: &mut SendMessage,
    ) -> io::Result<Port> {
        self.accept_reject(Some(config), request, port_context, message)
            .map(Option::unwrap)
    }

    pub fn reject(&self, request: RecvMessage<'_>) -> io::Result<()> {
        self.accept_reject(None, request, 0, &mut SendMessage::new())
            .map(drop)
    }

    pub fn recv<'a>(&'a self, message: &'a mut RecvMessageBuffer) -> io::Result<RecvMessage<'a>> {
        Ok(self.do_recv(message, None)?.unwrap())
    }

    pub fn try_recv<'a>(
        &'a self,
        message: &'a mut RecvMessageBuffer,
    ) -> io::Result<Option<RecvMessage<'a>>> {
        self.do_recv(message, Some(Duration::from_secs(0)))
    }

    fn do_recv<'a>(
        &'a self,
        message: &'a mut RecvMessageBuffer,
        timeout: Option<Duration>,
    ) -> io::Result<Option<RecvMessage<'a>>> {
        let mut message_len = message.buf.total_byte_capacity();
        message.buf.head = Default::default();
        message.buf.clear_tail();
        message.attributes.clear();

        let mut timeout_100ns = timeout
            .map(|d| min(d.as_nanos() / 100, i64::MAX as u128) as i64)
            .unwrap_or(0);

        // SAFETY: calling API according to contract
        unsafe {
            if chk_status(NtAlpcSendWaitReceivePort(
                self.0.as_raw_handle(),
                0,
                null_mut(),
                null_mut(),
                &mut message.buf.head.0,
                &mut message_len,
                message.attributes.as_mut_ptr(),
                if timeout.is_some() {
                    std::ptr::from_mut::<i64>(&mut timeout_100ns).cast()
                } else {
                    null_mut()
                },
            ))? == STATUS_TIMEOUT
            {
                return Ok(None);
            }
        }

        // SAFETY: DataLength is set to the initialized byte length of the
        // buffer, so it is safe to mark those bytes as initialized.
        unsafe {
            let len = if message.internal_type() == LPC_PORT_CLOSED {
                // The buffer is the message ID. Don't return that.
                0
            } else {
                message.buf.head.0.u1.s.DataLength as usize
            };
            message.buf.set_tail_len(len);
        }

        Ok(Some(RecvMessage {
            port: self,
            drop_view: message.attributes.view().is_some(),
            message,
        }))
    }

    pub fn start_send<'a>(&'a self, message: &'a mut SendMessage) -> SendOperation<'a> {
        SendOperation {
            port: self,
            message,
            handles: Vec::new(),
            view: None,
            secure_view: false,
        }
    }

    pub fn send(&self, data: &[u8]) -> io::Result<()> {
        self.start_send(&mut data.into()).send()
    }

    pub fn request(&self, data: &[u8]) -> io::Result<()> {
        self.start_send(&mut data.into()).request()
    }

    pub fn reply(&self, request: RecvMessage<'_>, data: &[u8]) -> io::Result<()> {
        self.start_send(&mut data.into()).reply(request)
    }

    pub fn associate_iocp(&self, iocp: &IoCompletionPort, key: usize) -> io::Result<()> {
        let mut info = ALPC_PORT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: key as *mut c_void,
            CompletionPort: iocp.as_handle().as_raw_handle(),
        };
        // SAFETY: calling the API according to the contract.
        unsafe {
            chk_status(NtAlpcSetInformation(
                self.0.as_raw_handle(),
                AlpcAssociateCompletionPortInformation,
                std::ptr::from_mut::<ALPC_PORT_ASSOCIATE_COMPLETION_PORT>(&mut info)
                    .cast::<c_void>(),
                size_of_val(&info) as u32,
            ))?;
            Ok(())
        }
    }
}

impl AsHandle for Port {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

/// A section object associated with a port.
pub struct PortSection<'a> {
    port: &'a Port,
    handle: usize,
}

impl<'a> PortSection<'a> {
    /// Creates a new pagefile-backed section that can be used to create secure
    /// views.
    pub fn new_secure(port: &'a Port, len: usize) -> io::Result<Self> {
        let mut handle = null_mut();
        // SAFETY: calling as documented internally, no safety requirements.
        unsafe {
            let mut actual_len = 0;
            chk_status(NtAlpcCreatePortSection(
                port.0.as_raw_handle(),
                ALPC_VIEWFLG_SECURED_ACCESS,
                null_mut(),
                len,
                &mut handle,
                &mut actual_len,
            ))?;
        }
        Ok(Self {
            port,
            handle: handle as usize,
        })
    }

    /// Allocates a new writable view from the section heap.
    ///
    /// The caller has exclusive access to this view until it is sent to the
    /// other endpoint.
    pub fn alloc_view(&self, len: usize) -> io::Result<WritablePortSectionView<'a>> {
        // SAFETY: calling as documented internally, no safety requirements.
        unsafe {
            let mut attr = ALPC_DATA_VIEW_ATTR {
                SectionHandle: self.handle as *mut _,
                ViewSize: len,
                ..std::mem::zeroed()
            };
            chk_status(NtAlpcCreateSectionView(
                self.port.0.as_raw_handle(),
                0,
                &mut attr,
            ))?;
            Ok(WritablePortSectionView(PortSectionView {
                port: self.port,
                attr,
                unmap: true,
            }))
        }
    }
}

impl Drop for PortSection<'_> {
    fn drop(&mut self) {
        // SAFETY: the port and handle are known to be valid.
        unsafe {
            chk_status(NtAlpcDeletePortSection(
                self.port.0.as_raw_handle(),
                0,
                self.handle as *mut _,
            ))
            .unwrap();
        }
    }
}

/// A view to which the owner has exclusive write access.
pub struct WritablePortSectionView<'a>(PortSectionView<'a>);

impl<'a> WritablePortSectionView<'a> {
    /// Extracts the inner view, dropping guarantees of exclusive access.
    pub fn into_inner(self) -> PortSectionView<'a> {
        self.0
    }
}

impl Deref for WritablePortSectionView<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: this object has exclusive access to the view memory, which is
        // valid for read.
        unsafe { std::slice::from_raw_parts(self.0.attr.ViewBase.cast(), self.0.attr.ViewSize) }
    }
}

impl DerefMut for WritablePortSectionView<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: this object has exclusive access to the view memory, which is
        // valid for write.
        unsafe { std::slice::from_raw_parts_mut(self.0.attr.ViewBase.cast(), self.0.attr.ViewSize) }
    }
}

/// A view that is guaranteed to be immutable.
pub struct ReadablePortSectionView<'a>(PortSectionView<'a>);

impl<'a> ReadablePortSectionView<'a> {
    /// Extracts the inner view, dropping guarantees of immutable access.
    pub fn into_inner(self) -> PortSectionView<'a> {
        self.0
    }
}

impl Deref for ReadablePortSectionView<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: the view memory is immutable and valid for read.
        unsafe { std::slice::from_raw_parts(self.0.attr.ViewBase.cast(), self.0.attr.ViewSize) }
    }
}

/// A mapped view.
pub struct PortSectionView<'a> {
    port: &'a Port,
    attr: ALPC_DATA_VIEW_ATTR,
    unmap: bool,
}

impl PortSectionView<'_> {
    /// The length of the mapped view in bytes.
    pub fn len(&self) -> usize {
        self.attr.ViewSize
    }
}

impl Drop for PortSectionView<'_> {
    fn drop(&mut self) {
        if self.unmap {
            // SAFETY: The view is no longer in use and is known to be valid.
            unsafe {
                chk_status(NtAlpcDeleteSectionView(
                    self.port.0.as_raw_handle(),
                    0,
                    self.attr.ViewBase,
                ))
                .unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::UnicodeString;

    fn new_server(config: PortConfig) -> (Port, UnicodeString) {
        let mut id = [0; 16];
        getrandom::fill(&mut id).unwrap();
        let path: UnicodeString = format!("\\BaseNamedObjects\\{:x}", u128::from_ne_bytes(id))
            .try_into()
            .unwrap();
        let server = config.create(ObjectAttributes::new().name(&path)).unwrap();
        (server, path)
    }

    fn connected_pair(config: PortConfig) -> (Port, Port, Port) {
        let (server, path) = new_server(config.clone());
        let client = config
            .clone()
            .connect(ObjectAttributes::new().name(&path), &[])
            .unwrap();

        let comm;
        let mut msg = RecvMessageBuffer::new(0);
        {
            let m = server.recv(&mut msg).unwrap();
            assert!(m.data().is_empty());

            assert!(matches!(m.message_type(), MessageType::ConnectionRequest));
            assert_eq!(m.pid(), std::process::id());

            comm = server
                .accept(config, m, 0, &mut SendMessage::new())
                .unwrap();
        }
        {
            let m = client.recv(&mut msg).unwrap();
            assert!(m.data().is_empty());
            assert!(matches!(m.message_type(), MessageType::ConnectionReply));
        }
        (server, comm, client)
    }

    #[test]
    fn test_connect() {
        connected_pair(PortConfig::new());
    }

    #[test]
    fn test_reject() {
        let (server, path) = new_server(PortConfig::new());
        let client = PortConfig::new()
            .connect(ObjectAttributes::new().name(&path), &[])
            .unwrap();
        let mut m = RecvMessageBuffer::new(0);
        {
            let m = server.recv(&mut m).unwrap();
            assert!(matches!(m.message_type(), MessageType::ConnectionRequest));
            server.reject(m).unwrap();
        }
        assert_eq!(
            client.recv(&mut m).map(drop).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn test_send_recv() {
        let (server, _comm, client) = connected_pair(PortConfig::new());
        client.start_send(&mut b"abc"[..].into()).send().unwrap();
        let mut msg = RecvMessageBuffer::new(3);
        let m = server.recv(&mut msg).unwrap();
        assert!(matches!(m.message_type(), MessageType::Datagram));
        assert!(!m.needs_reply());
        assert_eq!(m.data(), b"abc");
    }

    #[test]
    fn test_send_lots() {
        let (server, _comm, client) = connected_pair(PortConfig::new());
        for i in 0u32..10000 {
            client.send(&i.to_ne_bytes()).unwrap();
        }
        let mut msg = RecvMessageBuffer::new(4);
        for i in 0u32..10000 {
            let m = server.recv(&mut msg).unwrap();
            assert!(matches!(m.message_type(), MessageType::Datagram));
            assert_eq!(m.data(), i.to_ne_bytes());
        }
    }

    #[test]
    fn test_port_context() {
        let (server, path) = new_server(PortConfig::new());
        let context = 0xbadf00d;
        let client = PortConfig::new()
            .connect(ObjectAttributes::new().name(&path), &[])
            .unwrap();
        let mut msg = RecvMessageBuffer::new(0);
        let m = server.recv(&mut msg).unwrap();
        assert!(matches!(m.message_type(), MessageType::ConnectionRequest));
        assert_eq!(m.pid(), std::process::id());
        let _comm = server
            .accept(PortConfig::new(), m, context, &mut SendMessage::new())
            .unwrap();
        client.send(&[]).unwrap();
        let m = server.recv(&mut msg).unwrap();
        assert!(matches!(m.message_type(), MessageType::Datagram));
        assert_eq!(m.context(), context);
    }

    #[test]
    fn test_disconnect() {
        let (server, _comm, client) = connected_pair(PortConfig::new());
        drop(client);
        let mut m = RecvMessageBuffer::new(0);
        let m = server.recv(&mut m).unwrap();
        assert!(matches!(m.message_type(), MessageType::PortClosed));
        assert_eq!(m.data().len(), 0);
    }

    #[test]
    fn test_handle_passing() {
        let (server, _comm, client) = connected_pair(PortConfig::new());
        let event = pal_event::Event::new();
        let mut message = SendMessage::new();
        let mut op = client.start_send(&mut message);
        op.add_handle(event.as_handle());
        op.send().unwrap();
        let mut m = RecvMessageBuffer::new(0);
        let m = server.recv(&mut m).unwrap();
        assert!(matches!(m.message_type(), MessageType::Datagram));
        m.handles(&server, &mut Vec::new()).unwrap();
        let mut handles = Vec::new();
        m.handles(&server, &mut handles).unwrap();
        server.reply(m, &[]).unwrap();
        assert_eq!(handles.len(), 1);
        let event2 = pal_event::Event::from(handles.into_iter().next().unwrap());
        event2.signal();
        event.wait();
    }

    #[test]
    fn test_iocp() {
        let iocp = IoCompletionPort::new();
        let (server, path) = new_server(PortConfig::new());
        server.associate_iocp(&iocp, 0x1234).unwrap();
        let _client = PortConfig::new()
            .connect(ObjectAttributes::new().name(&path), &[])
            .unwrap();
        let mut entries = [Default::default(); 2];
        assert_eq!(iocp.get(&mut entries, None), 1);
        assert_eq!(entries[0].lpCompletionKey, 0x1234);
    }

    #[test]
    fn test_send_then_close() {
        let (server, _comm, client) = connected_pair(PortConfig::new());
        client.send(b"abc").unwrap();
        drop(client);
        let mut m = RecvMessageBuffer::new(3);
        // ALPC helpfully throws away the data but still sends notification that
        // a message had been sent.
        {
            let m = server.recv(&mut m).unwrap();
            assert!(matches!(m.message_type(), MessageType::Canceled));
            assert_eq!(m.data(), &[]);
        }
        let m = server.recv(&mut m).unwrap();
        assert!(matches!(m.message_type(), MessageType::PortClosed));
    }

    #[test]
    fn test_request_reply() {
        let (server, comm, client) = connected_pair(PortConfig::new().waitable(true));
        comm.request(b"abc").unwrap();
        let mut m = RecvMessageBuffer::new(4);
        {
            let m = client.recv(&mut m).unwrap();
            assert_eq!(m.data(), b"abc");
            assert!(matches!(m.message_type(), MessageType::Request));
            client.reply(m, b"def").unwrap();
        }
        let m = server.recv(&mut m).unwrap();
        assert!(matches!(m.message_type(), MessageType::Reply));
        assert_eq!(m.data(), b"def");
    }

    #[test]
    fn test_view() {
        let (_server, comm, client) = connected_pair(PortConfig::new().waitable(true));
        {
            let section = PortSection::new_secure(&comm, 4096).unwrap();
            let mut view = section.alloc_view(4096).unwrap();
            view.fill(0xcd);
            let mut message = SendMessage::new();
            let mut send = comm.start_send(&mut message);
            send.set_view(view.into_inner(), true);
            send.request().unwrap();
        }

        {
            let mut m = RecvMessageBuffer::new(4);
            let mut m = client.recv(&mut m).unwrap();
            {
                let view = m.secure_view().unwrap();
                assert!(view.iter().eq(&vec![0xcd; 4096]));
            }
            let mut message = SendMessage::new();
            let send = client.start_send(&mut message);
            send.reply(m).unwrap();
        }
    }
}
