// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Top-level VMBus connection operations backed by `ClientCore`.

use crate::Error;
use crate::Result;
use crate::client_driver::ClientDriver;
use crate::client_driver::MessagePump;
use crate::client_driver::driver;
use crate::interrupt::SimpPump;
use crate::protocol::VMBUS_CONNECTION_ID_LEGACY;
use crate::protocol::VMBUS_CONNECTION_ID_MODERN;
use crate::synic::VMBUS_SINT;
use crate::synic::synic_pages;
use alloc::vec::Vec;
use guid::Guid;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
use spin::Mutex;
use vmbus_client_core::CompletionResult;
use vmbus_client_core::ConnectParams;
use vmbus_client_core::Event;
use vmbus_client_core::MonitorPageGpas;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::OfferChannel;
use vmbus_core::protocol::Version;

/// Top-level connection state established after successful negotiation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionState {
    /// Version selected by the host.
    pub selected_version: Version,
    /// Connection ID to use for `HvCallPostMessage`.
    pub post_message_connection_id: u32,
    /// Feature flags supported by both sides.
    pub feature_flags: FeatureFlags,
    /// Server-provided parent-to-child monitor page GPA.
    pub parent_to_child_monitor_page_gpa: u64,
    /// Server-provided child-to-parent monitor page GPA.
    pub child_to_parent_monitor_page_gpa: u64,
}

/// Return the negotiated connection state, if connected.
pub fn connection() -> Option<ConnectionState> {
    connection_from_driver(&driver())
}

fn connection_from_driver(driver: &ClientDriver) -> Option<ConnectionState> {
    let version = driver.version()?;
    let (parent_to_child, child_to_parent) = driver.monitor_pages();
    Some(ConnectionState {
        selected_version: version.version,
        post_message_connection_id: driver.post_message_connection_id(),
        feature_flags: version.feature_flags,
        parent_to_child_monitor_page_gpa: parent_to_child,
        child_to_parent_monitor_page_gpa: child_to_parent,
    })
}

/// The initial connection ID used for `InitiateContact`.
pub fn initial_connection_id(version: Version) -> u32 {
    if version < Version::Win10Rs3_1 {
        VMBUS_CONNECTION_ID_LEGACY
    } else {
        VMBUS_CONNECTION_ID_MODERN
    }
}

/// Process-wide monitor-page GPAs advertised during negotiation.
pub static MONITOR_PAGES: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// A stable guest client ID (`"opentmk-invariant"`).
pub const CLIENT_ID: Guid = Guid {
    data1: 0x6f_70_74_6d,
    data2: 0x6b_5f,
    data3: 0x69_6e,
    data4: *b"variant\0",
};

/// Negotiate a VMBus version using an explicit driver and message pump.
pub fn initiate_with<C, P>(
    ctx: &mut C,
    driver: &mut ClientDriver,
    pump: &mut P,
    client_id: Guid,
) -> Result<ConnectionState>
where
    C: HypercallPlatformTrait<Config = HyperVHypercallConfig>,
    P: MessagePump,
{
    let request_id = driver.request_id();
    let (parent_to_child, child_to_parent) = *MONITOR_PAGES.lock();
    driver.step(
        ctx,
        Event::Connect {
            request_id,
            params: ConnectParams {
                target_message_vp: 0,
                monitor_page: Some(MonitorPageGpas {
                    parent_to_child,
                    child_to_parent,
                }),
                client_id,
            },
        },
    )?;
    match driver.wait_for(ctx, pump, request_id)? {
        CompletionResult::Connect(Ok(_)) => {
            connection_from_driver(driver).ok_or(Error::VersionMismatch)
        }
        CompletionResult::Connect(Err(_)) => Err(Error::VersionMismatch),
        _ => Err(Error::UnexpectedCompletion),
    }
}

/// Request and collect all current channel offers.
pub fn request_offers_with<C, P>(
    ctx: &mut C,
    driver: &mut ClientDriver,
    pump: &mut P,
) -> Result<Vec<OfferChannel>>
where
    C: HypercallPlatformTrait<Config = HyperVHypercallConfig>,
    P: MessagePump,
{
    driver.take_offers();
    let request_id = driver.request_id();
    driver.step(ctx, Event::RequestOffers { request_id })?;
    match driver.wait_for(ctx, pump, request_id)? {
        CompletionResult::RequestOffers(Ok(())) => Ok(driver.take_offers()),
        CompletionResult::RequestOffers(Err(_)) => Err(Error::VersionMismatch),
        _ => Err(Error::UnexpectedCompletion),
    }
}

/// Disconnect using an explicit driver and message pump.
pub fn unload_with<C, P>(ctx: &mut C, driver: &mut ClientDriver, pump: &mut P) -> Result<()>
where
    C: HypercallPlatformTrait<Config = HyperVHypercallConfig>,
    P: MessagePump,
{
    let request_id = driver.request_id();
    driver.step(ctx, Event::Unload { request_id })?;
    match driver.wait_for(ctx, pump, request_id)? {
        CompletionResult::Unload => Ok(()),
        _ => Err(Error::UnexpectedCompletion),
    }
}

/// Negotiate using the process-wide driver and SIMP pump.
pub fn initiate<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
) -> Result<()> {
    let pages = synic_pages().ok_or(Error::VersionMismatch)?;
    let mut pump = SimpPump::new(pages.simp_gpa);
    initiate_with(ctx, &mut driver(), &mut pump, CLIENT_ID)?;
    Ok(())
}

/// Enumerate offers using the process-wide driver and SIMP pump.
pub fn request_offers<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
) -> Result<Vec<OfferChannel>> {
    let pages = synic_pages().ok_or(Error::VersionMismatch)?;
    request_offers_with(ctx, &mut driver(), &mut SimpPump::new(pages.simp_gpa))
}

/// Disconnect the process-wide driver.
pub fn unload<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
) -> Result<()> {
    let pages = synic_pages().ok_or(Error::VersionMismatch)?;
    unload_with(ctx, &mut driver(), &mut SimpPump::new(pages.simp_gpa))
}

/// Standard channel-manager SINT.
pub const SINT: u8 = VMBUS_SINT;
